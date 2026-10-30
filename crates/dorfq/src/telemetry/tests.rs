use dorfq_core::{
    BatchState, CheckName, CiStatus, ConflictWith, Failure, Hold, LandRefusal, PrNumber, PrStatus,
    Sha,
};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::otlp::{
    Attr, Data, LogRecord, Metric, Number, Point, Resource, logs_request, metrics_request,
};
use super::*;
use crate::journal::status::{Batch, QueuedPr};
use crate::{driver, journal};

fn resource_fixture() -> Resource {
    Resource {
        attributes: vec![
            ("service.name".to_owned(), Attr::Str("dorfq".to_owned())),
            ("service.version".to_owned(), Attr::Str("1.2.3".to_owned())),
        ],
    }
}

#[test]
fn metrics_request_encodes_a_gauge_and_a_sum() {
    let metrics = vec![
        Metric {
            name: "g".to_owned(),
            description: "a gauge".to_owned(),
            unit: "s".to_owned(),
            data: Data::Gauge(vec![
                Point {
                    attributes: vec![
                        ("status".to_owned(), Attr::Str("ready".to_owned())),
                        ("window_seconds".to_owned(), Attr::Int(86400)),
                    ],
                    time_unix_nano: 1_700_000_000_000_000_000,
                    value: Number::Int(3),
                },
                Point {
                    attributes: Vec::new(),
                    time_unix_nano: 1_700_000_000_000_000_000,
                    value: Number::Double(1.5),
                },
            ]),
        },
        Metric {
            name: "c_total".to_owned(),
            description: "a sum".to_owned(),
            unit: "{request}".to_owned(),
            data: Data::Sum {
                start_unix_nano: 1_600_000_000_000_000_000,
                points: vec![Point {
                    attributes: Vec::new(),
                    time_unix_nano: 1_700_000_000_000_000_000,
                    value: Number::Int(7),
                }],
            },
        },
    ];
    let body = metrics_request(&resource_fixture(), "dorfq", &metrics);
    let expected = concat!(
        r#"{"resourceMetrics":[{"resource":{"attributes":["#,
        r#"{"key":"service.name","value":{"stringValue":"dorfq"}},"#,
        r#"{"key":"service.version","value":{"stringValue":"1.2.3"}}]},"#,
        r#""scopeMetrics":[{"scope":{"name":"dorfq"},"metrics":["#,
        r#"{"name":"g","description":"a gauge","unit":"s","gauge":{"dataPoints":["#,
        r#"{"attributes":[{"key":"status","value":{"stringValue":"ready"}},"#,
        r#"{"key":"window_seconds","value":{"intValue":"86400"}}],"#,
        r#""timeUnixNano":"1700000000000000000","asInt":"3"},"#,
        r#"{"attributes":[],"timeUnixNano":"1700000000000000000","asDouble":1.5}]}},"#,
        r#"{"name":"c_total","description":"a sum","unit":"{request}","sum":{"dataPoints":["#,
        r#"{"attributes":[],"startTimeUnixNano":"1600000000000000000","#,
        r#""timeUnixNano":"1700000000000000000","asInt":"7"}],"#,
        r#""aggregationTemporality":2,"isMonotonic":true}}]}]}]}"#,
    );
    assert_eq!(body, expected);
    assert!(!body.contains("null"));
}

fn log_records() -> Vec<LogRecord> {
    vec![
        LogRecord {
            time_unix_nano: 5_000_000_000,
            observed_time_unix_nano: 6_000_000_000,
            severity_number: 9,
            severity_text: "INFO",
            body: r#"{"t":5,"kind":"merged","pr":4}"#.to_owned(),
            attributes: vec![
                ("kind".to_owned(), Attr::Str("merged".to_owned())),
                ("pr".to_owned(), Attr::Int(4)),
            ],
        },
        LogRecord {
            time_unix_nano: 7_000_000_000,
            observed_time_unix_nano: 8_000_000_000,
            severity_number: 17,
            severity_text: "ERROR",
            body: "boom".to_owned(),
            attributes: Vec::new(),
        },
    ]
}

#[test]
fn logs_request_encodes_records() {
    let body = logs_request(&resource_fixture(), "dorfq", &log_records());
    let expected = concat!(
        r#"{"resourceLogs":[{"resource":{"attributes":["#,
        r#"{"key":"service.name","value":{"stringValue":"dorfq"}},"#,
        r#"{"key":"service.version","value":{"stringValue":"1.2.3"}}]},"#,
        r#""scopeLogs":[{"scope":{"name":"dorfq"},"logRecords":["#,
        r#"{"timeUnixNano":"5000000000","observedTimeUnixNano":"6000000000","#,
        r#""severityNumber":9,"severityText":"INFO","#,
        r#""body":{"stringValue":"{\"t\":5,\"kind\":\"merged\",\"pr\":4}"},"#,
        r#""attributes":[{"key":"kind","value":{"stringValue":"merged"}},"#,
        r#"{"key":"pr","value":{"intValue":"4"}}]},"#,
        r#"{"timeUnixNano":"7000000000","observedTimeUnixNano":"8000000000","#,
        r#""severityNumber":17,"severityText":"ERROR","#,
        r#""body":{"stringValue":"boom"},"attributes":[]}]}]}]}"#,
    );
    assert_eq!(body, expected);
}

