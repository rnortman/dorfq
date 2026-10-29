use dorfq_core::{
    BatchState, CheckName, CiStatus, ConflictWith, Failure, Hold, LandRefusal, PrNumber, PrStatus,
    Sha,
};
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
            head: "ab".to_owned(),
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
                head: "ab".to_owned(),
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
