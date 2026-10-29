//! The webhook receiver: GitHub deliveries in, refresh hints out.
//!
//! A delivery is a hint, never a fact. The receiver verifies its signature,
//! answers it, and turns the few events that can change what the queue sees
//! into a [`Hint`] on a channel. Nothing a delivery says is believed: a
//! consumer must read GitHub for the entity a hint names.
//!
//! [`Listener::spawn`] binds the address and starts one thread that accepts
//! connections and serves each to completion before accepting the next:
//! read the request head (parsed by `httparse`), read the body, verify,
//! answer, close. Every response carries `Connection: close`; there is no
//! keep-alive and no thread pool.
//!
//! **Per connection.** Every read and write runs under one deadline, ten
//! seconds from `accept` (GitHub's own delivery deadline); past it the
//! connection is dropped with no response. Before a byte of the body is
//! read the head must fit in 8 KiB and 64 headers (`431`), the method must
//! be `POST` (`405`), there must be no `Expect` (`417`), no
//! `Transfer-Encoding` and exactly one `Content-Length` (`411`), a decimal
//! length (`400`) of at most 25 MiB (`413`). Those refusals are answered at
//! once and the connection closed; nothing drains the body. The body is
//! then read to exactly its declared length. A request missing
//! `X-GitHub-Event` or `X-GitHub-Delivery` is `400`; one whose
//! `X-Hub-Signature-256` is missing or does not verify as HMAC-SHA256 of
//! the body under the secret is `401`. A verified delivery is answered
//! `204`, then parsed into its hint.
//!
//! **Stderr.** `401`, `411` and `413` each write one line naming the status
//! and the delivery id when the request carried one: a wrong secret, a
//! proxy that forwards chunked bodies, a proxy without a body limit. Every
//! other refusal and every silent close writes nothing. A verified delivery
//! writes exactly one line, after its response: its id, event and action
//! with the hint it became or why it became none, or, when its body is not
//! the JSON its event should carry, the id and the event.
//!
//! **Hints.** Only the events and conditions below become hints, and only
//! for a delivery whose `repository.full_name` is the queue's repository
//! (compared ignoring ASCII case): an app installed on several
//! repositories, or an organisation webhook, sends one receiver every
//! repository's deliveries. Every other delivery, `ping` included, is
//! answered and dropped.
//!
//! | event | condition | hint |
//! |---|---|---|
//! | `pull_request` | the pull request carries the label, or `action` is `unlabeled` with `label.name` the label | [`Hint::Pr`] |
//! | `pull_request_review` | the pull request carries the label | [`Hint::Pr`] |
//! | `check_run` | `action` is `completed` and the run's name is a required or batch check | [`Hint::Commit`] of `head_sha` |
//! | `check_suite` | `action` is `completed` | [`Hint::Commit`] of `head_sha` |
//! | `status` | `state` is not `pending` and `context` is a required or batch check | [`Hint::Commit`] of `sha` |
//! | `push` | `ref` is the trunk branch | [`Hint::Trunk`] |
//!
//! An `accept` error concerns one pending connection or a passing shortage
//! of descriptors: the thread writes one line, pauses 100 ms and accepts
//! again. The thread ends only by panicking, or when the hint receiver is
//! gone; either way the [`Sender`] drops with it, and the forge sees its
//! channel disconnect.

#[cfg(test)]
mod tests;

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{SendError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use dorfq_core::{PrNumber, Sha};
use ring::hmac;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::Settings;
use crate::refresh::Entity;

/// How long one connection may take, from `accept` to its response:
/// GitHub's own delivery deadline.
const DEADLINE: Duration = Duration::from_secs(10);
/// The most bytes a request head may take.
const HEAD_LIMIT: usize = 8 * 1024;
/// The most headers a request may carry.
const MAX_HEADERS: usize = 64;
/// The largest body accepted: GitHub's ceiling for a delivery.
const BODY_LIMIT: u64 = 25 * 1024 * 1024;
/// The pause after an `accept` error.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

/// Something on GitHub may have changed; the forge decides what to re-read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hint {
    /// A pull request that carries, or just lost, the queue's label.
    Pr(PrNumber),
    /// A commit whose required checks reported a result.
    Commit(Sha),
    /// The trunk branch was pushed.
    Trunk,
}