#[test]
fn encoding_is_deterministic() {
    let snapshot = snapshot(&document(), 1_000, Some(gauges(Some(10))));
    let counters = counters();
    let a = metrics_request(
        &resource(),
        SCOPE,
        &metrics(&snapshot, Some(3), &counters, 2_000),
    );
    let b = metrics_request(
        &resource(),
        SCOPE,
        &metrics(&snapshot, Some(3), &counters, 2_000),
    );
    assert_eq!(a, b);
    let records = log_records();
    assert_eq!(
        logs_request(&resource(), SCOPE, &records),
        logs_request(&resource(), SCOPE, &records)
    );
}

#[test]
fn resource_names_the_service_and_its_version() {
    assert_eq!(
        resource().attributes,
        vec![
            ("service.name".to_owned(), Attr::Str("dorfq".to_owned())),
            (
                "service.version".to_owned(),
                Attr::Str(crate::version().to_owned())
            ),
        ]
    );
}

fn queued(number: u64, status: &str, arrived: u64) -> QueuedPr {
    QueuedPr {
        number,
        base: None,
        head: format!("{number:040x}"),
        status: status.to_owned(),
        since: arrived,
        arrived,
        batches: Vec::new(),
    }
}

fn batch(id: u64, state: &str) -> Batch {
    Batch {
        id,
        base: None,
        members: vec![1],
        state: state.to_owned(),
        started: 0,
    }
}

fn document() -> Document {
    Document {
        prs: vec![
            queued(1, "ready", 400),
            queued(2, "waiting:own_ci", 300),
            queued(3, "conflicting:#4", 500),
            queued(5, "ready", 600),
        ],
        batches: vec![
            batch(1, "testing"),
            batch(2, "failed:check"),
            batch(3, "landed"),
            batch(4, "failed:"),
        ],
        recent: Vec::new(),
        figures: Figures {
            landed_prs: 4,
            landed_batches: 2,
            ejected: 1,
            refused: 3,
            passed_runs: 5,
            failed_runs: 6,
            cancelled_runs: 7,
            batch_run_p50_hours: Some(0.5),
            batch_run_p95_hours: Some(2.0),
            land_p50_hours: Some(1.0),
            land_p95_hours: None,
        },
        record_since: Some(0),
    }
}

fn gauges(rate_limit_remaining: Option<u64>) -> ForgeGauges {
    ForgeGauges {
        blocked_prs: 2,
        rate_limit_remaining,
    }
}

fn counters() -> Counters {
    Counters {
        started_at: 100,
        export_failures: 4,
        events_dropped: 9,
    }
}

#[test]
fn snapshot_counts_the_classes_and_the_oldest_arrival() {
    let forge = Some(gauges(Some(10)));
    let snapshot = snapshot(&document(), 1_000, forge);
    assert_eq!(snapshot.queue_prs, [2, 1, 1]);
    assert_eq!(snapshot.batches, [1, 0, 2, 1, 0, 0]);
    assert_eq!(snapshot.oldest_queued_seconds, 700);
    assert_eq!(snapshot.figures, document().figures);
    assert_eq!(snapshot.forge, forge);
}

#[test]
fn snapshot_of_an_empty_pool_is_zeros() {
    let document = Document {
        prs: Vec::new(),
        batches: Vec::new(),
        recent: Vec::new(),
        figures: Figures::default(),
        record_since: None,
    };
    let snapshot = snapshot(&document, 1_000, None);
    assert_eq!(snapshot.queue_prs, [0; 3]);
    assert_eq!(snapshot.batches, [0; 6]);
    assert_eq!(snapshot.oldest_queued_seconds, 0);
}

#[test]
#[should_panic(expected = "which is no class")]
fn snapshot_refuses_an_unknown_class() {
    let mut document = document();
    document.prs.push(queued(9, "sleeping", 0));
    snapshot(&document, 0, None);
}

/// One `PrStatus` per variant. The match fails to compile when a variant
/// is added, so the list cannot fall behind the enum.
fn every_status() -> Vec<PrStatus> {
    let all = vec![
        PrStatus::Ready,
        PrStatus::Waiting([Hold::OwnCiPending, Hold::Draft].into()),
        PrStatus::Conflicting(ConflictWith::Pr(PrNumber(4))),
    ];
    for status in &all {
        match status {
            PrStatus::Ready | PrStatus::Waiting(_) | PrStatus::Conflicting(_) => {}
        }
    }
    all
}

