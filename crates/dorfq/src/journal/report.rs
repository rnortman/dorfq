//! The report document: a journal's events and summary in the simulation
//! page's shape, so the page's file picker opens it as it opens a
//! simulator report.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::{Map, Value};

use super::Header;
use super::events::Event;
use super::summary::Summary;

/// The simulation page's document schema this document follows.
pub const SCHEMA: u32 = 4;

#[derive(Serialize)]
struct Document<'a> {
    schema: u32,
    /// The forge time the trace's `t` counts from.
    t0: u64,
    name: &'a str,
    config: Config,
    config_text: String,
    policies: [Policy<'a>; 1],
}

#[derive(Serialize)]
struct Config {
    policy: [Value; 1],
}

#[derive(Serialize)]
struct Policy<'a> {
    name: &'a str,
    replications: [Replication<'a>; 1],
    trace: Vec<Event>,
}

#[derive(Serialize)]
struct Replication<'a> {
    metrics: &'a Summary,
}

#[derive(Serialize)]
struct SchedulerTable<'a> {
    scheduler: &'a Map<String, Value>,
}

// TODO(status-document): the pool now and a window of events as one
// document, rendered to an issue and published for the page's live mode.
/// The document for the journal `segments`, named `name`, whose first
/// header is `header`: one policy, `name`, running the header's scheduler,
/// with `summary` as its one replication and `events` as its trace, each
/// event's `t` counted from the first. Fails when the header's scheduler
/// table holds a value TOML cannot render.
pub fn document(
    name: &str,
    segments: &[PathBuf],
    header: &Header,
    events: &[Event],
    summary: &Summary,
) -> Result<String, toml::ser::Error> {
    let t0 = events.first().map_or(0, |e| e.t);
    let trace = events
        .iter()
        .map(|e| Event {
            t: e.t.saturating_sub(t0),
            kind: e.kind.clone(),
        })
        .collect();
    let mut policy = header.scheduler.clone();
    policy.insert("name".to_owned(), Value::from(name));
    policy.insert("admission".to_owned(), Value::from("label"));
    let table = toml::to_string(&SchedulerTable {
        scheduler: &header.scheduler,
    })?;
    let names: Vec<String> = segments.iter().map(|p| p.display().to_string()).collect();
    let config_text = format!(
        "# The journal {}, {:.2} hours from forge time {t0}.\n# Its scheduler:\n{table}",
        names.join(" "),
        summary.window_hours,
    );
    let document = Document {
        schema: SCHEMA,
        t0,
        name,
        config: Config {
            policy: [Value::Object(policy)],
        },
        config_text,
        policies: [Policy {
            name,
            replications: [Replication { metrics: summary }],
            trace,
        }],
    };
    Ok(serde_json::to_string(&document).expect("a report document serialises"))
}

#[cfg(test)]
mod tests;