impl fmt::Display for Hint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Hint::Pr(number) => fmt::Display::fmt(&Entity::Pr(*number), f),
            Hint::Commit(sha) => write!(f, "commit {sha}"),
            Hint::Trunk => f.write_str("trunk"),
        }
    }
}

/// Why a verified delivery became no hint: one variant per condition of
/// the hint table that fails, each carrying the names its reason quotes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Dropped {
    /// The delivery's repository, and the queue's.
    OtherRepository { found: String, queue: String },
    /// The pull request does not carry the label.
    Unlabelled { label: String },
    /// A check run (`suite: false`) or suite that is not completed.
    NotCompleted { suite: bool },
    /// A check run named neither a required nor a batch check.
    NotRequired { check: String },
    /// A status whose context is neither a required nor a batch check.
    NotRequiredContext { context: String },
    /// A pending status.
    Pending,
    /// A push to a branch other than trunk.
    OtherRef { reference: String, trunk: String },
    /// An event the table does not name.
    Unwatched,
}

impl fmt::Display for Dropped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Dropped::OtherRepository { found, queue } => {
                write!(f, "repository {found:?} is not the queue's {queue:?}")
            }
            Dropped::Unlabelled { label } => {
                write!(f, "the pull request does not carry the label {label:?}")
            }
            Dropped::NotCompleted { suite: false } => f.write_str("the run is not completed"),
            Dropped::NotCompleted { suite: true } => f.write_str("the suite is not completed"),
            Dropped::NotRequired { check } => {
                write!(f, "check {check:?} is neither a required nor a batch check")
            }
            Dropped::NotRequiredContext { context } => {
                write!(
                    f,
                    "context {context:?} is neither a required nor a batch check"
                )
            }
            Dropped::Pending => f.write_str("the status is pending"),
            Dropped::OtherRef { reference, trunk } => {
                let trunk = format!("refs/heads/{trunk}");
                write!(f, "ref {reference:?} is not trunk ({trunk:?})")
            }
            Dropped::Unwatched => f.write_str("the queue does not watch this event"),
        }
    }
}

/// A running receiver.
#[derive(Debug)]
pub struct Listener {
    address: SocketAddr,
}

impl Listener {
    /// Binds `address` and starts the receiver's thread, which sends every
    /// hint on `hints`. `repository` (`owner/repo`) and `settings`' trunk,
    /// label and check names are what hints are filtered by; `secret` is
    /// the webhook secret. A bind failure is an error naming the address.
    pub fn spawn(
        address: SocketAddr,
        secret: String,
        repository: String,
        settings: Settings,
        hints: Sender<Hint>,
    ) -> io::Result<Listener> {
        let filter = Filter {
            repository,
            settings,
        };
        Listener::spawn_with_deadline(address, secret, filter, hints, DEADLINE)
    }

    /// [`Listener::spawn`] with a connection deadline other than ten seconds.
    fn spawn_with_deadline(
        address: SocketAddr,
        secret: String,
        filter: Filter,
        hints: Sender<Hint>,
        deadline: Duration,
    ) -> io::Result<Listener> {
        let socket = TcpListener::bind(address).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot listen for webhooks on {address}: {error}"),
            )
        })?;
        let address = socket.local_addr()?;
        thread::Builder::new()
            .name("webhook".to_owned())
            .spawn(move || serve(&socket, secret.as_bytes(), &filter, &hints, deadline))?;
        Ok(Listener { address })
    }

    /// The address the receiver is bound to.
    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

/// What a verified delivery must name to become a hint.
struct Filter {
    /// The queue's repository, `owner/repo`.
    repository: String,
    settings: Settings,
}

/// Accepts and serves connections one at a time, until the hint receiver
/// is gone.
fn serve(
    socket: &TcpListener,
    secret: &[u8],
    filter: &Filter,
    hints: &Sender<Hint>,
    deadline: Duration,
) {
    loop {
        match socket.accept() {
            Ok((stream, _)) => {
                if handle(stream, secret, filter, hints, deadline).is_err() {
                    return;
                }
            }
            Err(error) => {
                eprintln!("dorfq: webhook listener: accept failed: {error}");
                thread::sleep(ACCEPT_PAUSE);
            }
        }
    }
}