/// One `BatchState` per variant and CI outcome, under the same guard. The
/// failed check's name carries a `:`, so the class is the text before the
/// first one.
fn every_state() -> Vec<BatchState> {
    let tip = Sha::parse(&format!("{:040x}", 1)).unwrap();
    let all = vec![
        BatchState::Testing {
            tip: tip.clone(),
            ci: CiStatus::Pending,
        },
        BatchState::Testing {
            tip: tip.clone(),
            ci: CiStatus::Passed,
        },
        BatchState::Testing {
            tip: tip.clone(),
            ci: CiStatus::Failed(Failure::new([CheckName("lint: clippy".to_owned())].into())),
        },
        BatchState::Landed { tip: tip.clone() },
        BatchState::LandRefused {
            tip,
            reason: LandRefusal::MemberGone { pr: PrNumber(2) },
        },
        BatchState::Conflict {
            pr: PrNumber(3),
            with: ConflictWith::Trunk,
        },
    ];
    for state in &all {
        match state {
            BatchState::Testing {
                ci: CiStatus::Pending | CiStatus::Passed | CiStatus::Failed(_),
                ..
            }
            | BatchState::Landed { .. }
            | BatchState::LandRefused { .. }
            | BatchState::Conflict { .. } => {}
        }
    }
    all
}

#[test]
fn snapshot_classes_every_wording_the_producers_give() {
    let document = Document {
        prs: every_status()
            .iter()
            .enumerate()
            .map(|(i, status)| queued(i as u64, &driver::status(status), 0))
            .collect(),
        batches: every_state()
            .iter()
            .enumerate()
            .map(|(i, state)| batch(i as u64, &journal::status::state(state)))
            .collect(),
        recent: Vec::new(),
        figures: Figures::default(),
        record_since: None,
    };
    let snapshot = snapshot(&document, 0, None);
    assert_eq!(snapshot.queue_prs, [1; 3]);
    assert_eq!(snapshot.batches, [1; 6]);
}

/// The metric named `name` in `metrics`.
fn find<'a>(metrics: &'a [Metric], name: &str) -> &'a Metric {
    metrics
        .iter()
        .find(|m| m.name == name)
        .unwrap_or_else(|| panic!("no metric {name}"))
}

fn names(metrics: &[Metric]) -> Vec<&str> {
    metrics.iter().map(|m| m.name.as_str()).collect()
}

/// The gauge's points as (attributes, value).
fn gauge_points(metric: &Metric) -> Vec<(Vec<(String, Attr)>, Number)> {
    match &metric.data {
        Data::Gauge(points) => points
            .iter()
            .map(|p| (p.attributes.clone(), p.value))
            .collect(),
        Data::Sum { .. } => panic!("{} is a sum", metric.name),
    }
}

fn s(key: &str, value: &str) -> (String, Attr) {
    (key.to_owned(), Attr::Str(value.to_owned()))
}

fn window() -> (String, Attr) {
    ("window_seconds".to_owned(), Attr::Int(86400))
}

