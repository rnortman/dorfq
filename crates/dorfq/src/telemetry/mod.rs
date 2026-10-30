//! Telemetry: the queue's gauges, derived from the status document, and its
//! events, one log record each, as OTLP request bodies.
//!
//! A gauge's time is the host time of the export that carries it; a log
//! record's is its event's forge time, its observed time the host time of
//! the tick that produced it. The window figures are the status document's:
//! what ended in the trailing [`WINDOW`] seconds of forge time.
//!
//! The [`Exporter`] pushes them from a thread of its own: the run loop hands
//! it one [`Tick`] per tick and never waits on the network.

pub mod otlp;

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;
use ureq::http::HeaderValue;

use crate::config::TelemetryConfig;
use crate::journal::events::{self, Event, Kind};
use crate::journal::status::{Document, Figures, WINDOW};
use crate::journal::summary::HOUR;
use crate::providers::ForgeGauges;
use crate::unix_now;
use otlp::{Attr, Data, Encoding, LogRecord, Metric, Number, Point, Resource};

/// The instrumentation scope's name.
pub const SCOPE: &str = "dorfq";

/// The queued pull requests' status classes, in [`Snapshot::queue_prs`]'s
/// order.
const STATUSES: [&str; 3] = ["ready", "waiting", "conflicting"];

/// The tracked batches' state classes, in [`Snapshot::batches`]' order.
const STATES: [&str; 6] = [
    "testing", "passed", "failed", "landed", "refused", "conflict",
];

/// The gauges one tick gives, derived from the document on the main
/// thread; what the exporter re-exports at each interval until the next.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// `ready`, `waiting`, `conflicting`, in that order.
    pub queue_prs: [u64; 3],
    /// `testing`, `passed`, `failed`, `landed`, `refused`, `conflict`.
    pub batches: [u64; 6],
    /// Forge time since the earliest arrival in the pool; 0 when it is
    /// empty.
    pub oldest_queued_seconds: u64,
    pub figures: Figures,
    pub forge: Option<ForgeGauges>,
}

/// The gauges of `document`, whose forge clock reads `forge_now`.
pub fn snapshot(document: &Document, forge_now: u64, forge: Option<ForgeGauges>) -> Snapshot {
    let mut queue_prs = [0; 3];
    for pr in &document.prs {
        queue_prs[class(&STATUSES, &pr.status)] += 1;
    }
    let mut batches = [0; 6];
    for batch in &document.batches {
        batches[class(&STATES, &batch.state)] += 1;
    }
    let oldest_queued_seconds = document
        .prs
        .iter()
        .map(|pr| pr.arrived)
        .min()
        .map_or(0, |arrived| forge_now.saturating_sub(arrived));
    Snapshot {
        queue_prs,
        batches,
        oldest_queued_seconds,
        figures: document.figures.clone(),
        forge,
    }
}

/// The index in `classes` of the text before `text`'s first `:`. The
/// document's wordings are this crate's own, `driver::status` and
/// `status::state`, whose every variant the tests run through here; an
/// unknown class is a bug.
fn class(classes: &[&str], text: &str) -> usize {
    let head = text.split_once(':').map_or(text, |(head, _)| head);
    classes
        .iter()
        .position(|c| *c == head)
        .unwrap_or_else(|| panic!("the status document worded {text:?}, which is no class"))
}

/// The exporter's own counts, cumulative since `started_at` (host time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    pub started_at: u64,
    /// Requests the collector did not take.
    pub export_failures: u64,
    /// Events dropped from a full buffer before delivery.
    pub events_dropped: u64,
}

/// The resource every request describes: this process.
pub fn resource() -> Resource {
    Resource {
        attributes: vec![
            ("service.name".to_owned(), Attr::Str("dorfq".to_owned())),
            (
                "service.version".to_owned(),
                Attr::Str(crate::version().to_owned()),
            ),
        ],
    }
}

