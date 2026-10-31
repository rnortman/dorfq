use std::sync::mpsc::{self, Receiver};

use super::*;

const SECRET: &str = "webhook secret";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

fn settings() -> Settings {
    Settings {
        trunk: "main".to_owned(),
        label: "dorfq".to_owned(),
        checks: vec!["build".to_owned()],
        batch_checks: vec!["batch".to_owned()],
        lock_heads: false,
        ci: None,
        refresh: crate::refresh::Refresh {
            listing: Duration::from_secs(300),
            pr: Duration::from_secs(1800),
            batch: Duration::from_secs(300),
        },
        status_issue: None,
    }
}

const REPOSITORY: &str = "alice/widgets";

fn filter() -> Filter {
    Filter {
        repository: REPOSITORY.to_owned(),
        settings: settings(),
    }
}

/// `body` with its `repository` set to `full_name`; text that is not a
/// JSON object is returned as it is.
fn from_repository(full_name: &str, body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(serde_json::Value::Object(mut fields)) => {
            fields.insert(
                "repository".to_owned(),
                serde_json::json!({ "full_name": full_name }),
            );
            serde_json::Value::Object(fields).to_string()
        }
        _ => body.to_owned(),
    }
}

/// What `body` delivered from the queue's repository becomes.
fn hint(event: &str, body: &str) -> Result<Hint, NoHint> {
    hint_of(
        event,
        from_repository(REPOSITORY, body).as_bytes(),
        &filter(),
    )
}

fn hinted(hint: Hint) -> Result<Hint, NoHint> {
    Ok(hint)
}

fn dropped(dropped: Dropped) -> Result<Hint, NoHint> {
    Err(NoHint::Dropped(dropped))
}

/// Whether `outcome` is a body that is not the JSON its event carries.
fn unreadable(outcome: Result<Hint, NoHint>) -> bool {
    matches!(outcome, Err(NoHint::Unreadable(_)))
}

fn unlabelled() -> Result<Hint, NoHint> {
    dropped(Dropped::Unlabelled {
        label: "dorfq".to_owned(),
    })
}

fn signature(secret: &str, body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let tag = hmac::sign(&key, body);
    let digits: String = tag
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256={digits}")
}

#[test]
fn verify_accepts_githubs_documented_digest() {
    assert!(verify(
        b"It's a Secret to Everybody",
        b"Hello, World!",
        Some("sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"),
    ));
}

#[test]
fn verify_refuses_wrong_secret_tampered_body_missing_header_and_sha1() {
    let body = b"{\"zen\":\"Keep it logically awesome.\"}";
    let good = signature(SECRET, body);
    assert!(verify(SECRET.as_bytes(), body, Some(&good)));
    assert!(!verify(b"another secret", body, Some(&good)));
    assert!(!verify(
        SECRET.as_bytes(),
        b"{\"zen\":\"tampered\"}",
        Some(&good)
    ));
    assert!(!verify(SECRET.as_bytes(), body, None));
    let sha1 = good.replacen("sha256=", "sha1=", 1);
    assert!(!verify(SECRET.as_bytes(), body, Some(&sha1)));
    assert!(!verify(SECRET.as_bytes(), body, Some("sha256=zz")));
    assert!(!verify(SECRET.as_bytes(), body, Some("sha256=abc")));
}

#[test]
fn pull_request_with_the_label_is_a_pr_hint() {
    let body = r#"{"action":"synchronize","number":7,
        "pull_request":{"number":7,"labels":[{"name":"bug"},{"name":"dorfq"}]}}"#;
    assert_eq!(hint("pull_request", body), hinted(Hint::Pr(PrNumber(7))));
}

#[test]
fn pull_request_losing_the_label_is_a_pr_hint() {
    let body = r#"{"action":"unlabeled","label":{"name":"dorfq"},
        "pull_request":{"number":8,"labels":[]}}"#;
    assert_eq!(hint("pull_request", body), hinted(Hint::Pr(PrNumber(8))));
}

#[test]
fn pull_request_without_the_label_is_no_hint() {
    let body = r#"{"action":"opened","pull_request":{"number":9,"labels":[{"name":"bug"}]}}"#;
    assert_eq!(hint("pull_request", body), unlabelled());
    let other_label = r#"{"action":"unlabeled","label":{"name":"bug"},
        "pull_request":{"number":9,"labels":[]}}"#;
    assert_eq!(hint("pull_request", other_label), unlabelled());
}