#[test]
fn metrics_carry_every_instrument() {
    let snapshot = snapshot(&document(), 1_000, Some(gauges(Some(10))));
    let metrics = metrics(&snapshot, Some(42), &counters(), 2_000);
    assert_eq!(
        names(&metrics),
        [
            "dorfq_queue_prs",
            "dorfq_batches",
            "dorfq_oldest_queued_seconds",
            "dorfq_window_landed_prs",
            "dorfq_window_landed_batches",
            "dorfq_window_ejected_prs",
            "dorfq_window_refused_landings",
            "dorfq_window_runs",
            "dorfq_window_batch_run_seconds",
            "dorfq_window_land_seconds",
            "dorfq_tick_age_seconds",
            "dorfq_queue_blocked_prs",
            "dorfq_api_rate_limit_remaining",
            "dorfq_telemetry_export_failures_total",
            "dorfq_telemetry_events_dropped_total",
        ]
    );
    let units: Vec<&str> = metrics.iter().map(|m| m.unit.as_str()).collect();
    assert_eq!(
        units,
        [
            "{pull_request}",
            "{batch}",
            "s",
            "{pull_request}",
            "{batch}",
            "{pull_request}",
            "{batch}",
            "{run}",
            "s",
            "s",
            "s",
            "{pull_request}",
            "{request}",
            "{request}",
            "{event}",
        ]
    );
    assert!(metrics.iter().all(|m| !m.description.is_empty()));
    for metric in &metrics {
        let points = match &metric.data {
            Data::Gauge(points) => points,
            Data::Sum { points, .. } => points,
        };
        assert!(
            points.iter().all(|p| p.time_unix_nano == 2_000_000_000_000),
            "{}",
            metric.name
        );
    }

    assert_eq!(
        gauge_points(find(&metrics, "dorfq_queue_prs")),
        [
            (vec![s("status", "ready")], Number::Int(2)),
            (vec![s("status", "waiting")], Number::Int(1)),
            (vec![s("status", "conflicting")], Number::Int(1)),
        ]
    );
    assert_eq!(
        gauge_points(find(&metrics, "dorfq_batches")),
        [
            (vec![s("state", "testing")], Number::Int(1)),
            (vec![s("state", "passed")], Number::Int(0)),
            (vec![s("state", "failed")], Number::Int(2)),
            (vec![s("state", "landed")], Number::Int(1)),
            (vec![s("state", "refused")], Number::Int(0)),
            (vec![s("state", "conflict")], Number::Int(0)),
        ]
    );
    assert_eq!(
        gauge_points(find(&metrics, "dorfq_oldest_queued_seconds")),
        [(Vec::new(), Number::Int(700))]
    );
    for (name, value) in [
        ("dorfq_window_landed_prs", 4),
        ("dorfq_window_landed_batches", 2),
        ("dorfq_window_ejected_prs", 1),
        ("dorfq_window_refused_landings", 3),
    ] {
        assert_eq!(
            gauge_points(find(&metrics, name)),
            [(vec![window()], Number::Int(value))],
            "{name}"
        );
    }
    assert_eq!(
        gauge_points(find(&metrics, "dorfq_window_runs")),
        [
            (vec![s("verdict", "passed"), window()], Number::Int(5)),
            (vec![s("verdict", "failed"), window()], Number::Int(6)),
            (vec![s("verdict", "cancelled"), window()], Number::Int(7)),
        ]
    );
    assert_eq!(
        gauge_points(find(&metrics, "dorfq_window_batch_run_seconds")),
        [
            (vec![s("quantile", "0.5"), window()], Number::Double(1800.0)),
            (
                vec![s("quantile", "0.95"), window()],
                Number::Double(7200.0)
            ),
        ]
    );
    // `land_p95_hours` is `None`: no point.
    assert_eq!(
        gauge_points(find(&metrics, "dorfq_window_land_seconds")),
        [(vec![s("quantile", "0.5"), window()], Number::Double(3600.0))]
    );
    assert_eq!(
        gauge_points(find(&metrics, "dorfq_tick_age_seconds")),
        [(Vec::new(), Number::Int(42))]
    );
    assert_eq!(
        gauge_points(find(&metrics, "dorfq_queue_blocked_prs")),
        [(Vec::new(), Number::Int(2))]
    );
    assert_eq!(
        gauge_points(find(&metrics, "dorfq_api_rate_limit_remaining")),
        [(Vec::new(), Number::Int(10))]
    );
    for (name, value) in [
        ("dorfq_telemetry_export_failures_total", 4),
        ("dorfq_telemetry_events_dropped_total", 9),
    ] {
        assert_eq!(
            find(&metrics, name).data,
            Data::Sum {
                start_unix_nano: 100_000_000_000,
                points: vec![Point {
                    attributes: Vec::new(),
                    time_unix_nano: 2_000_000_000_000,
                    value: Number::Int(value),
                }],
            },
            "{name}"
        );
    }
}

#[test]
fn metrics_omit_what_is_unknown() {
    let mut document = document();
    document.figures = Figures::default();
    let without_forge = metrics(&snapshot(&document, 0, None), None, &counters(), 0);
    let present = names(&without_forge);
    for absent in [
        "dorfq_tick_age_seconds",
        "dorfq_queue_blocked_prs",
        "dorfq_api_rate_limit_remaining",
    ] {
        assert!(!present.contains(&absent), "{absent}");
    }
    // A quantile without samples is a metric without points; the fixed
    // series are still all there, at zero.
    assert!(gauge_points(find(&without_forge, "dorfq_window_batch_run_seconds")).is_empty());
    assert!(gauge_points(find(&without_forge, "dorfq_window_land_seconds")).is_empty());
    assert_eq!(
        gauge_points(find(&without_forge, "dorfq_window_runs")).len(),
        3
    );
    assert_eq!(
        gauge_points(find(&without_forge, "dorfq_window_landed_prs")),
        [(vec![window()], Number::Int(0))]
    );

    let unknown_limit = metrics(
        &snapshot(&document, 0, Some(gauges(None))),
        None,
        &counters(),
        0,
    );
    let present = names(&unknown_limit);
    assert!(present.contains(&"dorfq_queue_blocked_prs"));
    assert!(!present.contains(&"dorfq_api_rate_limit_remaining"));
}

/// The record of `kind` at forge time 5, observed at host time 9.
fn record_of(kind: Kind) -> (Event, LogRecord) {
    let event = Event { t: 5, kind };
    let record = record(&event, 9);
    (event, record)
}