/// Every metric of one export at host time `at`: `snapshot`'s gauges, the
/// age of the last observing tick when there was one, and `counters`.
pub fn metrics(
    snapshot: &Snapshot,
    tick_age: Option<u64>,
    counters: &Counters,
    at: u64,
) -> Vec<Metric> {
    let time = nanos(at);
    let point = |attributes: Vec<(String, Attr)>, value: Number| Point {
        attributes,
        time_unix_nano: time,
        value,
    };
    let window = || ("window_seconds".to_owned(), int_attr(WINDOW));
    let figures = &snapshot.figures;
    let windowed = |name: &str, description: &str, unit: &str, count: usize| {
        gauge(
            name,
            description,
            unit,
            vec![point(vec![window()], count_value(count))],
        )
    };
    let quantiles = |p50: Option<f64>, p95: Option<f64>| {
        [("0.5", p50), ("0.95", p95)]
            .into_iter()
            .filter_map(|(quantile, hours)| {
                hours.map(|hours| {
                    point(
                        vec![
                            ("quantile".to_owned(), Attr::Str(quantile.to_owned())),
                            window(),
                        ],
                        Number::Double(hours * HOUR),
                    )
                })
            })
            .collect()
    };

    let mut metrics = vec![
        gauge(
            "dorfq_queue_prs",
            "Pull requests in the queue's pool, by status.",
            "{pull_request}",
            STATUSES
                .iter()
                .zip(snapshot.queue_prs)
                .map(|(status, n)| point(vec![str_attr("status", status)], int(n)))
                .collect(),
        ),
        gauge(
            "dorfq_batches",
            "Batches the queue tracks, by state.",
            "{batch}",
            STATES
                .iter()
                .zip(snapshot.batches)
                .map(|(state, n)| point(vec![str_attr("state", state)], int(n)))
                .collect(),
        ),
        gauge(
            "dorfq_oldest_queued_seconds",
            "Forge time since the earliest arrival still in the pool; 0 when it is empty.",
            "s",
            vec![point(Vec::new(), int(snapshot.oldest_queued_seconds))],
        ),
        windowed(
            "dorfq_window_landed_prs",
            "Pull requests the queue landed in the window.",
            "{pull_request}",
            figures.landed_prs,
        ),
        windowed(
            "dorfq_window_landed_batches",
            "Batches the queue landed in the window.",
            "{batch}",
            figures.landed_batches,
        ),
        windowed(
            "dorfq_window_ejected_prs",
            "Pull requests the queue ejected in the window.",
            "{pull_request}",
            figures.ejected,
        ),
        windowed(
            "dorfq_window_refused_landings",
            "Landings the forge refused in the window.",
            "{batch}",
            figures.refused,
        ),
        gauge(
            "dorfq_window_runs",
            "Batch CI runs that ended in the window, by verdict.",
            "{run}",
            [
                ("passed", figures.passed_runs),
                ("failed", figures.failed_runs),
                ("cancelled", figures.cancelled_runs),
            ]
            .into_iter()
            .map(|(verdict, n)| point(vec![str_attr("verdict", verdict), window()], count_value(n)))
            .collect(),
        ),
        gauge(
            "dorfq_window_batch_run_seconds",
            "Duration of the batch CI runs that ended in the window, by quantile.",
            "s",
            quantiles(figures.batch_run_p50_hours, figures.batch_run_p95_hours),
        ),
        gauge(
            "dorfq_window_land_seconds",
            "Time from arrival to landing of the pull requests landed in the window, by quantile.",
            "s",
            quantiles(figures.land_p50_hours, figures.land_p95_hours),
        ),
    ];
    if let Some(age) = tick_age {
        metrics.push(gauge(
            "dorfq_tick_age_seconds",
            "Host time since the last tick that observed the repository.",
            "s",
            vec![point(Vec::new(), int(age))],
        ));
    }
    if let Some(forge) = snapshot.forge {
        metrics.push(gauge(
            "dorfq_queue_blocked_prs",
            "Pull requests labelled for the queue but not enqueued at the latest observation.",
            "{pull_request}",
            vec![point(Vec::new(), int(forge.blocked_prs))],
        ));
        if let Some(remaining) = forge.rate_limit_remaining {
            metrics.push(gauge(
                "dorfq_api_rate_limit_remaining",
                "Requests left in the forge's API rate limit, as last reported.",
                "{request}",
                vec![point(Vec::new(), int(remaining))],
            ));
        }
    }
    let start = nanos(counters.started_at);
    metrics.push(sum(
        "dorfq_telemetry_export_failures_total",
        "Telemetry requests the collector did not take.",
        "{request}",
        start,
        point(Vec::new(), int(counters.export_failures)),
    ));
    metrics.push(sum(
        "dorfq_telemetry_events_dropped_total",
        "Events dropped from the telemetry buffer before delivery.",
        "{event}",
        start,
        point(Vec::new(), int(counters.events_dropped)),
    ));
    metrics
}

