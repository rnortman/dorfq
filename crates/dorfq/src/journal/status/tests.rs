use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use dorfq_core::{
    BatchId, BatchMember, BatchState, BatchStatus, CiStatus, Hold, Observation, Plan, PrBase,
    PrNumber, PrSnapshot, PrStatus, Sha, Timestamp, Withdrawal,
};
use dorfq_scheduler::SchedulerConfig;
use serde_json::Value;

use super::*;
use crate::driver::TickReport;
use crate::journal::{Header, Writer};

/// 2023-11-14 00:00 UTC.
const DAY: u64 = 1_699_920_000;

fn sha(n: u64) -> Sha {
    Sha::parse(&format!("{n:040x}")).unwrap()
}

fn pr(n: u64, status: PrStatus) -> PrSnapshot {
    PrSnapshot::new(PrNumber(n), sha(n), PrBase::Trunk, status, 0)
}

fn ready(n: u64) -> PrSnapshot {
    pr(n, PrStatus::Ready)
}

fn waiting(n: u64, hold: Hold) -> PrSnapshot {
    pr(n, PrStatus::Waiting(BTreeSet::from([hold])))
}

fn testing(id: u64, members: &[u64]) -> BatchStatus {
    BatchStatus::new(
        BatchId(id),
        sha(100),
        members
            .iter()
            .map(|&m| BatchMember::new(PrNumber(m), sha(m)))
            .collect(),
        BatchState::Testing {
            tip: sha(200 + id),
            ci: CiStatus::Pending,
        },
    )
}

fn observation(now: u64, prs: Vec<PrSnapshot>, batches: Vec<BatchStatus>) -> Observation {
    Observation::new(Timestamp(now), sha(1000), 0, prs, batches, Vec::new())
}

fn event(t: u64, kind: Kind) -> Event {
    Event { t, kind }
}

fn arrive(t: u64, pr: u64) -> Event {
    event(
        t,
        Kind::Arrive {
            pr,
            base: None,
            atomic: false,
            head: sha(pr).to_string(),
        },
    )
}

fn start(t: u64, batch: u64, members: &[u64]) -> Event {
    event(
        t,
        Kind::Start {
            batch,
            base: None,
            members: members.to_vec(),
            probe: false,
        },
    )
}

fn run_end(t: u64, batch: u64, passed: bool) -> Event {
    let checks = if passed {
        Vec::new()
    } else {
        vec!["build".to_owned()]
    };
    event(
        t,
        Kind::RunEnd {
            batch,
            passed,
            checks,
        },
    )
}

fn land(t: u64, batch: u64, members: &[u64]) -> Event {
    event(
        t,
        Kind::Land {
            batch,
            members: members.to_vec(),
        },
    )
}

fn eject(t: u64, pr: u64) -> Event {
    event(
        t,
        Kind::Eject {
            pr,
            reason: Some("its CI failed".to_owned()),
            r#final: true,
        },
    )
}

fn retire(t: u64, batch: u64, cancelled: bool) -> Event {
    event(t, Kind::Retire { batch, cancelled })
}

fn step(t: u64) -> Event {
    event(
        t,
        Kind::Step {
            queued: 0,
            in_flight: 0,
            land: Vec::new(),
            notify: Vec::new(),
            start: Vec::new(),
            eject: Vec::new(),
            retire: Vec::new(),
            at: t,
            tick: 0,
        },
    )
}

/// One pull request arrived and ready at `DAY`, then waiting on its own CI
/// at `DAY + 600`.
fn one_queued() -> Status {
    let mut status = Status::default();
    status.tick(
        Some(&observation(DAY, vec![ready(1)], Vec::new())),
        &[arrive(DAY, 1), step(DAY)],
    );
    status.tick(
        Some(&observation(
            DAY + 600,
            vec![waiting(1, Hold::OwnCiPending)],
            Vec::new(),
        )),
        &[step(DAY + 600)],
    );
    status
}

