//! Telemetry: the queue's gauges, derived from the status document, and its
//! events, one log record each, as OTLP/JSON request bodies.
//!
//! A gauge's time is the host time of the export that carries it; a log
//! record's is its event's forge time, its observed time the host time of
//! the tick that produced it. The window figures are the status document's:
//! what ended in the trailing [`WINDOW`] seconds of forge time.

pub mod otlp;

use serde_json::Value;

use crate::journal::events::{self, Event, Kind};
use crate::journal::status::{Document, Figures, WINDOW};
use crate::journal::summary::HOUR;
use crate::providers::ForgeGauges;
use otlp::{Attr, Data, LogRecord, Metric, Number, Point, Resource};

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

#[cfg(test)]
mod tests;