fn gauge(name: &str, description: &str, unit: &str, points: Vec<Point>) -> Metric {
    Metric {
        name: name.to_owned(),
        description: description.to_owned(),
        unit: unit.to_owned(),
        data: Data::Gauge(points),
    }
}

fn sum(name: &str, description: &str, unit: &str, start: u64, point: Point) -> Metric {
    Metric {
        name: name.to_owned(),
        description: description.to_owned(),
        unit: unit.to_owned(),
        data: Data::Sum {
            start_unix_nano: start,
            points: vec![point],
        },
    }
}

fn str_attr(key: &str, value: &str) -> (String, Attr) {
    (key.to_owned(), Attr::Str(value.to_owned()))
}

fn int_attr(value: u64) -> Attr {
    Attr::Int(i64::try_from(value).unwrap_or(i64::MAX))
}

/// `n` as a point's value; past `i64::MAX`, which no count reaches, it
/// saturates.
fn int(n: u64) -> Number {
    Number::Int(i64::try_from(n).unwrap_or(i64::MAX))
}

fn count_value(n: usize) -> Number {
    int(u64::try_from(n).unwrap_or(u64::MAX))
}

/// Seconds as nanoseconds, saturating.
fn nanos(seconds: u64) -> u64 {
    seconds.saturating_mul(1_000_000_000)
}

/// `event` as one log record: the events file's line as the body, observed
/// by the tick at host time `observed_at`.
pub fn record(event: &Event, observed_at: u64) -> LogRecord {
    let mut body = events::line(event);
    // The line's trailing `\n`.
    body.pop();
    let object: Value = serde_json::from_str(&body).expect("an event's line is JSON");
    let kind = object["kind"].as_str().expect("an event's line has a kind");
    let mut attributes = vec![str_attr("kind", kind)];
    for key in ["pr", "batch"] {
        if let Some(value) = object.get(key) {
            let n = value.as_u64().expect("an event's pr and batch are numbers");
            attributes.push((key.to_owned(), int_attr(n)));
        }
    }
    let (severity_number, severity_text) = match event.kind {
        Kind::Error { fatal: true, .. } => (21, "FATAL"),
        Kind::Error { fatal: false, .. } => (17, "ERROR"),
        _ => (9, "INFO"),
    };
    LogRecord {
        time_unix_nano: nanos(event.t),
        observed_time_unix_nano: nanos(observed_at),
        severity_number,
        severity_text,
        body,
        attributes,
    }
}

/// Events held for delivery at most; past it the oldest are dropped.
pub const EVENT_BUFFER: usize = 10_000;

/// Log records per `/v1/logs` request at most.
pub const LOGS_PER_REQUEST: usize = 1_000;

/// The bound on one request, connection to response.
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// What one tick hands the exporter.
#[derive(Debug, Clone, PartialEq)]
pub struct Tick {
    /// Host time of the tick.
    pub at: u64,
    pub events: Vec<Event>,
    /// The gauges after this tick; `None` when the status structure was
    /// not folded (a fatal tick) or has not observed yet.
    pub snapshot: Option<Snapshot>,
    /// Whether the tick observed the repository: what `tick_age` measures.
    pub observed: bool,
}

/// Sends one request body to `path` (`/v1/metrics` or `/v1/logs`);
/// `Err` is the failure's description.
pub type Sink = Box<dyn FnMut(&str, &[u8]) -> Result<(), String> + Send>;

/// The exporter thread's handle. Dropping it hands the thread nothing more
/// and waits for the export in flight, if any, then for one last export:
/// against an unreachable collector, up to four request timeouts.
pub struct Exporter {
    ticks: Option<mpsc::Sender<Tick>>,
    thread: Option<JoinHandle<()>>,
}