#[test]
fn an_arrival_sets_arrived_and_since_moves_only_when_the_status_text_changes() {
    let mut status = one_queued();
    let queued = &status.document().prs[0];
    assert_eq!((queued.arrived, queued.since), (DAY, DAY + 600));
    assert_eq!(queued.status, "waiting:own CI pending");

    // The same status text at a later observation keeps `since`.
    status.tick(
        Some(&observation(
            DAY + 900,
            vec![waiting(1, Hold::OwnCiPending)],
            Vec::new(),
        )),
        &[step(DAY + 900)],
    );
    let queued = &status.document().prs[0];
    assert_eq!((queued.arrived, queued.since), (DAY, DAY + 600));
}

#[test]
fn departures_remove_pull_requests_and_retire_removes_batches() {
    let mut status = Status::default();
    let arrivals: Vec<Event> = (1..=6).map(|n| arrive(DAY, n)).collect();
    status.tick(
        Some(&observation(DAY, (1..=6).map(ready).collect(), Vec::new())),
        &arrivals,
    );
    status.tick(
        Some(&observation(
            DAY + 60,
            vec![ready(6)],
            vec![testing(1, &[6])],
        )),
        &[
            event(DAY + 60, Kind::Merged { pr: 1 }),
            event(
                DAY + 60,
                Kind::Withdraw {
                    pr: 2,
                    how: Withdrawal::Closed,
                },
            ),
            event(
                DAY + 60,
                Kind::Blocked {
                    pr: 3,
                    on: 2,
                    r#final: true,
                },
            ),
            event(DAY + 60, Kind::Lost { pr: 4 }),
            eject(DAY + 60, 5),
            start(DAY + 60, 1, &[6]),
        ],
    );
    let document = status.document();
    let numbers: Vec<u64> = document.prs.iter().map(|p| p.number).collect();
    assert_eq!(numbers, [6]);
    assert_eq!(document.prs[0].batches, [1]);
    assert_eq!(status.prs.keys().copied().collect::<Vec<_>>(), [6]);
    assert_eq!(document.batches[0].started, DAY + 60);
    assert_eq!(document.batches[0].state, "testing");

    status.tick(
        Some(&observation(DAY + 120, vec![ready(6)], Vec::new())),
        &[retire(DAY + 120, 1, true)],
    );
    assert!(status.batches.is_empty());
    assert!(status.document().batches.is_empty());
}

#[test]
fn landings_and_run_ends_are_sampled_and_ejections_only_counted() {
    let mut status = Status::default();
    status.tick(
        Some(&observation(
            DAY,
            vec![ready(1), ready(2), ready(3)],
            Vec::new(),
        )),
        &[arrive(DAY, 1), arrive(DAY, 2), arrive(DAY, 3)],
    );
    status.tick(
        Some(&observation(
            DAY + 3600,
            vec![ready(1), ready(2), ready(3)],
            vec![testing(1, &[1, 2]), testing(2, &[3])],
        )),
        &[start(DAY + 3600, 1, &[1, 2]), start(DAY + 3600, 2, &[3])],
    );
    status.tick(
        Some(&observation(DAY + 3 * 3600, Vec::new(), Vec::new())),
        &[
            run_end(DAY + 3 * 3600, 1, true),
            land(DAY + 3 * 3600, 1, &[1, 2]),
            run_end(DAY + 3 * 3600, 2, false),
            eject(DAY + 3 * 3600, 3),
            retire(DAY + 3 * 3600, 1, false),
            retire(DAY + 3 * 3600, 2, false),
            event(
                DAY + 3 * 3600,
                Kind::Refused {
                    batch: 9,
                    reason: "trunk moved".to_owned(),
                },
            ),
        ],
    );
    assert_eq!(
        status.document().figures,
        Figures {
            landed_prs: 2,
            landed_batches: 1,
            ejected: 1,
            refused: 1,
            passed_runs: 1,
            failed_runs: 1,
            cancelled_runs: 0,
            batch_run_p50_hours: Some(2.0),
            batch_run_p95_hours: Some(2.0),
            land_p50_hours: Some(3.0),
            land_p95_hours: Some(3.0),
        }
    );
    assert_eq!(status.runs.len(), 2);
    assert_eq!(status.lands.len(), 2);
}