#[test]
fn values_past_the_wire_range_saturate() {
    let mut snapshot = snapshot(&document(), 1_000, None);
    snapshot.oldest_queued_seconds = u64::MAX;
    let counters = Counters {
        started_at: u64::MAX,
        ..counters()
    };
    let metrics = metrics(&snapshot, None, &counters, u64::MAX);
    assert_eq!(
        gauge_points(find(&metrics, "dorfq_oldest_queued_seconds")),
        vec![(Vec::new(), Number::Int(i64::MAX))]
    );
    let Data::Sum {
        start_unix_nano,
        points,
    } = &find(&metrics, "dorfq_telemetry_export_failures_total").data
    else {
        panic!("not a sum");
    };
    assert_eq!(*start_unix_nano, u64::MAX);
    assert_eq!(points[0].time_unix_nano, u64::MAX);

    let event = Event {
        t: u64::MAX,
        kind: Kind::Arrive {
            pr: u64::MAX,
            base: None,
            atomic: false,
            head: Sha::parse(&"ab".repeat(20)).unwrap(),
        },
    };
    let record = record(&event, u64::MAX);
    assert_eq!(record.time_unix_nano, u64::MAX);
    assert_eq!(record.observed_time_unix_nano, u64::MAX);
    assert_eq!(
        record.attributes,
        vec![s("kind", "arrive"), ("pr".to_owned(), Attr::Int(i64::MAX))]
    );
}

#[test]
fn record_carries_the_event_line_and_its_numbers() {
    let cases = [
        (
            Kind::Arrive {
                pr: 4,
                base: None,
                atomic: false,
                head: Sha::parse(&"ab".repeat(20)).unwrap(),
            },
            vec![s("kind", "arrive"), ("pr".to_owned(), Attr::Int(4))],
            (9, "INFO"),
        ),
        (
            Kind::Conflict {
                batch: 2,
                pr: 4,
                with: Value::from("trunk"),
            },
            vec![
                s("kind", "conflict"),
                ("pr".to_owned(), Attr::Int(4)),
                ("batch".to_owned(), Attr::Int(2)),
            ],
            (9, "INFO"),
        ),
        (
            Kind::Trunk {
                position: 3,
                sha: "cd".to_owned(),
                landed: vec![4],
                foreign: 0,
            },
            vec![s("kind", "trunk")],
            (9, "INFO"),
        ),
        (
            Kind::Error {
                message: "the forge refused".to_owned(),
                fatal: false,
                at: 7,
            },
            vec![s("kind", "error")],
            (17, "ERROR"),
        ),
        (
            Kind::Error {
                message: "the journal is unwritable".to_owned(),
                fatal: true,
                at: 7,
            },
            vec![s("kind", "error")],
            (21, "FATAL"),
        ),
    ];
    for (kind, attributes, (severity_number, severity_text)) in cases {
        let (event, record) = record_of(kind);
        let line = events::line(&event);
        assert_eq!(
            record,
            LogRecord {
                time_unix_nano: 5_000_000_000,
                observed_time_unix_nano: 9_000_000_000,
                severity_number,
                severity_text,
                body: line.strip_suffix('\n').unwrap().to_owned(),
                attributes,
            }
        );
    }
}

type Requests = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// A sink that records every request and refuses each while `refusing` is
/// set.
fn recording(refusing: &Arc<AtomicBool>) -> (Sink, Requests) {
    let requests = Requests::default();
    let kept = Arc::clone(&requests);
    let refusing = Arc::clone(refusing);
    let sink = Box::new(move |path: &str, body: &[u8]| {
        kept.lock().unwrap().push((path.to_owned(), body.to_vec()));
        if refusing.load(Ordering::SeqCst) {
            Err("refused".to_owned())
        } else {
            Ok(())
        }
    });
    (sink, requests)
}

/// `n` events, the `i`th a `pr_ci` of pull request `i` at forge time `i`.
fn ci_events(n: u64) -> Vec<Event> {
    (0..n)
        .map(|pr| Event {
            t: pr,
            kind: Kind::PrCi { pr },
        })
        .collect()
}

fn tick(at: u64, events: Vec<Event>, snapshot: Option<Snapshot>) -> Tick {
    Tick {
        at,
        events,
        snapshot,
        observed: true,
    }
}

fn paths_of(requests: &[(String, Vec<u8>)]) -> Vec<&str> {
    requests.iter().map(|(path, _)| path.as_str()).collect()
}

fn json(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("a request body is JSON")
}

/// The metric `name` of a `/v1/metrics` body.
fn metric_in(body: &Value, name: &str) -> Value {
    body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|metric| metric["name"] == name)
        .unwrap_or_else(|| panic!("no metric {name}"))
        .clone()
}