impl Exporter {
    /// Starts the thread, which exports every `interval` and once more when
    /// the exporter is dropped; `started_at` is the host time the sums
    /// start.
    pub fn start(interval: Duration, started_at: u64, mut sink: Sink) -> Exporter {
        let (ticks, received) = mpsc::channel::<Tick>();
        let thread = thread::Builder::new()
            .name("telemetry".to_owned())
            .spawn(move || {
                let mut pump = Pump::new(interval, started_at);
                let mut next_due = Instant::now() + interval;
                loop {
                    let wait = next_due.saturating_duration_since(Instant::now());
                    match received.recv_timeout(wait) {
                        Ok(tick) => pump.take(tick, &mut io::stderr()),
                        Err(RecvTimeoutError::Timeout) => {
                            pump.export(unix_now(), &mut sink, &mut io::stderr());
                            // An export that overran the interval delays the
                            // next; the missed ones are not replayed.
                            let now = Instant::now();
                            while next_due <= now {
                                next_due += interval;
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            pump.export(unix_now(), &mut sink, &mut io::stderr());
                            return;
                        }
                    }
                }
            })
            .expect("the telemetry thread starts");
        Exporter {
            ticks: Some(ticks),
            thread: Some(thread),
        }
    }

    /// Hands a tick over; a thread that is gone takes nothing, and the
    /// queue goes on.
    pub fn tick(&self, tick: Tick) {
        if let Some(ticks) = &self.ticks {
            // A send fails only when the thread has panicked; `drop` says so.
            let _ = ticks.send(tick);
        }
    }
}

impl Drop for Exporter {
    fn drop(&mut self) {
        drop(self.ticks.take());
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            eprintln!("dorfq: telemetry: the exporter thread panicked");
        }
    }
}

/// The exporter thread's state: what it last knows of the queue, the events
/// it has not delivered, and its own counts.
struct Pump {
    interval: Duration,
    /// Each event with the host time of the tick that produced it.
    pending: VecDeque<(u64, Event)>,
    latest: Option<Snapshot>,
    last_observed_at: Option<u64>,
    counters: Counters,
    /// Per path, the requests failed since the last one delivered there.
    failed_runs: BTreeMap<&'static str, u64>,
    /// Events dropped since the last `/v1/logs` request delivered.
    dropped_run: u64,
}

impl Pump {
    fn new(interval: Duration, started_at: u64) -> Pump {
        Pump {
            interval,
            pending: VecDeque::new(),
            latest: None,
            last_observed_at: None,
            counters: Counters {
                started_at,
                export_failures: 0,
                events_dropped: 0,
            },
            failed_runs: BTreeMap::new(),
            dropped_run: 0,
        }
    }

    /// Takes one tick: its snapshot replaces the last, its events join the
    /// buffer, the oldest dropped and counted past [`EVENT_BUFFER`].
    fn take(&mut self, tick: Tick, notes: &mut dyn Write) {
        if tick.snapshot.is_some() {
            self.latest = tick.snapshot;
        }
        if tick.observed {
            self.last_observed_at = Some(tick.at);
        }
        self.pending
            .extend(tick.events.into_iter().map(|event| (tick.at, event)));
        let excess = self.pending.len().saturating_sub(EVENT_BUFFER);
        if excess == 0 {
            return;
        }
        self.pending.drain(..excess);
        let excess = u64::try_from(excess).unwrap_or(u64::MAX);
        self.counters.events_dropped = self.counters.events_dropped.saturating_add(excess);
        if self.dropped_run == 0 {
            note(
                notes,
                format_args!(
                    "{EVENT_BUFFER} events pending; dropping the oldest until the collector takes them"
                ),
            );
        }
        self.dropped_run = self.dropped_run.saturating_add(excess);
    }

    /// One export at host time `at`: the gauges when there are any, then
    /// the pending events in chunks until one is refused.
    fn export(&mut self, at: u64, sink: &mut Sink, notes: &mut dyn Write) {
        let resource = resource();
        if let Some(latest) = &self.latest {
            let tick_age = self.last_observed_at.map(|t| at.saturating_sub(t));
            let body = otlp::metrics_request(
                Encoding::Json,
                &resource,
                SCOPE,
                &metrics(latest, tick_age, &self.counters, at),
            );
            let result = sink("/v1/metrics", &body);
            self.count("/v1/metrics", result, notes);
        }
        while !self.pending.is_empty() {
            let chunk = self.pending.len().min(LOGS_PER_REQUEST);
            let records: Vec<LogRecord> = self
                .pending
                .range(..chunk)
                .map(|(observed_at, event)| record(event, *observed_at))
                .collect();
            let body = otlp::logs_request(Encoding::Json, &resource, SCOPE, &records);
            let result = sink("/v1/logs", &body);
            if !self.count("/v1/logs", result, notes) {
                break;
            }
            self.pending.drain(..chunk);
        }
    }