#[test]
fn review_of_a_labelled_pull_request_is_a_pr_hint() {
    let body = r#"{"action":"submitted","review":{"state":"approved"},
        "pull_request":{"number":10,"labels":[{"name":"dorfq"}]}}"#;
    assert_eq!(
        hint("pull_request_review", body),
        hinted(Hint::Pr(PrNumber(10)))
    );
    let unlabelled = r#"{"action":"submitted","pull_request":{"number":10,"labels":[]}}"#;
    assert_eq!(hint("pull_request_review", unlabelled), self::unlabelled());
}

#[test]
fn completed_check_run_of_a_read_check_is_a_commit_hint() {
    let commit = Hint::Commit(Sha::parse(HEAD).unwrap());
    for name in ["build", "batch"] {
        let body = format!(
            r#"{{"action":"completed","check_run":{{"name":"{name}","head_sha":"{HEAD}"}}}}"#
        );
        assert_eq!(hint("check_run", &body), hinted(commit.clone()));
    }
}

#[test]
fn check_run_not_completed_or_not_read_is_no_hint() {
    let other =
        format!(r#"{{"action":"completed","check_run":{{"name":"lint","head_sha":"{HEAD}"}}}}"#);
    assert_eq!(
        hint("check_run", &other),
        dropped(Dropped::NotRequired {
            check: "lint".to_owned()
        })
    );
    let running =
        format!(r#"{{"action":"created","check_run":{{"name":"build","head_sha":"{HEAD}"}}}}"#);
    assert_eq!(
        hint("check_run", &running),
        dropped(Dropped::NotCompleted { suite: false })
    );
    // Completion is tested before the name.
    let running_other =
        format!(r#"{{"action":"created","check_run":{{"name":"lint","head_sha":"{HEAD}"}}}}"#);
    assert_eq!(
        hint("check_run", &running_other),
        dropped(Dropped::NotCompleted { suite: false })
    );
}

#[test]
fn completed_check_suite_is_a_commit_hint() {
    let body = format!(r#"{{"action":"completed","check_suite":{{"head_sha":"{HEAD}"}}}}"#);
    assert_eq!(
        hint("check_suite", &body),
        hinted(Hint::Commit(Sha::parse(HEAD).unwrap()))
    );
    let requested = format!(r#"{{"action":"requested","check_suite":{{"head_sha":"{HEAD}"}}}}"#);
    assert_eq!(
        hint("check_suite", &requested),
        dropped(Dropped::NotCompleted { suite: true })
    );
}

#[test]
fn settled_status_of_a_read_context_is_a_commit_hint() {
    let body = format!(r#"{{"sha":"{HEAD}","state":"success","context":"build"}}"#);
    assert_eq!(
        hint("status", &body),
        hinted(Hint::Commit(Sha::parse(HEAD).unwrap()))
    );
    let pending = format!(r#"{{"sha":"{HEAD}","state":"pending","context":"build"}}"#);
    assert_eq!(hint("status", &pending), dropped(Dropped::Pending));
    let other = format!(r#"{{"sha":"{HEAD}","state":"failure","context":"lint"}}"#);
    assert_eq!(
        hint("status", &other),
        dropped(Dropped::NotRequiredContext {
            context: "lint".to_owned()
        })
    );
}

#[test]
fn push_to_trunk_is_a_trunk_hint() {
    let body = r#"{"ref":"refs/heads/main","after":"0000000000000000000000000000000000000000"}"#;
    assert_eq!(hint("push", body), hinted(Hint::Trunk));
    for reference in ["refs/heads/feature", "refs/tags/main"] {
        assert_eq!(
            hint("push", &format!(r#"{{"ref":"{reference}"}}"#)),
            dropped(Dropped::OtherRef {
                reference: reference.to_owned(),
                trunk: "main".to_owned()
            })
        );
    }
}

#[test]
fn other_events_are_no_hint() {
    let issue = r#"{"action":"labeled","issue":{"number":1},"label":{"name":"dorfq"}}"#;
    assert_eq!(hint("issues", issue), dropped(Dropped::Unwatched));
    assert_eq!(
        hint("ping", r#"{"zen":"Design for failure."}"#),
        dropped(Dropped::Unwatched)
    );
}

#[test]
fn another_repositorys_delivery_is_no_hint() {
    let deliveries = [
        (
            "pull_request",
            r#"{"action":"labeled","pull_request":{"number":7,"labels":[{"name":"dorfq"}]}}"#
                .to_owned(),
        ),
        (
            "pull_request_review",
            r#"{"action":"submitted","pull_request":{"number":7,"labels":[{"name":"dorfq"}]}}"#
                .to_owned(),
        ),
        (
            "check_run",
            format!(
                r#"{{"action":"completed","check_run":{{"name":"build","head_sha":"{HEAD}"}}}}"#
            ),
        ),
        (
            "check_suite",
            format!(r#"{{"action":"completed","check_suite":{{"head_sha":"{HEAD}"}}}}"#),
        ),
        (
            "status",
            format!(r#"{{"sha":"{HEAD}","state":"success","context":"build"}}"#),
        ),
        ("push", r#"{"ref":"refs/heads/main"}"#.to_owned()),
    ];
    for (event, body) in deliveries {
        let elsewhere = |found: &str| {
            dropped(Dropped::OtherRepository {
                found: found.to_owned(),
                queue: REPOSITORY.to_owned(),
            })
        };
        assert!(hint(event, &body).is_ok(), "{event}");
        let other = from_repository("alice/throwaway", &body);
        assert_eq!(
            hint_of(event, other.as_bytes(), &filter()),
            elsewhere("alice/throwaway"),
            "{event}"
        );
        let same_name_elsewhere = from_repository("bob/widgets", &body);
        assert_eq!(
            hint_of(event, same_name_elsewhere.as_bytes(), &filter()),
            elsewhere("bob/widgets"),
            "{event}"
        );
        let other_case = from_repository("Alice/Widgets", &body);
        assert!(
            hint_of(event, other_case.as_bytes(), &filter()).is_ok(),
            "{event}"
        );
        assert!(
            unreadable(hint_of(event, body.as_bytes(), &filter())),
            "{event}"
        );
    }
}

#[test]
fn malformed_bodies_are_errors_without_a_hint() {
    assert!(unreadable(hint("pull_request", "not json")));
    assert!(unreadable(hint("push", "{}")));
    assert!(unreadable(hint(
        "check_run",
        r#"{"action":"completed","check_run":{"name":"build"}}"#
    )));
    let short_sha = r#"{"action":"completed","check_run":{"name":"build","head_sha":"abc"}}"#;
    assert!(unreadable(hint("check_run", short_sha)));
}

/// A listener on a free loopback port, and the receiving end of its hints.
fn listener(deadline: Duration) -> (SocketAddr, Receiver<Hint>) {
    let (sender, receiver) = mpsc::channel();
    let listener = Listener::spawn_with_deadline(
        "127.0.0.1:0".parse().unwrap(),
        SECRET.to_owned(),
        filter(),
        sender,
        deadline,
    )
    .unwrap();
    (listener.address(), receiver)
}

/// Posts `body` as a delivery of `event`, signed when `signed`, and
/// returns the status.
fn deliver(address: SocketAddr, event: &str, body: &str, signed: bool) -> u16 {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut request = agent
        .post(format!("http://{address}/hooks/github"))
        .header("Content-Type", "application/json")
        .header("X-GitHub-Event", event)
        .header("X-GitHub-Delivery", "72d3162e-cc78-11e3-81ab-4c9367dc0958");
    if signed {
        request = request.header("X-Hub-Signature-256", signature(SECRET, body.as_bytes()));
    }
    request.send(body.as_bytes()).unwrap().status().as_u16()
}

const PUSH: &str = r#"{"ref":"refs/heads/main","repository":{"full_name":"alice/widgets"}}"#;

/// Delivers a signed trunk push and asserts its hint is the next one.
fn assert_next_hint_is_a_fresh_push(address: SocketAddr, hints: &Receiver<Hint>) {
    assert_eq!(deliver(address, "push", PUSH, true), 204);
    assert_eq!(hints.recv_timeout(Duration::from_secs(10)), Ok(Hint::Trunk));
}

/// Sends `request` on a fresh connection and returns everything read back
/// until the server closes it.
fn exchange(address: SocketAddr, request: &[u8]) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(request).unwrap();
    let mut response = Vec::new();
    // A server that closes with request bytes unread resets the connection
    // after its response; what was read before the reset is the response.
    let _ = stream.read_to_end(&mut response);
    String::from_utf8_lossy(&response).into_owned()
}

#[test]
fn signed_delivery_is_answered_and_its_hint_sent() {
    let (address, hints) = listener(DEADLINE);
    let body = r#"{"action":"labeled","pull_request":{"number":42,"labels":[{"name":"dorfq"}]},
        "repository":{"full_name":"alice/widgets"}}"#;
    assert_eq!(deliver(address, "pull_request", body, true), 204);
    assert_eq!(
        hints.recv_timeout(Duration::from_secs(10)),
        Ok(Hint::Pr(PrNumber(42)))
    );
}

#[test]
fn unsigned_delivery_is_401_and_sends_nothing() {
    let (address, hints) = listener(DEADLINE);
    assert_eq!(deliver(address, "push", PUSH, false), 401);
    let body = r#"{"ref":"refs/heads/main","forged":true}"#;
    let request = format!(
        "POST / HTTP/1.1\r\nX-GitHub-Event: push\r\nX-GitHub-Delivery: d\r\n\
         X-Hub-Signature-256: {}\r\nContent-Length: {}\r\n\r\n{body}",
        signature("another secret", body.as_bytes()),
        body.len()
    );
    assert!(exchange(address, request.as_bytes()).starts_with("HTTP/1.1 401 "));
    assert_next_hint_is_a_fresh_push(address, &hints);
    assert_eq!(hints.try_recv(), Err(mpsc::TryRecvError::Empty));
}

#[test]
fn get_is_405() {
    let (address, _hints) = listener(DEADLINE);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let status = agent
        .get(format!("http://{address}/"))
        .call()
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(status, 405);
}

#[test]
fn ping_is_204_and_sends_nothing() {
    let (address, hints) = listener(DEADLINE);
    assert_eq!(
        deliver(address, "ping", r#"{"zen":"Speak like a human."}"#, true),
        204
    );
    assert_next_hint_is_a_fresh_push(address, &hints);
}

#[test]
fn verified_delivery_with_a_malformed_body_is_204_and_sends_nothing() {
    let (address, hints) = listener(DEADLINE);
    assert_eq!(deliver(address, "push", "[1, 2, 3]", true), 204);
    assert_next_hint_is_a_fresh_push(address, &hints);
}

#[test]
fn huge_declared_length_is_413_at_once_and_the_next_delivery_is_served() {
    let (address, hints) = listener(DEADLINE);
    let response = exchange(
        address,
        b"POST / HTTP/1.1\r\nX-GitHub-Event: push\r\nX-GitHub-Delivery: d\r\n\
          Content-Length: 1000000000000000\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 413 "), "{response}");
    assert_next_hint_is_a_fresh_push(address, &hints);
}

#[test]
fn refusals_before_the_body() {
    let (address, _hints) = listener(DEADLINE);
    let cases: [(&str, String); 7] = [
        (
            "411 ",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".to_owned(),
        ),
        ("411 ", "POST / HTTP/1.1\r\nHost: x\r\n\r\n".to_owned()),
        (
            "400 ",
            "POST / HTTP/1.1\r\nContent-Length: abc\r\n\r\n".to_owned(),
        ),
        (
            "400 ",
            "POST / HTTP/1.1\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n".to_owned(),
        ),
        (
            "417 ",
            "POST / HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\n".to_owned(),
        ),
        (
            "431 ",
            format!(
                "POST / HTTP/1.1\r\n{}\r\n",
                "X-Filler: x\r\n".repeat(MAX_HEADERS + 1)
            ),
        ),
        (
            "431 ",
            format!(
                "POST / HTTP/1.1\r\nX-Filler: {}\r\n\r\n",
                "x".repeat(9 * 1024)
            ),
        ),
    ];
    for (status, request) in cases {
        let response = exchange(address, request.as_bytes());
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status}")),
            "{request:?} -> {response:?}"
        );
    }
}

#[test]
fn garbage_is_400() {
    let (address, _hints) = listener(DEADLINE);
    let response = exchange(
        address,
        b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 400 "), "{response:?}");
}

#[test]
fn missing_delivery_headers_are_400() {
    let (address, _hints) = listener(DEADLINE);
    let request = format!(
        "POST / HTTP/1.1\r\nX-GitHub-Event: push\r\nX-Hub-Signature-256: {}\r\n\
         Content-Length: {}\r\n\r\n{PUSH}",
        signature(SECRET, PUSH.as_bytes()),
        PUSH.len()
    );
    assert!(exchange(address, request.as_bytes()).starts_with("HTTP/1.1 400 "));
}

#[test]
fn every_response_closes_the_connection() {
    let (address, _hints) = listener(DEADLINE);
    let request = format!(
        "POST / HTTP/1.1\r\nX-GitHub-Event: push\r\nX-GitHub-Delivery: d\r\n\
         X-Hub-Signature-256: {}\r\nContent-Length: {}\r\n\r\n{PUSH}",
        signature(SECRET, PUSH.as_bytes()),
        PUSH.len()
    );
    let response = exchange(address, request.as_bytes());
    assert_eq!(
        response,
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let refused = exchange(address, b"GET / HTTP/1.1\r\n\r\n");
    assert_eq!(
        refused,
        "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
}

#[test]
fn a_body_that_never_arrives_is_dropped_at_the_deadline() {
    let (address, hints) = listener(Duration::from_millis(300));
    let started = Instant::now();
    let response = exchange(
        address,
        b"POST / HTTP/1.1\r\nX-GitHub-Event: push\r\nX-GitHub-Delivery: d\r\n\
          Content-Length: 10\r\n\r\n",
    );
    assert_eq!(response, "");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_next_hint_is_a_fresh_push(address, &hints);
}

#[test]
fn a_client_that_sends_nothing_is_dropped_at_the_deadline() {
    let (address, hints) = listener(Duration::from_millis(300));
    assert_eq!(exchange(address, b""), "");
    assert_next_hint_is_a_fresh_push(address, &hints);
}

#[test]
fn bind_failure_names_the_address() {
    let (address, _hints) = listener(DEADLINE);
    let (sender, _receiver) = mpsc::channel();
    let error = Listener::spawn(
        address,
        SECRET.to_owned(),
        REPOSITORY.to_owned(),
        settings(),
        sender,
    )
    .unwrap_err();
    assert!(error.to_string().contains(&address.to_string()), "{error}");
}

#[test]
fn refusals_an_operator_can_act_on_write_a_line_naming_the_delivery() {
    let id = Some("72d3162e-cc78-11e3-81ab-4c9367dc0958");
    assert_eq!(
        refusal_line(Status::Unauthorized, id).as_deref(),
        Some(
            "dorfq: webhook delivery \"72d3162e-cc78-11e3-81ab-4c9367dc0958\": answered \
             401 Unauthorized: the signature is missing or does not verify"
        )
    );
    assert_eq!(
        refusal_line(Status::LengthRequired, None).as_deref(),
        Some(
            "dorfq: webhook request answered 411 Length Required: the request has no \
             Content-Length; a proxy in front must forward the body with its length"
        )
    );
    assert_eq!(
        refusal_line(Status::PayloadTooLarge, id).as_deref(),
        Some(
            "dorfq: webhook delivery \"72d3162e-cc78-11e3-81ab-4c9367dc0958\": answered \
             413 Content Too Large: the declared body is over 25 MiB"
        )
    );
    for silent in [
        Status::NoContent,
        Status::BadRequest,
        Status::MethodNotAllowed,
        Status::ExpectationFailed,
        Status::HeaderFieldsTooLarge,
    ] {
        assert_eq!(refusal_line(silent, id), None, "{}", silent.line());
    }
}

#[test]
fn an_unreadable_delivery_writes_a_line_naming_it_and_its_event() {
    let body = from_repository(REPOSITORY, "{}");
    let Err(NoHint::Unreadable(error)) = hint_of("push", body.as_bytes(), &filter()) else {
        panic!("an empty push body is unreadable");
    };
    let delivery = Delivery {
        id: "d-1".to_owned(),
        event: "push".to_owned(),
        body: body.into_bytes(),
    };
    assert_eq!(
        outcome(&delivery, &filter()),
        (
            format!("dorfq: webhook delivery \"d-1\": unreadable \"push\" event: {error}"),
            None
        )
    );
}

#[test]
fn each_dropped_reason_reads_as_the_operator_is_told() {
    let reasons = [
        (
            Dropped::OtherRepository {
                found: "bob/widgets".to_owned(),
                queue: REPOSITORY.to_owned(),
            },
            r#"repository "bob/widgets" is not the queue's "alice/widgets""#,
        ),
        (
            Dropped::Unlabelled {
                label: "dorfq".to_owned(),
            },
            r#"the pull request does not carry the label "dorfq""#,
        ),
        (
            Dropped::NotCompleted { suite: false },
            "the run is not completed",
        ),
        (
            Dropped::NotCompleted { suite: true },
            "the suite is not completed",
        ),
        (
            Dropped::NotRequired {
                check: "lint".to_owned(),
            },
            r#"check "lint" is neither a required nor a batch check"#,
        ),
        (
            Dropped::NotRequiredContext {
                context: "ci/legacy".to_owned(),
            },
            r#"context "ci/legacy" is neither a required nor a batch check"#,
        ),
        (Dropped::Pending, "the status is pending"),
        (
            Dropped::OtherRef {
                reference: "refs/heads/feature".to_owned(),
                trunk: "main".to_owned(),
            },
            r#"ref "refs/heads/feature" is not trunk ("refs/heads/main")"#,
        ),
        (Dropped::Unwatched, "the queue does not watch this event"),
    ];
    for (reason, text) in reasons {
        assert_eq!(reason.to_string(), text);
    }
}

/// The line `handle` writes for a verified delivery of `event` with `body`
/// from the queue's repository, and the hint it sends.
fn outcome_for(event: &str, body: &str) -> (String, Option<Hint>) {
    let delivery = Delivery {
        id: "7a1-42".to_owned(),
        event: event.to_owned(),
        body: from_repository(REPOSITORY, body).into_bytes(),
    };
    outcome(&delivery, &filter())
}

#[test]
fn a_verified_delivery_writes_its_event_action_and_verdict() {
    assert_eq!(
        outcome_for(
            "pull_request",
            r#"{"action":"labeled","label":{"name":"dorfq"},
                "pull_request":{"number":42,"labels":[{"name":"dorfq"}]}}"#
        ),
        (
            r#"dorfq: webhook delivery "7a1-42": "pull_request" "labeled": hint pull request #42"#
                .to_owned(),
            Some(Hint::Pr(PrNumber(42)))
        )
    );
    assert_eq!(
        outcome_for("push", r#"{"ref":"refs/heads/main"}"#),
        (
            r#"dorfq: webhook delivery "7a1-42": "push": hint trunk"#.to_owned(),
            Some(Hint::Trunk)
        )
    );
    assert_eq!(
        outcome_for(
            "check_run",
            &format!(
                r#"{{"action":"completed","check_run":{{"name":"lint","head_sha":"{HEAD}"}}}}"#
            )
        ),
        (
            r#"dorfq: webhook delivery "7a1-42": "check_run" "completed": no hint: check "lint" is neither a required nor a batch check"#
                .to_owned(),
            None
        )
    );
    assert_eq!(
        outcome_for(
            "status",
            &format!(r#"{{"sha":"{HEAD}","state":"success","context":"build"}}"#)
        ),
        (
            format!(r#"dorfq: webhook delivery "7a1-42": "status": hint commit {HEAD}"#),
            Some(Hint::Commit(Sha::parse(HEAD).unwrap()))
        )
    );
    assert_eq!(
        outcome_for("ping", r#"{"zen":"Design for failure.","hook_id":1}"#),
        (
            r#"dorfq: webhook delivery "7a1-42": "ping": no hint: the queue does not watch this event"#
                .to_owned(),
            None
        )
    );
}

#[test]
fn an_action_that_is_not_a_string_or_a_body_that_is_not_json_is_no_action() {
    assert_eq!(action(br#"{"action":"opened"}"#).as_deref(), Some("opened"));
    assert_eq!(action(br#"{"action":7}"#), None);
    assert_eq!(action(br#"{"zen":"x"}"#), None);
    assert_eq!(action(b"not json"), None);
}
