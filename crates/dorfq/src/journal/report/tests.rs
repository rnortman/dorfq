use std::path::PathBuf;

use dorfq_scheduler::SchedulerConfig;
use serde::Deserialize;
use serde_json::Value;

use super::super::Header;
use super::super::events::{Event, Kind};
use super::super::summary::summarise;
use super::*;

fn events() -> Vec<Event> {
    vec![
        Event {
            t: 1_000,
            kind: Kind::Arrive {
                pr: 1,
                base: None,
                atomic: false,
                head: format!("{:040x}", 1),
            },
        },
        Event {
            t: 1_000,
            kind: Kind::Admit { pr: 1, base: None },
        },
        Event {
            t: 4_600,
            kind: Kind::Withdraw {
                pr: 1,
                how: dorfq_core::Withdrawal::Closed,
            },
        },
    ]
}

fn report() -> Value {
    let scheduler = SchedulerConfig::Bisect {
        lanes: 2,
        max_batch: 4,
    };
    let header = Header::start("mock", &scheduler, 7);
    let events = events();
    let summary = summarise(&events);
    let segments = [PathBuf::from("q.journal.7"), PathBuf::from("q.journal")];
    let text = document("nightly", &segments, &header, &events, &summary)
        .expect("the scheduler table renders as TOML");
    serde_json::from_str(&text).expect("the document is JSON")
}

#[test]
fn the_document_is_in_the_pages_shape() {
    let doc = report();
    assert_eq!(doc["schema"], Value::from(dorfq_sim::SCHEMA));
    assert_eq!(SCHEMA, dorfq_sim::SCHEMA);
    assert_eq!(doc["t0"], 1_000);
    assert_eq!(doc["name"], "nightly");
    let policy = &doc["config"]["policy"][0];
    assert_eq!(policy["kind"], "bisect");
    assert_eq!(policy["lanes"], 2);
    assert_eq!(policy["max_batch"], 4);
    assert_eq!(policy["admission"], "label");
    assert_eq!(policy["name"], "nightly");
    let policies = doc["policies"].as_array().unwrap();
    assert_eq!(policies.len(), 1);
    assert_eq!(policies[0]["name"], "nightly");
    let trace = policies[0]["trace"].as_array().unwrap();
    assert_eq!(trace[0]["t"], 0);
    assert_eq!(trace[0]["kind"], "arrive");
    assert_eq!(trace[2]["t"], 3_600);
    let metrics = &policies[0]["replications"][0]["metrics"];
    let summary = serde_json::to_value(summarise(&events())).unwrap();
    for name in summary.as_object().unwrap().keys() {
        assert!(metrics.get(name).is_some(), "{name}");
    }
    assert_eq!(metrics["withdrawn"], 1);
}

#[test]
fn the_config_text_names_the_segments_and_holds_the_scheduler_table() {
    let doc = report();
    let text = doc["config_text"].as_str().unwrap();
    assert!(text.contains("q.journal.7 q.journal"), "{text}");
    assert!(text.contains("1.00 hours"), "{text}");
    #[derive(Deserialize)]
    struct Table {
        scheduler: SchedulerConfig,
    }
    let table: Table = toml::from_str(text).expect("the text is TOML");
    assert_eq!(
        table.scheduler,
        SchedulerConfig::Bisect {
            lanes: 2,
            max_batch: 4
        }
    );
}

#[test]
fn a_scheduler_table_toml_cannot_render_is_an_error() {
    let mut header = Header::start("mock", &SchedulerConfig::AllAtOnce {}, 7);
    header.scheduler.insert("unset".to_owned(), Value::Null);
    let events = events();
    let summary = summarise(&events);
    assert!(document("nightly", &[], &header, &events, &summary).is_err());
}