/// Serves one connection to completion; fails only when the hint receiver
/// is gone.
fn handle(
    stream: TcpStream,
    secret: &[u8],
    filter: &Filter,
    hints: &Sender<Hint>,
    deadline: Duration,
) -> Result<(), SendError<Hint>> {
    let mut connection = Connection {
        stream,
        deadline: Instant::now() + deadline,
    };
    let delivery = match receive(&mut connection, secret) {
        Ok(delivery) => delivery,
        Err(Abort::Close) => return Ok(()),
        Err(Abort::Reject { status, delivery }) => {
            if let Some(line) = refusal_line(status, delivery.as_deref()) {
                eprintln!("{line}");
            }
            connection.respond(status);
            return Ok(());
        }
    };
    connection.respond(Status::NoContent);
    drop(connection);
    let (line, hint) = outcome(&delivery, filter);
    eprintln!("{line}");
    hint.map_or(Ok(()), |hint| hints.send(hint))
}

/// What a verified delivery comes to under `filter`: the stderr line it
/// writes, and the hint it sends, if any.
fn outcome(delivery: &Delivery, filter: &Filter) -> (String, Option<Hint>) {
    let line = |outcome| delivery_line(delivery, action(&delivery.body).as_deref(), outcome);
    match hint_of(&delivery.event, &delivery.body, filter) {
        Ok(hint) => (line(Ok(&hint)), Some(hint)),
        Err(NoHint::Dropped(dropped)) => (line(Err(&dropped)), None),
        Err(NoHint::Unreadable(error)) => (unreadable_line(delivery, &error), None),
    }
}

/// The stderr line for a verified delivery whose body `hint_of` read,
/// `action` being the body's top-level `action`, when it has one.
fn delivery_line(
    delivery: &Delivery,
    action: Option<&str>,
    outcome: Result<&Hint, &Dropped>,
) -> String {
    let action = action.map_or_else(String::new, |action| format!(" {action:?}"));
    let outcome = match outcome {
        Ok(hint) => format!("hint {hint}"),
        Err(reason) => format!("no hint: {reason}"),
    };
    format!(
        "dorfq: webhook delivery {:?}: {:?}{action}: {outcome}",
        delivery.id, delivery.event
    )
}

/// The top-level `action` of a delivery's body, when it is a string.
fn action(body: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Action {
        action: Option<String>,
    }
    serde_json::from_slice::<Action>(body).ok()?.action
}

/// The stderr line for a refusal with `status` of a request carrying the
/// delivery id `delivery`; `None` for a status an operator cannot act on.
fn refusal_line(status: Status, delivery: Option<&str>) -> Option<String> {
    let reason = status.logged()?;
    let line = status.line();
    Some(match delivery {
        Some(id) => format!("dorfq: webhook delivery {id:?}: answered {line}: {reason}"),
        None => format!("dorfq: webhook request answered {line}: {reason}"),
    })
}

/// The stderr line for a verified delivery whose body `hint_of` could not
/// read.
fn unreadable_line(delivery: &Delivery, error: &str) -> String {
    format!(
        "dorfq: webhook delivery {:?}: unreadable {:?} event: {error}",
        delivery.id, delivery.event
    )
}

/// A request whose signature verified.
struct Delivery {
    id: String,
    event: String,
    body: Vec<u8>,
}

/// Why a connection ends without a delivery.
enum Abort {
    /// The request is answered with `status`; `delivery` is its delivery id,
    /// when it carried one.
    Reject {
        status: Status,
        delivery: Option<String>,
    },
    /// The connection is dropped with no response: the deadline passed, the
    /// client went away, or an I/O error ended it.
    Close,
}

impl From<io::Error> for Abort {
    fn from(_: io::Error) -> Abort {
        Abort::Close
    }
}

/// The statuses the receiver answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    NoContent,
    BadRequest,
    Unauthorized,
    MethodNotAllowed,
    LengthRequired,
    PayloadTooLarge,
    ExpectationFailed,
    HeaderFieldsTooLarge,
}