/// The single value of the sum `name` in a `/v1/metrics` body.
fn sum_in(body: &Value, name: &str) -> String {
    metric_in(body, name)["sum"]["dataPoints"][0]["asInt"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The log record bodies of a `/v1/logs` body.
fn log_bodies(body: &Value) -> Vec<String> {
    body["resourceLogs"][0]["scopeLogs"][0]["logRecords"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["body"]["stringValue"].as_str().unwrap().to_owned())
        .collect()
}

fn lines(notes: &[u8]) -> Vec<String> {
    String::from_utf8(notes.to_vec())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_exporter_exports_on_its_interval_while_the_queue_idles() {
    let (sink, requests) = recording(&Arc::default());
    let exporter = Exporter::start(Duration::from_millis(50), unix_now(), sink);
    exporter.tick(tick(
        unix_now(),
        Vec::new(),
        Some(snapshot(&document(), 1_000, None)),
    ));
    let started = Instant::now();
    thread::sleep(Duration::from_millis(230));
    let elapsed = started.elapsed();
    let exported: Vec<Value> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|(path, body)| {
            assert_eq!(path, "/v1/metrics");
            json(body)
        })
        .collect();
    assert!(exported.len() >= 2, "{} exports", exported.len());
    // One export per interval at most: a timer that never advanced would
    // export in a tight loop.
    let most = usize::try_from(elapsed.as_millis() / 50).unwrap() + 1;
    assert!(
        exported.len() <= most,
        "{} exports in {elapsed:?}",
        exported.len()
    );
    let times: Vec<u64> = exported
        .iter()
        .map(|body| {
            let age = metric_in(body, "dorfq_tick_age_seconds");
            age["gauge"]["dataPoints"][0]["timeUnixNano"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    assert!(times.is_sorted(), "{times:?}");
    drop(exporter);
}

#[test]
fn nothing_is_sent_before_a_snapshot_or_an_event() {
    let (sink, requests) = recording(&Arc::default());
    let exporter = Exporter::start(Duration::from_millis(10), 0, sink);
    exporter.tick(Tick {
        at: 1,
        events: Vec::new(),
        snapshot: None,
        observed: false,
    });
    thread::sleep(Duration::from_millis(60));
    drop(exporter);
    assert!(paths_of(&requests.lock().unwrap()).is_empty());
}

#[test]
fn dropping_the_exporter_exports_what_is_pending() {
    let (sink, requests) = recording(&Arc::default());
    let exporter = Exporter::start(Duration::from_secs(3600), 0, sink);
    exporter.tick(tick(7, ci_events(2), None));
    drop(exporter);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, "/v1/logs");
    assert_eq!(log_bodies(&json(&requests[0].1)).len(), 2);
}

#[test]
fn a_tick_with_events_and_no_snapshot_sends_logs_only() {
    let (mut sink, requests) = recording(&Arc::default());
    let mut pump = Pump::new(Duration::from_secs(60), 0);
    let mut notes = Vec::new();
    pump.take(tick(7, ci_events(3), None), &mut notes);
    pump.export(8, &mut sink, &mut notes);
    assert_eq!(paths_of(&requests.lock().unwrap()), ["/v1/logs"]);
    let body = json(&requests.lock().unwrap()[0].1);
    let record = &body["resourceLogs"][0]["scopeLogs"][0]["logRecords"][2];
    assert_eq!(record["timeUnixNano"], "2000000000");
    assert_eq!(record["observedTimeUnixNano"], "7000000000");
    assert!(notes.is_empty());
}

#[test]
fn a_failed_export_is_counted_said_once_and_its_events_kept() {
    let refusing = Arc::new(AtomicBool::new(true));
    let (mut sink, requests) = recording(&refusing);
    let mut pump = Pump::new(Duration::from_secs(60), 0);
    let mut notes = Vec::new();
    pump.take(
        tick(7, ci_events(2), Some(snapshot(&document(), 1_000, None))),
        &mut notes,
    );
    pump.export(8, &mut sink, &mut notes);
    pump.export(9, &mut sink, &mut notes);
    assert_eq!(pump.counters.export_failures, 4);
    assert_eq!(
        lines(&notes),
        [
            "dorfq: telemetry: export to /v1/metrics failed: refused; retrying every 60s",
            "dorfq: telemetry: export to /v1/logs failed: refused; retrying every 60s",
        ]
    );

    refusing.store(false, Ordering::SeqCst);
    requests.lock().unwrap().clear();
    pump.export(10, &mut sink, &mut notes);
    let requests = requests.lock().unwrap();
    assert_eq!(paths_of(&requests), ["/v1/metrics", "/v1/logs"]);
    let metrics = json(&requests[0].1);
    assert_eq!(
        sum_in(&metrics, "dorfq_telemetry_export_failures_total"),
        "4"
    );
    assert_eq!(
        sum_in(&metrics, "dorfq_telemetry_events_dropped_total"),
        "0"
    );
    assert_eq!(log_bodies(&json(&requests[1].1)).len(), 2);
    assert!(pump.pending.is_empty());
    assert_eq!(
        lines(&notes)[2..],
        [
            "dorfq: telemetry: export to /v1/metrics resumed after 2 failed requests",
            "dorfq: telemetry: export to /v1/logs resumed after 2 failed requests",
        ]
    );
}

#[test]
fn a_path_refused_while_the_other_delivers_is_said_once() {
    let mut sink: Sink = Box::new(|path: &str, _: &[u8]| match path {
        "/v1/logs" => Err("HTTP 404: ".to_owned()),
        _ => Ok(()),
    });
    let mut pump = Pump::new(Duration::from_secs(60), 0);
    let mut notes = Vec::new();
    let buffer = u64::try_from(EVENT_BUFFER).unwrap();
    pump.take(
        tick(
            7,
            ci_events(buffer + 1),
            Some(snapshot(&document(), 1_000, None)),
        ),
        &mut notes,
    );
    pump.export(8, &mut sink, &mut notes);
    for at in [9, 11] {
        pump.take(tick(at, ci_events(1), None), &mut notes);
        pump.export(at + 1, &mut sink, &mut notes);
    }
    assert_eq!(pump.counters.export_failures, 3);
    assert_eq!(pump.counters.events_dropped, 3);
    assert_eq!(
        lines(&notes),
        [
            "dorfq: telemetry: 10000 events pending; dropping the oldest until the collector takes them",
            "dorfq: telemetry: export to /v1/logs failed: HTTP 404: ; retrying every 60s",
        ]
    );
}

#[test]
fn drops_without_a_failure_are_said_when_the_events_go_out() {
    let (mut sink, _requests) = recording(&Arc::default());
    let mut pump = Pump::new(Duration::from_secs(60), 0);
    let mut notes = Vec::new();
    let buffer = u64::try_from(EVENT_BUFFER).unwrap();
    pump.take(tick(7, ci_events(buffer + 2), None), &mut notes);
    pump.export(8, &mut sink, &mut notes);
    assert!(pump.pending.is_empty());
    // A later overflow is said again.
    pump.take(tick(9, ci_events(buffer + 1), None), &mut notes);
    pump.export(10, &mut sink, &mut notes);
    assert_eq!(
        lines(&notes),
        [
            "dorfq: telemetry: 10000 events pending; dropping the oldest until the collector takes them",
            "dorfq: telemetry: 2 events dropped while the buffer was full",
            "dorfq: telemetry: 10000 events pending; dropping the oldest until the collector takes them",
            "dorfq: telemetry: 1 events dropped while the buffer was full",
        ]
    );
}

#[test]
fn the_buffer_keeps_the_newest_events_and_counts_the_dropped() {
    let refusing = Arc::new(AtomicBool::new(true));
    let (mut sink, requests) = recording(&refusing);
    let mut pump = Pump::new(Duration::from_secs(60), 0);
    let mut notes = Vec::new();
    let buffer = u64::try_from(EVENT_BUFFER).unwrap();
    pump.take(
        tick(
            7,
            ci_events(buffer + 1),
            Some(snapshot(&document(), 1_000, None)),
        ),
        &mut notes,
    );
    pump.export(8, &mut sink, &mut notes);
    assert_eq!(pump.pending.len(), EVENT_BUFFER);
    assert_eq!(pump.counters.events_dropped, 1);

    refusing.store(false, Ordering::SeqCst);
    requests.lock().unwrap().clear();
    pump.export(9, &mut sink, &mut notes);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1 + EVENT_BUFFER / LOGS_PER_REQUEST);
    assert_eq!(
        sum_in(
            &json(&requests[0].1),
            "dorfq_telemetry_events_dropped_total"
        ),
        "1"
    );
    // The oldest, pull request 0's, is the one dropped.
    assert_eq!(
        log_bodies(&json(&requests[1].1))[0],
        r#"{"t":1,"kind":"pr_ci","pr":1}"#
    );
    assert_eq!(
        lines(&notes),
        [
            "dorfq: telemetry: 10000 events pending; dropping the oldest until the collector takes them",
            "dorfq: telemetry: export to /v1/metrics failed: refused; retrying every 60s",
            "dorfq: telemetry: export to /v1/logs failed: refused; retrying every 60s",
            "dorfq: telemetry: export to /v1/metrics resumed after 1 failed requests",
            "dorfq: telemetry: export to /v1/logs resumed after 1 failed requests, 1 events dropped",
        ]
    );
}

#[test]
fn pending_events_go_out_in_chunks() {
    let (mut sink, requests) = recording(&Arc::default());
    let mut pump = Pump::new(Duration::from_secs(60), 0);
    let mut notes = Vec::new();
    pump.take(tick(7, ci_events(2_500), None), &mut notes);
    pump.export(8, &mut sink, &mut notes);
    let sizes: Vec<usize> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|(_, body)| log_bodies(&json(body)).len())
        .collect();
    assert_eq!(sizes, [1_000, 1_000, 500]);
    assert!(pump.pending.is_empty());
}

#[test]
fn a_refused_chunk_stops_the_export_and_keeps_the_rest() {
    let refusing = Arc::new(AtomicBool::new(true));
    let (mut sink, requests) = recording(&refusing);
    let mut pump = Pump::new(Duration::from_secs(60), 0);
    let mut notes = Vec::new();
    pump.take(tick(7, ci_events(2_500), None), &mut notes);
    pump.export(8, &mut sink, &mut notes);
    assert_eq!(paths_of(&requests.lock().unwrap()), ["/v1/logs"]);
    assert_eq!(pump.pending.len(), 2_500);
}

#[test]
fn tick_age_measures_from_the_last_observing_tick() {
    let (mut sink, requests) = recording(&Arc::default());
    let mut pump = Pump::new(Duration::from_secs(60), 0);
    let mut notes = Vec::new();
    pump.take(
        tick(100, Vec::new(), Some(snapshot(&document(), 1_000, None))),
        &mut notes,
    );
    pump.take(
        Tick {
            at: 130,
            events: Vec::new(),
            snapshot: None,
            observed: false,
        },
        &mut notes,
    );
    pump.export(145, &mut sink, &mut notes);
    let body = json(&requests.lock().unwrap()[0].1);
    let age = metric_in(&body, "dorfq_tick_age_seconds");
    assert_eq!(age["gauge"]["dataPoints"][0]["asInt"], "45");
    // The snapshot is kept across a tick that brought none.
    assert!(pump.latest.is_some());
}

#[test]
fn http_sink_refuses_an_authorization_variable_that_is_unset() {
    let config = TelemetryConfig {
        otlp_endpoint: "http://127.0.0.1:4318".to_owned(),
        interval_secs: 60,
        authorization_env: Some("DORFQ_TEST_OTLP_AUTHORIZATION_NEVER_SET".to_owned()),
    };
    let error = http_sink(&config).err().unwrap();
    assert_eq!(
        error.to_string(),
        "telemetry.authorization_env DORFQ_TEST_OTLP_AUTHORIZATION_NEVER_SET is unset, empty or not UTF-8"
    );
    let unauthorised = TelemetryConfig {
        authorization_env: None,
        ..config
    };
    assert!(http_sink(&unauthorised).is_ok());
}

#[test]
fn an_authorization_value_no_header_can_carry_is_refused_without_echoing_it() {
    let error = authorization_header("AUTH", "Bearer secret\n").unwrap_err();
    assert_eq!(
        error.to_string(),
        "telemetry.authorization_env AUTH holds a value that is not a valid HTTP header value"
    );
    assert!(authorization_header("AUTH", "Bearer secret").is_ok());
}

/// Serves one connection on a loopback port: reads one request, answers
/// `response`, and yields the request's head and body.
fn serve_once(response: &'static str) -> (String, thread::JoinHandle<(String, Vec<u8>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut received = Vec::new();
        let mut buffer = [0; 4096];
        let head_end = loop {
            let n = stream.read(&mut buffer).unwrap();
            assert!(n > 0, "the request ended inside its head");
            received.extend_from_slice(&buffer[..n]);
            if let Some(at) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
        };
        let head = String::from_utf8(received[..head_end].to_vec()).unwrap();
        let length: usize = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap_or(0);
        while received.len() < head_end + length {
            let n = stream.read(&mut buffer).unwrap();
            assert!(n > 0, "the request ended inside its body");
            received.extend_from_slice(&buffer[..n]);
        }
        stream.write_all(response.as_bytes()).unwrap();
        (head, received[head_end..].to_vec())
    });
    (endpoint, server)
}