#[test]
fn the_figures_agree_with_the_summary_except_departures_with_a_base() {
    let stream = crate::journal::summary::tests::stream();
    let summary = crate::journal::summary::summarise(&stream);
    let mut status = Status::default();
    status.tick(None, &stream);
    let f = status.document().figures;
    assert_eq!(f.landed_prs, summary.landed);
    assert_eq!(f.refused, summary.refused_landings);
    assert_eq!(
        (f.passed_runs, f.failed_runs, f.cancelled_runs),
        (
            summary.passed_runs,
            summary.failed_runs,
            summary.cancelled_runs
        )
    );
    assert_eq!(
        (f.batch_run_p50_hours, f.batch_run_p95_hours),
        (summary.batch_run_p50_hours, summary.batch_run_p95_hours)
    );
    assert_eq!(
        (f.land_p50_hours, f.land_p95_hours),
        (summary.land_p50_hours, summary.land_p95_hours)
    );
    // The summary also counts #2, which left with its ejected base #4.
    assert_eq!(f.ejected, summary.ejected - summary.left_with_base);
}

#[test]
fn the_window_keeps_the_shown_kinds_and_evicts_after_24_hours() {
    let mut status = Status::default();
    let o = observation(DAY, vec![ready(1)], Vec::new());
    let shown = [
        land(DAY, 1, &[7]),
        eject(DAY, 7),
        event(
            DAY,
            Kind::Refused {
                batch: 1,
                reason: "r".to_owned(),
            },
        ),
        event(
            DAY,
            Kind::Notice {
                pr: 1,
                reason: "r".to_owned(),
            },
        ),
        event(DAY, Kind::Merged { pr: 7 }),
        event(
            DAY,
            Kind::Withdraw {
                pr: 7,
                how: Withdrawal::Closed,
            },
        ),
        event(
            DAY,
            Kind::Blocked {
                pr: 7,
                on: 1,
                r#final: true,
            },
        ),
        event(DAY, Kind::Lost { pr: 7 }),
        run_end(DAY, 1, false),
        event(
            DAY,
            Kind::Conflict {
                batch: 1,
                pr: 1,
                with: Value::from("trunk"),
            },
        ),
        retire(DAY, 1, true),
        event(
            DAY,
            Kind::Trunk {
                position: 3,
                sha: sha(1).to_string(),
                landed: Vec::new(),
                foreign: 2,
            },
        ),
        event(
            DAY,
            Kind::Restart {
                started: DAY,
                dorfq: "0.1.0".to_owned(),
                at: DAY,
            },
        ),
        event(
            DAY,
            Kind::Error {
                message: "m".to_owned(),
                fatal: false,
                at: DAY,
            },
        ),
    ];
    let hidden = [
        arrive(DAY, 1),
        event(DAY, Kind::Admit { pr: 1, base: None }),
        event(
            DAY,
            Kind::Repush {
                pr: 1,
                head: sha(2).to_string(),
            },
        ),
        event(
            DAY,
            Kind::Held {
                pr: 1,
                holds: Vec::new(),
            },
        ),
        event(DAY, Kind::PrCi { pr: 1 }),
        event(
            DAY,
            Kind::PrCiEnd {
                pr: 1,
                failed: false,
                cancelled: false,
            },
        ),
        event(
            DAY,
            Kind::Conflicting {
                pr: 1,
                with: Value::from("trunk"),
            },
        ),
        start(DAY, 2, &[1]),
        run_end(DAY, 2, true),
        retire(DAY, 2, false),
        event(
            DAY,
            Kind::Trunk {
                position: 4,
                sha: sha(1).to_string(),
                landed: vec![7],
                foreign: 0,
            },
        ),
        step(DAY),
    ];
    let all: Vec<Event> = hidden.iter().chain(&shown).cloned().collect();
    status.tick(Some(&o), &all);
    assert_eq!(status.document().recent, shown);
    // Every shown kind has a sentence.
    for e in &shown {
        assert!(!sentence(&e.kind).is_empty());
    }

    // Exactly 24 hours later everything is still in; a second later it is
    // out, figures and samples with it.
    status.tick(None, &[step(DAY + WINDOW)]);
    assert_eq!(status.document().recent.len(), shown.len());
    assert_ne!(status.document().figures, Figures::default());
    status.tick(None, &[step(DAY + WINDOW + 1)]);
    let document = status.document();
    assert!(document.recent.is_empty());
    assert_eq!(document.figures, Figures::default());
    assert!(status.runs.is_empty() && status.lands.is_empty() && status.counts.is_empty());
}