impl Status {
    /// The status code and reason phrase.
    fn line(self) -> &'static str {
        match self {
            Status::NoContent => "204 No Content",
            Status::BadRequest => "400 Bad Request",
            Status::Unauthorized => "401 Unauthorized",
            Status::MethodNotAllowed => "405 Method Not Allowed",
            Status::LengthRequired => "411 Length Required",
            Status::PayloadTooLarge => "413 Content Too Large",
            Status::ExpectationFailed => "417 Expectation Failed",
            Status::HeaderFieldsTooLarge => "431 Request Header Fields Too Large",
        }
    }

    /// What an operator is told when this status is answered, for the
    /// statuses an operator can act on.
    fn logged(self) -> Option<&'static str> {
        match self {
            Status::Unauthorized => Some("the signature is missing or does not verify"),
            Status::LengthRequired => Some(
                "the request has no Content-Length; a proxy in front must forward the body with its length",
            ),
            Status::PayloadTooLarge => Some("the declared body is over 25 MiB"),
            _ => None,
        }
    }
}

/// A connection under its deadline.
struct Connection {
    stream: TcpStream,
    deadline: Instant,
}

impl Connection {
    /// The time left before the deadline; none left is a timeout.
    fn remaining(&self) -> io::Result<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        Ok(left)
    }

    /// One read, timed out at the deadline.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.remaining()?;
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(buf)
    }

    /// Writes the response for `status`, each write timed out at the
    /// deadline. A failure ends the connection, which ends after this
    /// anyway, so it is not reported.
    fn respond(&mut self, status: Status) {
        let response = format!(
            "HTTP/1.1 {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            status.line()
        );
        let mut unsent = response.as_bytes();
        while !unsent.is_empty() {
            let Ok(left) = self.remaining() else { return };
            if self.stream.set_write_timeout(Some(left)).is_err() {
                return;
            }
            match self.stream.write(unsent) {
                Ok(0) | Err(_) => return,
                Ok(written) => unsent = &unsent[written..],
            }
        }
    }
}

/// A parsed request head.
struct Head {
    method: String,
    /// Header names in lowercase, with their raw values, in order.
    headers: Vec<(String, Vec<u8>)>,
    /// The bytes read after the head: the start of the body.
    rest: Vec<u8>,
}

impl Head {
    /// Every value of the header `name` (lowercase).
    fn values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a [u8]> {
        self.headers
            .iter()
            .filter(move |(header, _)| header == name)
            .map(|(_, value)| value.as_slice())
    }

    /// The first value of the header `name` (lowercase), when it is text.
    fn text<'a>(&'a self, name: &'a str) -> Option<&'a str> {
        self.values(name)
            .next()
            .and_then(|value| std::str::from_utf8(value).ok())
    }
}

/// Reads a request through its body and verifies its signature.
fn receive(connection: &mut Connection, secret: &[u8]) -> Result<Delivery, Abort> {
    let head = read_head(connection)?;
    let id = head.text("x-github-delivery").map(str::to_owned);
    let reject = |status| Abort::Reject {
        status,
        delivery: id.clone(),
    };
    if head.method != "POST" {
        return Err(reject(Status::MethodNotAllowed));
    }
    if head.values("expect").next().is_some() {
        return Err(reject(Status::ExpectationFailed));
    }
    if head.values("transfer-encoding").next().is_some() {
        return Err(reject(Status::LengthRequired));
    }
    let lengths: Vec<&[u8]> = head.values("content-length").collect();
    let length = match lengths.as_slice() {
        [] => return Err(reject(Status::LengthRequired)),
        [length] => decimal(length).ok_or_else(|| reject(Status::BadRequest))?,
        _ => return Err(reject(Status::BadRequest)),
    };
    if length > BODY_LIMIT {
        return Err(reject(Status::PayloadTooLarge));
    }
    let length = usize::try_from(length).map_err(|_| reject(Status::PayloadTooLarge))?;
    let body = read_body(connection, &head.rest, length)?;
    let (Some(id), Some(event)) = (id.clone(), head.text("x-github-event")) else {
        return Err(reject(Status::BadRequest));
    };
    let event = event.to_owned();
    if !verify(secret, &body, head.text("x-hub-signature-256")) {
        return Err(reject(Status::Unauthorized));
    }
    Ok(Delivery { id, event, body })
}