    /// Counts one request's `result` and says so on a change between
    /// delivering and failing on `path`; a delivered `/v1/logs` request
    /// also ends a run of drops and says how many. Whether it was
    /// delivered.
    fn count(
        &mut self,
        path: &'static str,
        result: Result<(), String>,
        notes: &mut dyn Write,
    ) -> bool {
        let run = self.failed_runs.entry(path).or_default();
        match result {
            Ok(()) => {
                let failed = std::mem::take(run);
                let dropped = match path {
                    "/v1/logs" => std::mem::take(&mut self.dropped_run),
                    _ => 0,
                };
                match (failed, dropped) {
                    (0, 0) => {}
                    (0, dropped) => note(
                        notes,
                        format_args!("{dropped} events dropped while the buffer was full"),
                    ),
                    (failed, 0) => note(
                        notes,
                        format_args!("export to {path} resumed after {failed} failed requests"),
                    ),
                    (failed, dropped) => note(
                        notes,
                        format_args!(
                            "export to {path} resumed after {failed} failed requests, \
                             {dropped} events dropped"
                        ),
                    ),
                }
                true
            }
            Err(error) => {
                if *run == 0 {
                    note(
                        notes,
                        format_args!(
                            "export to {path} failed: {error}; retrying every {:?}",
                            self.interval
                        ),
                    );
                }
                *run += 1;
                self.counters.export_failures = self.counters.export_failures.saturating_add(1);
                false
            }
        }
    }
}

/// Writes one `dorfq: telemetry:` line; a line that cannot be written is
/// lost, as stderr's are.
fn note(notes: &mut dyn Write, message: fmt::Arguments<'_>) {
    let _ = writeln!(notes, "dorfq: telemetry: {message}");
}

/// The variable `authorization_env` names cannot be sent as the
/// `Authorization` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationError {
    /// Unset, empty or not UTF-8.
    Missing { var: String },
    /// Holds a byte an HTTP header value cannot carry, such as a trailing
    /// line feed.
    Invalid { var: String },
}

impl fmt::Display for AuthorizationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthorizationError::Missing { var } => write!(
                f,
                "telemetry.authorization_env {var} is unset, empty or not UTF-8"
            ),
            AuthorizationError::Invalid { var } => write!(
                f,
                "telemetry.authorization_env {var} holds a value that is not a valid HTTP header value"
            ),
        }
    }
}

impl std::error::Error for AuthorizationError {}

/// A [`Sink`] that POSTs each body to the configured collector as
/// [`Encoding::Json`]'s content type, with the `Authorization` header when
/// one is configured. A 2xx is delivered; any other status, or no response,
/// is the failure's description.
pub fn http_sink(config: &TelemetryConfig) -> Result<Sink, AuthorizationError> {
    let authorization = config
        .authorization_env
        .as_ref()
        .map(|var| match crate::providers::env_secret(var) {
            Some(value) => authorization_header(var, &value),
            None => Err(AuthorizationError::Missing { var: var.clone() }),
        })
        .transpose()?;
    Ok(post_to(config.otlp_endpoint.clone(), authorization))
}

/// `value`, read from the variable `var`, as a header value; never echoed.
fn authorization_header(var: &str, value: &str) -> Result<HeaderValue, AuthorizationError> {
    let mut header = HeaderValue::from_str(value).map_err(|_| AuthorizationError::Invalid {
        var: var.to_owned(),
    })?;
    header.set_sensitive(true);
    Ok(header)
}

/// The sink of [`http_sink`] for `endpoint` and an already checked
/// `authorization`.
fn post_to(endpoint: String, authorization: Option<HeaderValue>) -> Sink {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .https_only(false)
        .max_redirects(0)
        .timeout_global(Some(TIMEOUT))
        .user_agent(crate::user_agent())
        .build()
        .into();
    Box::new(move |path: &str, body: &[u8]| {
        let mut request = agent
            .post(format!("{endpoint}{path}"))
            .header("Content-Type", Encoding::Json.content_type());
        if let Some(authorization) = &authorization {
            request = request.header("Authorization", authorization.clone());
        }
        let mut response = request.send(body).map_err(|error| error.to_string())?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let mut head = Vec::new();
        // The body is only a hint for the operator; an unreadable one is
        // left out.
        let _ = response
            .body_mut()
            .as_reader()
            .take(200)
            .read_to_end(&mut head);
        Err(format!(
            "HTTP {}: {}",
            status.as_u16(),
            one_line(&String::from_utf8_lossy(&head))
        ))
    })
}

/// `text` with every run of whitespace and control characters made one
/// space, so a response body stays on its note's line.
fn one_line(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod wire_reader;