#[test]
fn record_since_is_the_first_observations_now_and_does_not_move() {
    let mut status = Status::default();
    status.tick(None, &[]);
    assert_eq!(status.document().record_since, None);
    status.tick(
        Some(&observation(DAY, Vec::new(), Vec::new())),
        &[step(DAY)],
    );
    status.tick(
        Some(&observation(DAY + 60, Vec::new(), Vec::new())),
        &[step(DAY + 60)],
    );
    assert_eq!(status.document().record_since, Some(DAY));
}

#[test]
fn the_document_is_equal_across_a_same_tick_and_differs_after_a_change() {
    let mut status = one_queued();
    let before = status.document();
    // A `same` tick: no observation, only its step.
    status.tick(None, &[step(DAY + 900)]);
    assert_eq!(status.document(), before);
    status.tick(
        Some(&observation(DAY + 960, vec![ready(1)], Vec::new())),
        &[step(DAY + 960)],
    );
    assert_ne!(status.document(), before);
}

#[test]
fn live_hands_a_body_over_exactly_when_the_document_differs() {
    let mut live = Live::new();
    let mut sink: Vec<String> = Vec::new();
    let mut tick = |live: &mut Live, o: Option<&Observation>, events: &[Event]| {
        live.tick(o, events);
        if let Some(body) = live.changed("0.1.0", DAY) {
            sink.push(body);
        }
    };
    // An error before any observation: nothing to show yet.
    tick(&mut live, None, &[]);
    tick(
        &mut live,
        Some(&observation(DAY, vec![ready(1)], Vec::new())),
        &[arrive(DAY, 1), step(DAY)],
    );
    tick(&mut live, None, &[step(DAY + 30)]);
    tick(
        &mut live,
        Some(&observation(DAY + 60, vec![ready(1)], Vec::new())),
        &[step(DAY + 60)],
    );
    tick(
        &mut live,
        Some(&observation(DAY + 90, Vec::new(), Vec::new())),
        &[eject(DAY + 90, 1), step(DAY + 90)],
    );
    assert_eq!(sink.len(), 2);
    assert!(sink[0].contains("| #1 | ready |"));
    assert!(sink[1].contains("nothing queued"));
    assert!(sink[1].contains("ejected #1: its CI failed"));
}

#[test]
fn an_error_tick_adds_only_its_error_to_the_window() {
    let mut status = one_queued();
    let before = status.document();
    let error = event(
        DAY + 700,
        Kind::Error {
            message: "observe flaked".to_owned(),
            fatal: false,
            at: DAY + 700,
        },
    );
    status.tick(None, std::slice::from_ref(&error));
    let after = status.document();
    assert_eq!(after.prs, before.prs);
    assert_eq!(after.batches, before.batches);
    assert_eq!(after.figures, before.figures);
    assert_eq!(after.record_since, before.record_since);
    assert_eq!(after.recent[..before.recent.len()], before.recent[..]);
    assert_eq!(after.recent[before.recent.len()..], [error]);
}

#[test]
fn live_hands_nothing_over_until_a_live_tick_observes() {
    let mut live = Live::new();
    live.seed(
        Some(&observation(DAY, vec![ready(1)], Vec::new())),
        &[arrive(DAY, 1), step(DAY)],
    );
    // The first live tick fails before observing: the seeded pool is the
    // previous process's, not what this one sees.
    live.tick(None, &[]);
    assert_eq!(live.changed("0.1.0", DAY + 3600), None);
    live.tick(
        Some(&observation(DAY + 3600, vec![ready(1)], Vec::new())),
        &[step(DAY + 3600)],
    );
    let body = live
        .changed("0.1.0", DAY + 3600)
        .expect("a live observation hands a body over");
    assert!(body.contains("| #1 | ready |"));
}