/// Reads until the request head parses, within [`HEAD_LIMIT`] bytes and
/// [`MAX_HEADERS`] headers.
fn read_head(connection: &mut Connection) -> Result<Head, Abort> {
    let reject = |status| Abort::Reject {
        status,
        delivery: None,
    };
    let mut buf = vec![0; HEAD_LIMIT];
    let mut filled = 0;
    loop {
        let read = connection.read(&mut buf[filled..])?;
        if read == 0 {
            return Err(Abort::Close);
        }
        filled += read;
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut request = httparse::Request::new(&mut headers);
        match request.parse(&buf[..filled]) {
            Ok(httparse::Status::Complete(end)) => {
                let Some(method) = request.method else {
                    return Err(reject(Status::BadRequest));
                };
                return Ok(Head {
                    method: method.to_owned(),
                    headers: request
                        .headers
                        .iter()
                        .map(|header| (header.name.to_ascii_lowercase(), header.value.to_vec()))
                        .collect(),
                    rest: buf[end..filled].to_vec(),
                });
            }
            Ok(httparse::Status::Partial) if filled == HEAD_LIMIT => {
                return Err(reject(Status::HeaderFieldsTooLarge));
            }
            Ok(httparse::Status::Partial) => {}
            Err(httparse::Error::TooManyHeaders) => {
                return Err(reject(Status::HeaderFieldsTooLarge));
            }
            Err(_) => return Err(reject(Status::BadRequest)),
        }
    }
}

/// Reads the body to exactly `length` bytes, `rest` being its start. A
/// client that ends the stream first closes the connection.
fn read_body(connection: &mut Connection, rest: &[u8], length: usize) -> Result<Vec<u8>, Abort> {
    let mut body = Vec::with_capacity(length);
    body.extend_from_slice(&rest[..rest.len().min(length)]);
    let mut chunk = [0; 16 * 1024];
    while body.len() < length {
        let want = (length - body.len()).min(chunk.len());
        let read = connection.read(&mut chunk[..want])?;
        if read == 0 {
            return Err(Abort::Close);
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(body)
}

/// A non-empty run of ASCII digits, as a `u64`.
fn decimal(text: &[u8]) -> Option<u64> {
    if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(text).ok()?.parse().ok()
}

/// Whether `signature`, an `X-Hub-Signature-256` value, is `sha256=` and
/// the hex HMAC-SHA256 of `body` under `secret`. The comparison is constant
/// time.
fn verify(secret: &[u8], body: &[u8], signature: Option<&str>) -> bool {
    let Some(tag) = signature
        .and_then(|signature| signature.strip_prefix("sha256="))
        .and_then(hex)
    else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    hmac::verify(&key, body, &tag).is_ok()
}

/// Hex digits, two to a byte.
fn hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digit = |byte: u8| char::from(byte).to_digit(16);
            u8::try_from(digit(pair[0])? * 16 + digit(pair[1])?).ok()
        })
        .collect()
}

/// Why a verified delivery became no hint.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NoHint {
    Dropped(Dropped),
    /// The body of an event the table names is not the JSON that event
    /// carries; the error describes it.
    Unreadable(String),
}

impl From<Dropped> for NoHint {
    fn from(dropped: Dropped) -> NoHint {
        NoHint::Dropped(dropped)
    }
}

impl From<String> for NoHint {
    fn from(error: String) -> NoHint {
        NoHint::Unreadable(error)
    }
}