#[test]
fn the_http_sink_posts_json_with_the_authorization_header() {
    let (endpoint, server) =
        serve_once("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
    let authorization = authorization_header("AUTH", "Basic dTpw").unwrap();
    let mut sink = post_to(format!("{endpoint}/otlp"), Some(authorization));
    assert_eq!(sink("/v1/logs", b"{\"a\":1}"), Ok(()));
    let (head, body) = server.join().unwrap();
    let head = head.to_ascii_lowercase();
    assert!(
        head.starts_with("post /otlp/v1/logs http/1.1\r\n"),
        "{head}"
    );
    assert!(
        head.contains("\r\ncontent-type: application/json\r\n"),
        "{head}"
    );
    assert!(head.contains("\r\nauthorization: basic dtpw\r\n"), "{head}");
    assert_eq!(body, b"{\"a\":1}");
}

#[test]
fn the_http_sink_reports_a_refusal_with_its_status_and_body_on_one_line() {
    let page = format!("<html>\r\n<title>busy</title>\r\n{}", "x".repeat(300));
    let response: &'static str = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    )
    .leak();
    let (endpoint, server) = serve_once(response);
    let mut sink = post_to(endpoint, None);
    let expected = format!("HTTP 503: <html> <title>busy</title> {}", "x".repeat(171));
    assert_eq!(sink("/v1/metrics", b"{}"), Err(expected));
    server.join().unwrap();
}

#[test]
fn the_http_sink_does_not_follow_a_redirect() {
    let (endpoint, server) = serve_once(
        "HTTP/1.1 302 Found\r\nLocation: /elsewhere\r\nContent-Length: 5\r\nConnection: close\r\n\r\nmoved",
    );
    let mut sink = post_to(endpoint, None);
    // A followed redirect would reach a closed port and fail otherwise.
    assert_eq!(
        sink("/v1/metrics", b"{}"),
        Err("HTTP 302: moved".to_owned())
    );
    server.join().unwrap();
}

#[test]
fn the_http_sink_reports_a_collector_that_is_not_there() {
    let endpoint = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let mut sink = post_to(endpoint, None);
    let error = sink("/v1/metrics", b"{}").unwrap_err();
    assert!(!error.starts_with("HTTP "), "{error}");
}