#[test]
fn a_seeded_status_has_the_times_of_one_fed_every_tick_live() {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"))
        .join("status-seed");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("q.journal");
    let header = |started| Header::start("github", &SchedulerConfig::AllAtOnce {}, started);
    let report = |tick, o: Observation| {
        Ok(TickReport {
            tick,
            observation: o,
            plan: Plan::default(),
            idle: false,
        })
    };
    let ticks = [
        observation(DAY, vec![ready(1), ready(2)], Vec::new()),
        observation(
            DAY + 60,
            vec![waiting(1, Hold::Draft), ready(2)],
            Vec::new(),
        ),
        observation(
            DAY + 120,
            vec![waiting(1, Hold::Draft), ready(2)],
            Vec::new(),
        ),
        observation(
            DAY + 180,
            vec![waiting(1, Hold::Draft), ready(2), ready(3)],
            Vec::new(),
        ),
    ];

    let mut live = Status::default();
    let (mut writer, _) = Writer::open(&path, header(DAY), None, &mut |_, _| {}).unwrap();
    for (tick, o) in ticks.iter().take(3).enumerate() {
        let outcome = report(tick as u64, o.clone());
        let events = writer.record(tick as u64, DAY, &outcome).unwrap();
        live.tick(Some(o), &events);
    }
    drop(writer);

    let mut seeded = Status::default();
    let (mut writer, _) = Writer::open(&path, header(DAY + 150), None, &mut |o, events| {
        seeded.tick(o, events)
    })
    .unwrap();
    let outcome = report(0, ticks[3].clone());
    let events = writer.record(0, DAY + 150, &outcome).unwrap();
    live.tick(Some(&ticks[3]), &events);
    seeded.tick(Some(&ticks[3]), &events);

    let times = |status: &Status| -> Vec<(u64, u64, u64)> {
        status
            .document()
            .prs
            .iter()
            .map(|p| (p.number, p.arrived, p.since))
            .collect()
    };
    assert_eq!(
        times(&seeded),
        [(1, DAY, DAY + 60), (2, DAY, DAY), (3, DAY + 180, DAY + 180)]
    );
    assert_eq!(times(&seeded), times(&live));
    assert_eq!(seeded.document().record_since, Some(DAY));
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, Kind::Restart { .. }))
    );
}

#[test]
fn times_and_durations_format_as_specified() {
    assert_eq!(civil(0), (1970, 1, 1));
    assert_eq!(civil(-1), (1969, 12, 31));
    assert_eq!(civil(11_016), (2000, 2, 29));
    assert_eq!(time(DAY + 9 * 3600 + 5 * 60, DAY + 20 * 3600), "09:05 UTC");
    assert_eq!(time(DAY - 60, DAY + 20 * 3600), "2023-11-13 23:59 UTC");
    assert_eq!(duration(59), "0m");
    assert_eq!(duration(45 * 60), "45m");
    assert_eq!(duration(2 * 3600 + 15 * 60), "2h 15m");
    assert_eq!(duration(3 * 86_400 + 4 * 3600 + 59 * 60), "3d 4h");
}

/// A document with queued pull requests, a stacked one, a batch, figures
/// and a window.
fn busy() -> (Status, u64) {
    let mut status = Status::default();
    let now = DAY + 10 * 3600;
    let stacked = PrSnapshot::new(
        PrNumber(12),
        sha(12),
        PrBase::Pr(PrNumber(11)),
        PrStatus::Ready,
        0,
    );
    status.tick(
        Some(&observation(
            DAY + 3600,
            vec![ready(11), stacked.clone()],
            Vec::new(),
        )),
        &[arrive(DAY + 3600, 11), arrive(DAY + 3600, 12)],
    );
    status.tick(
        Some(&observation(
            DAY + 2 * 3600,
            vec![ready(10), ready(11), stacked.clone()],
            Vec::new(),
        )),
        &[arrive(DAY + 2 * 3600, 10)],
    );
    status.tick(
        Some(&observation(
            DAY + 4 * 3600,
            vec![ready(10), ready(11), stacked],
            vec![testing(5, &[11, 12])],
        )),
        &[start(DAY + 4 * 3600, 5, &[11, 12])],
    );
    status.tick(
        None,
        &[
            land(DAY + 5 * 3600, 4, &[1, 2]),
            eject(DAY + 6 * 3600, 3),
            step(now),
        ],
    );
    (status, now)
}