/// What a verified delivery of `event` becomes: its hint, or why none,
/// testing the table's conditions in order.
fn hint_of(event: &str, body: &[u8], filter: &Filter) -> Result<Hint, NoHint> {
    let settings = &filter.settings;
    let labelled = |labels: &[Label]| labels.iter().any(|label| label.name == settings.label);
    let unlabelled = || Dropped::Unlabelled {
        label: settings.label.clone(),
    };
    let required = |name: &str| {
        settings
            .checks
            .iter()
            .chain(&settings.batch_checks)
            .any(|check| check == name)
    };
    let verdict = match event {
        "pull_request" => {
            let event: PullRequestEvent = parse(body, &filter.repository)?;
            let unlabeled = event.action == "unlabeled"
                && event
                    .label
                    .is_some_and(|label| label.name == settings.label);
            if unlabeled || labelled(&event.pull_request.labels) {
                Ok(Hint::Pr(PrNumber(event.pull_request.number)))
            } else {
                Err(unlabelled())
            }
        }
        "pull_request_review" => {
            let event: ReviewEvent = parse(body, &filter.repository)?;
            if labelled(&event.pull_request.labels) {
                Ok(Hint::Pr(PrNumber(event.pull_request.number)))
            } else {
                Err(unlabelled())
            }
        }
        "check_run" => {
            let event: CheckRunEvent = parse(body, &filter.repository)?;
            if event.action != "completed" {
                Err(Dropped::NotCompleted { suite: false })
            } else if !required(&event.check_run.name) {
                Err(Dropped::NotRequired {
                    check: event.check_run.name,
                })
            } else {
                Ok(commit(&event.check_run.head_sha)?)
            }
        }
        "check_suite" => {
            let event: CheckSuiteEvent = parse(body, &filter.repository)?;
            if event.action == "completed" {
                Ok(commit(&event.check_suite.head_sha)?)
            } else {
                Err(Dropped::NotCompleted { suite: true })
            }
        }
        "status" => {
            let event: StatusEvent = parse(body, &filter.repository)?;
            if event.state == "pending" {
                Err(Dropped::Pending)
            } else if !required(&event.context) {
                Err(Dropped::NotRequiredContext {
                    context: event.context,
                })
            } else {
                Ok(commit(&event.sha)?)
            }
        }
        "push" => {
            let event: PushEvent = parse(body, &filter.repository)?;
            if event.reference == format!("refs/heads/{}", settings.trunk) {
                Ok(Hint::Trunk)
            } else {
                Err(Dropped::OtherRef {
                    reference: event.reference,
                    trunk: settings.trunk.clone(),
                })
            }
        }
        _ => Err(Dropped::Unwatched),
    };
    Ok(verdict?)
}

/// The event `body` carries when its repository is `repository`; dropped
/// for another repository's.
fn parse<T: DeserializeOwned>(body: &[u8], repository: &str) -> Result<T, NoHint> {
    let delivery: Repository<T> =
        serde_json::from_slice(body).map_err(|error| error.to_string())?;
    let found = delivery.repository.full_name;
    if !found.eq_ignore_ascii_case(repository) {
        return Err(Dropped::OtherRepository {
            found,
            queue: repository.to_owned(),
        }
        .into());
    }
    Ok(delivery.event)
}

fn commit(sha: &str) -> Result<Hint, String> {
    Sha::parse(sha)
        .map(Hint::Commit)
        .map_err(|error| error.to_string())
}

/// An event with the repository it concerns.
#[derive(Deserialize)]
struct Repository<T> {
    repository: FullName,
    #[serde(flatten)]
    event: T,
}

#[derive(Deserialize)]
struct FullName {
    full_name: String,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

#[derive(Deserialize)]
struct PullRequest {
    number: u64,
    labels: Vec<Label>,
}

#[derive(Deserialize)]
struct PullRequestEvent {
    action: String,
    label: Option<Label>,
    pull_request: PullRequest,
}

#[derive(Deserialize)]
struct ReviewEvent {
    pull_request: PullRequest,
}

#[derive(Deserialize)]
struct CheckRun {
    name: String,
    head_sha: String,
}

#[derive(Deserialize)]
struct CheckRunEvent {
    action: String,
    check_run: CheckRun,
}

#[derive(Deserialize)]
struct CheckSuite {
    head_sha: String,
}

#[derive(Deserialize)]
struct CheckSuiteEvent {
    action: String,
    check_suite: CheckSuite,
}

#[derive(Deserialize)]
struct StatusEvent {
    sha: String,
    state: String,
    context: String,
}

#[derive(Deserialize)]
struct PushEvent {
    #[serde(rename = "ref")]
    reference: String,
}