#[test]
fn render_shows_the_queue_the_batches_the_figures_and_the_window() {
    let (status, now) = busy();
    let body = render(&status.document(), now, "0.1.0", DAY);
    assert!(body.starts_with(&format!("{MARKER}\n")));
    assert!(body.contains(
        "dorfq 0.1.0 · started 00:00 UTC · last change 10:00 UTC · 3 queued · 1 in flight"
    ));
    // Oldest arrival first; the stacked one says its base.
    let rows: Vec<&str> = body.lines().filter(|l| l.starts_with("| #")).collect();
    assert_eq!(
        rows,
        [
            "| #11 | ready | since 01:00 UTC (9h 0m) | queued 9h 0m (since 01:00 UTC) | batch 5 |",
            "| #12 on #11 | ready | since 01:00 UTC (9h 0m) | queued 9h 0m (since 01:00 UTC) | batch 5 |",
            "| #10 | ready | since 02:00 UTC (8h 0m) | queued 8h 0m (since 02:00 UTC) | — |",
        ]
    );
    assert!(
        body.contains("| 5 | on trunk | #11 #12 | testing | running 6h 0m (since 04:00 UTC) |")
    );
    assert!(body.contains(
        "landed 2 pull requests in 1 batches · ejected 1 · refused 0 · runs 0 passed, 0 failed, 0 cancelled"
    ));
    assert!(body.contains("batch run p50 —, p95 — · time to land p50 —, p95 —"));
    // Newest first.
    let ejected = body
        .find("| 06:00 UTC | ejected #3: its CI failed |")
        .unwrap();
    let landed = body.find("| 05:00 UTC | landed batch 4: #1 #2 |").unwrap();
    assert!(ejected < landed);
    // The record began inside the window.
    assert!(body.contains(
        "the record begins 01:00 UTC; earlier events are in the closed journal segments"
    ));
    let later = render(&status.document(), DAY + 1 + WINDOW + 3600, "0.1.0", DAY);
    assert!(!later.contains("the record begins"));
}

#[test]
fn an_empty_document_says_so() {
    let mut status = Status::default();
    status.tick(
        Some(&observation(DAY, Vec::new(), Vec::new())),
        &[step(DAY)],
    );
    let body = render(&status.document(), DAY, "0.1.0", DAY);
    assert!(body.contains("nothing queued"));
    assert!(body.contains("nothing in flight"));
    assert!(body.contains("nothing in the window"));
}

#[test]
fn table_cells_hold_one_line_and_no_column_break() {
    assert_eq!(
        sentence(&Kind::Error {
            message: "a | b\nc".to_owned(),
            fatal: true,
            at: 0,
        }),
        "fatal error: a | b\nc"
    );
    assert_eq!(cell("a | b\nc"), "a \\| b c");
}

#[test]
fn the_ceiling_drops_window_rows_then_the_table_then_queue_rows_never_batches() {
    let (status, now) = busy();
    let mut document = status.document();
    let long = "x".repeat(1000);
    let error = |i: u64| {
        event(
            DAY + i,
            Kind::Error {
                message: format!("{i} {long}"),
                fatal: false,
                at: 0,
            },
        )
    };

    // Window rows, oldest first.
    document.recent = (0..100).map(error).collect();
    let body = render(&document, now, "0.1.0", DAY);
    assert!(body.chars().count() <= BODY_CEILING);
    assert!(body.contains("error: 99 "));
    assert!(!body.contains("error: 0 "));
    assert!(body.contains("| #10 |"));
    assert!(body.contains("| 5 | on trunk |"));

    // Then the whole table, the figure lines staying; then queue rows from
    // the newest arrival.
    document.recent = vec![error(0)];
    let template = document.prs[2].clone();
    document.prs = (0..2000)
        .map(|i| QueuedPr {
            number: 1000 + i,
            arrived: DAY + i,
            ..template.clone()
        })
        .collect();
    let body = render(&document, now, "0.1.0", DAY);
    assert!(body.chars().count() <= BODY_CEILING);
    assert!(!body.contains("| time | event |"));
    assert!(!body.contains("nothing in the window"));
    assert!(body.contains("landed 2 pull requests"));
    assert!(body.contains("| #1000 |"));
    assert!(!body.contains("| #2999 |"));
    let kept = body.lines().filter(|l| l.starts_with("| #")).count();
    assert!(body.contains(&format!("… and {} more", 2000 - kept)));
    assert!(body.contains("| 5 | on trunk | #11 #12 |"));
}
