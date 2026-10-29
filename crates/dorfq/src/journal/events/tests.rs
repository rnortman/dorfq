use std::collections::BTreeSet;

use dorfq_core::{
    BatchId, BatchMember, BatchState, BatchStatus, CheckName, CiStatus, ConflictWith, Departed,
    Departure, EjectReason, Ejection, Failure, Hold, LandRefusal, Notice, NoticeReason,
    Observation, Plan, PrBase, PrNumber, PrSnapshot, PrStatus, Sha, StackPolicy, Timestamp,
    Withdrawal,
};
use dorfq_scheduler::SchedulerConfig;
use dorfq_sim::{PlannedPr as SimPr, PlannedStart as SimStart, TraceEvent};
use serde_json::Value;

use super::super::{Cause, Entry, Failure as LineFailure, Header, Line, LineKind};
use super::*;
use crate::driver::TickReport;

fn sha(n: u64) -> Sha {
    Sha::parse(&format!("{n:040x}")).unwrap()
}

fn pr(n: u64) -> PrSnapshot {
    PrSnapshot::new(PrNumber(n), sha(n), PrBase::Trunk, PrStatus::Ready, 0)
}

fn waiting(n: u64, holds: &[Hold]) -> PrSnapshot {
    let holds: BTreeSet<Hold> = holds.iter().copied().collect();
    PrSnapshot::new(
        PrNumber(n),
        sha(n),
        PrBase::Trunk,
        PrStatus::Waiting(holds),
        0,
    )
}

fn with_head(snapshot: PrSnapshot, head: u64) -> PrSnapshot {
    PrSnapshot::new(
        snapshot.number,
        sha(head),
        snapshot.base,
        snapshot.status,
        snapshot.behind,
    )
    .with_stack(snapshot.stack)
}

fn batch(id: u64, base: u64, members: &[u64], state: BatchState) -> BatchStatus {
    BatchStatus::new(
        BatchId(id),
        sha(base),
        members
            .iter()
            .map(|&m| BatchMember::new(PrNumber(m), sha(m)))
            .collect(),
        state,
    )
}

fn testing(tip: u64, ci: CiStatus) -> BatchState {
    BatchState::Testing { tip: sha(tip), ci }
}

/// An observation at `now` with trunk at position `position`.
struct Obs {
    now: u64,
    position: u64,
    prs: Vec<PrSnapshot>,
    batches: Vec<BatchStatus>,
    departed: Vec<Departure>,
}

fn at(now: u64) -> Obs {
    Obs {
        now,
        position: 0,
        prs: Vec::new(),
        batches: Vec::new(),
        departed: Vec::new(),
    }
}

impl Obs {
    fn prs(mut self, prs: Vec<PrSnapshot>) -> Obs {
        self.prs = prs;
        self
    }
    fn batches(mut self, batches: Vec<BatchStatus>) -> Obs {
        self.batches = batches;
        self
    }
    fn departed(mut self, n: u64, how: Departed) -> Obs {
        self.departed.push(Departure::new(PrNumber(n), sha(n), how));
        self
    }
    fn position(mut self, position: u64) -> Obs {
        self.position = position;
        self
    }
    fn build(self) -> Observation {
        Observation::new(
            Timestamp(self.now),
            sha(1000 + self.position),
            self.position,
            self.prs,
            self.batches,
            self.departed,
        )
    }
}

fn header(cause: Cause, started: u64, opened: u64) -> Entry {
    let mut header = Header::start("mock", &SchedulerConfig::AllAtOnce {}, started);
    header.cause = cause;
    header.opened = opened;
    Entry::Header(header)
}

fn report(tick: u64, o: Obs, plan: Plan) -> Entry {
    report_failing(tick, o, plan, None)
}

fn report_failing(tick: u64, o: Obs, plan: Plan, failure: Option<LineFailure>) -> Entry {
    let observation = o.build();
    let idle = observation.prs.is_empty() && observation.batches.is_empty() && plan.is_empty();
    Entry::Tick(Line {
        tick,
        at: 5000 + tick,
        kind: LineKind::Report {
            report: Box::new(TickReport {
                tick,
                observation,
                plan,
                idle,
            }),
            failure,
        },
    })
}

fn same(tick: u64, now: u64) -> Entry {
    Entry::Tick(Line {
        tick,
        at: 5000 + tick,
        kind: LineKind::Same {
            now: Timestamp(now),
        },
    })
}

fn error(tick: u64, message: &str, fatal: bool) -> Entry {
    Entry::Tick(Line {
        tick,
        at: 5000 + tick,
        kind: LineKind::Error(LineFailure {
            message: message.to_owned(),
            fatal,
        }),
    })
}

fn start() -> Entry {
    header(Cause::Start, 4000, 4000)
}

/// The events, `step`s left out, each with its time.
fn events(entries: &[Entry]) -> Vec<(u64, Kind)> {
    derive(entries)
        .into_iter()
        .filter(|e| !matches!(e.kind, Kind::Step { .. }))
        .map(|e| (e.t, e.kind))
        .collect()
}

/// The events of the last tick, `step`s left out.
fn last(entries: &[Entry]) -> Vec<Kind> {
    let all = derive(entries);
    let t = all.last().unwrap().t;
    all.into_iter()
        .filter(|e| e.t == t && !matches!(e.kind, Kind::Step { .. }))
        .map(|e| e.kind)
        .collect()
}

fn kind_name(kind: &impl Serialize) -> String {
    serde_json::to_value(kind).unwrap()["kind"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn a_pull_request_arrives_and_is_admitted_with_its_base_stack_and_head() {
    let stacked = PrSnapshot::new(
        PrNumber(2),
        sha(2),
        PrBase::Pr(PrNumber(1)),
        PrStatus::Ready,
        0,
    )
    .with_stack(StackPolicy::Atomic);
    let entries = [
        start(),
        report(0, at(10).prs(vec![pr(1), stacked]), Plan::default()),
    ];
    assert_eq!(
        events(&entries),
        vec![
            (
                10,
                Kind::Arrive {
                    pr: 1,
                    base: None,
                    atomic: false,
                    head: sha(1).to_string(),
                }
            ),
            (10, Kind::Admit { pr: 1, base: None }),
            (
                10,
                Kind::Arrive {
                    pr: 2,
                    base: Some(1),
                    atomic: true,
                    head: sha(2).to_string(),
                }
            ),
            (
                10,
                Kind::Admit {
                    pr: 2,
                    base: Some(1)
                }
            ),
        ]
    );
}

#[test]
fn an_arrival_with_holds_and_own_ci_pending_says_both() {
    let entries = [
        start(),
        report(
            0,
            at(10).prs(vec![waiting(1, &[Hold::Draft, Hold::OwnCiPending])]),
            Plan::default(),
        ),
    ];
    let kinds = last(&entries);
    assert_eq!(
        kinds[2..],
        [
            Kind::Held {
                pr: 1,
                holds: vec![Hold::Draft],
            },
            Kind::PrCi { pr: 1 },
        ]
    );
}

#[test]
fn a_number_that_departs_and_reappears_is_a_new_lifetime() {
    let entries = [
        start(),
        report(0, at(10).prs(vec![pr(1)]), Plan::default()),
        report(
            1,
            at(20).departed(1, Departed::Withdrawn(Withdrawal::IntentCleared)),
            Plan::default(),
        ),
        report(2, at(30).prs(vec![pr(1)]), Plan::default()),
    ];
    let names: Vec<(u64, String)> = events(&entries)
        .iter()
        .map(|(t, k)| (*t, kind_name(k)))
        .collect();
    assert_eq!(
        names,
        [
            (10, "arrive"),
            (10, "admit"),
            (20, "withdraw"),
            (30, "arrive"),
            (30, "admit"),
        ]
        .map(|(t, k)| (t, k.to_owned()))
    );
}

#[test]
fn a_head_change_is_a_repush() {
    let entries = [
        start(),
        report(0, at(10).prs(vec![pr(1)]), Plan::default()),
        report(1, at(20).prs(vec![with_head(pr(1), 77)]), Plan::default()),
    ];
    assert_eq!(
        last(&entries),
        [Kind::Repush {
            pr: 1,
            head: sha(77).to_string(),
        }]
    );
}

#[test]
fn hold_transitions_are_held_and_own_ci_is_pr_ci_and_pr_ci_end() {
    let ticks = [
        vec![pr(1)],
        vec![waiting(1, &[Hold::Protection])],
        vec![waiting(1, &[Hold::Protection, Hold::OwnCiPending])],
        vec![waiting(1, &[Hold::Protection, Hold::OwnCiFailed])],
        vec![waiting(1, &[Hold::OwnCiPending])],
        vec![pr(1)],
    ];
    let mut entries = vec![start()];
    for (i, prs) in ticks.into_iter().enumerate() {
        entries.push(report(
            i as u64,
            at(10 * i as u64).prs(prs),
            Plan::default(),
        ));
    }
    let after_arrival: Vec<(u64, Kind)> = events(&entries).into_iter().skip(2).collect();
    assert_eq!(
        after_arrival,
        vec![
            (
                10,
                Kind::Held {
                    pr: 1,
                    holds: vec![Hold::Protection],
                }
            ),
            (20, Kind::PrCi { pr: 1 }),
            (
                30,
                Kind::PrCiEnd {
                    pr: 1,
                    failed: true,
                    cancelled: false,
                }
            ),
            (
                40,
                Kind::Held {
                    pr: 1,
                    holds: vec![],
                }
            ),
            (40, Kind::PrCi { pr: 1 }),
            (
                50,
                Kind::PrCiEnd {
                    pr: 1,
                    failed: false,
                    cancelled: false,
                }
            ),
        ]
    );
}

#[test]
fn a_push_while_own_ci_runs_cancels_that_run_and_starts_another() {
    let entries = [
        start(),
        report(
            0,
            at(10).prs(vec![waiting(1, &[Hold::OwnCiPending])]),
            Plan::default(),
        ),
        report(
            1,
            at(20).prs(vec![with_head(waiting(1, &[Hold::OwnCiPending]), 9)]),
            Plan::default(),
        ),
    ];
    assert_eq!(
        last(&entries),
        [
            Kind::Repush {
                pr: 1,
                head: sha(9).to_string(),
            },
            Kind::PrCiEnd {
                pr: 1,
                failed: false,
                cancelled: true,
            },
            Kind::PrCi { pr: 1 },
        ]
    );
}

#[test]
fn becoming_conflicting_names_what_with() {
    let conflicting = |n, with| {
        PrSnapshot::new(
            PrNumber(n),
            sha(n),
            PrBase::Trunk,
            PrStatus::Conflicting(with),
            0,
        )
    };
    let entries = [
        start(),
        report(0, at(10).prs(vec![pr(1), pr(2)]), Plan::default()),
        report(
            1,
            at(20).prs(vec![
                conflicting(1, ConflictWith::Trunk),
                conflicting(2, ConflictWith::Pr(PrNumber(1))),
            ]),
            Plan::default(),
        ),
        report(
            2,
            at(30).prs(vec![
                conflicting(1, ConflictWith::Trunk),
                conflicting(2, ConflictWith::Pr(PrNumber(1))),
            ]),
            Plan::default(),
        ),
    ];
    assert_eq!(
        events(&entries)[4..],
        [
            (
                20,
                Kind::Conflicting {
                    pr: 1,
                    with: Value::from("trunk"),
                }
            ),
            (
                20,
                Kind::Conflicting {
                    pr: 2,
                    with: Value::from(1),
                }
            ),
        ]
    );
}

#[test]
fn each_departure_kind_and_a_number_gone_unsaid_is_lost() {
    let plan = Plan {
        eject: vec![Ejection::new(
            PrNumber(4),
            EjectReason::BatchFailed { batch: BatchId(3) },
        )],
        ..Plan::default()
    };
    let entries = [
        start(),
        report(0, at(10).prs((1..=7).map(pr).collect()), plan),
        report(
            1,
            at(20)
                .departed(1, Departed::Withdrawn(Withdrawal::Closed))
                .departed(
                    2,
                    Departed::Withdrawn(Withdrawal::Blocked { on: PrNumber(1) }),
                )
                .departed(3, Departed::Merged)
                .departed(4, Departed::Ejected)
                .departed(5, Departed::Landed { batch: BatchId(1) })
                .departed(6, Departed::Withdrawn(Withdrawal::Retargeted)),
            Plan::default(),
        ),
    ];
    assert_eq!(
        last(&entries),
        [
            Kind::Withdraw {
                pr: 1,
                how: Withdrawal::Closed,
            },
            Kind::Blocked {
                pr: 2,
                on: 1,
                r#final: true,
            },
            Kind::Merged { pr: 3 },
            Kind::Eject {
                pr: 4,
                reason: Some("batch 3 failed".to_owned()),
                r#final: true,
            },
            Kind::Withdraw {
                pr: 6,
                how: Withdrawal::Retargeted,
            },
            Kind::Lost { pr: 7 },
        ]
    );
}

#[test]
fn an_eject_carries_final_true_and_a_reason_absent_from_the_input_is_null() {
    let entries = [
        start(),
        report(0, at(10).prs(vec![pr(4)]), Plan::default()),
        report(1, at(20).departed(4, Departed::Ejected), Plan::default()),
    ];
    let eject = derive(&entries)
        .into_iter()
        .find(|e| matches!(e.kind, Kind::Eject { .. }))
        .unwrap();
    assert_eq!(
        serde_json::to_value(&eject).unwrap(),
        serde_json::json!({"t": 20, "kind": "eject", "pr": 4, "reason": null, "final": true})
    );
}

#[test]
fn across_a_restart_a_pull_request_gone_is_lost_and_a_batch_gone_is_retired() {
    let entries = [
        start(),
        report(
            0,
            at(10).prs(vec![pr(1), pr(2)]).batches(vec![batch(
                1,
                1000,
                &[1],
                testing(50, CiStatus::Pending),
            )]),
            Plan::default(),
        ),
        header(Cause::Start, 4100, 4100),
        report(0, at(90).prs(vec![pr(2)]), Plan::default()),
    ];
    assert_eq!(
        last(&entries),
        [
            Kind::Restart {
                started: 4100,
                dorfq: crate::version().to_owned(),
                at: 4100,
            },
            Kind::Lost { pr: 1 },
            Kind::Retire {
                batch: 1,
                cancelled: true,
            },
        ]
    );
}

#[test]
fn a_restart_forgets_the_old_processs_batch_ids_and_trunk_position() {
    let pending = |tip| testing(tip, CiStatus::Pending);
    let entries = [
        start(),
        report(
            0,
            at(10).prs(vec![pr(1), pr(5)]).batches(vec![
                batch(1, 1000, &[1], pending(51)),
                batch(5, 1000, &[5], pending(55)),
            ]),
            Plan::default(),
        ),
        report(
            1,
            at(20)
                .prs(vec![pr(5)])
                .position(1)
                .departed(1, Departed::Landed { batch: BatchId(1) })
                .batches(vec![
                    batch(1, 1000, &[1], BatchState::Landed { tip: sha(51) }),
                    batch(5, 1000, &[5], pending(55)),
                ]),
            Plan::default(),
        ),
        report(
            2,
            at(30)
                .prs(vec![pr(5)])
                .position(1)
                .batches(vec![batch(5, 1000, &[5], pending(55))]),
            Plan::default(),
        ),
        header(Cause::Start, 4100, 4100),
        // The new process counts trunk from 0 and adopts batch 5.
        report(
            0,
            at(90)
                .prs(vec![pr(5)])
                .batches(vec![batch(5, 1000, &[5], pending(55))]),
            Plan::default(),
        ),
        report(
            1,
            at(100).prs(vec![pr(5), pr(7), pr(8), pr(9)]).batches(vec![
                batch(1, 1000, &[7], pending(61)),
                batch(2, 61, &[8], pending(62)),
                batch(5, 1000, &[5], pending(55)),
                batch(6, 55, &[9], pending(66)),
            ]),
            Plan::default(),
        ),
    ];
    let after: Vec<Kind> = events(&entries)
        .into_iter()
        .filter(|(t, k)| *t >= 90 && !matches!(k, Kind::Arrive { .. } | Kind::Admit { .. }))
        .map(|(_, k)| k)
        .collect();
    let start = |batch, base, members: Vec<u64>| Kind::Start {
        batch,
        base,
        probe: false,
        members,
    };
    assert_eq!(
        after,
        [
            Kind::Restart {
                started: 4100,
                dorfq: crate::version().to_owned(),
                at: 4100,
            },
            start(1, None, vec![7]),
            start(2, Some(1), vec![8]),
            start(6, Some(5), vec![9]),
        ]
    );
}

#[test]
fn a_rotate_header_emits_nothing_and_the_observation_carries_across_it() {
    let entries = [
        start(),
        report(0, at(10).prs(vec![pr(1)]), Plan::default()),
        header(Cause::Rotate, 4000, 4500),
        report(1, at(20).prs(vec![pr(1)]), Plan::default()),
    ];
    assert_eq!(events(&entries).len(), 2, "{:?}", events(&entries));
}

#[test]
fn batches_start_on_trunk_on_a_tip_or_as_probes_then_end() {
    let pending = |tip| testing(tip, CiStatus::Pending);
    let failed = Failure::new([CheckName("build".to_owned())].into());
    let entries = [
        start(),
        report(
            0,
            at(10).prs(vec![pr(1), pr(2)]).batches(vec![
                batch(1, 1000, &[1], pending(51)),
                batch(2, 51, &[2], pending(52)),
                batch(3, 1000, &[], pending(53)),
            ]),
            Plan::default(),
        ),
        report(
            1,
            at(20).prs(vec![pr(1), pr(2)]).batches(vec![
                batch(1, 1000, &[1], testing(51, CiStatus::Passed)),
                batch(2, 51, &[2], testing(52, CiStatus::Failed(failed))),
                batch(3, 1000, &[], pending(53)),
            ]),
            Plan::default(),
        ),
        report(
            2,
            at(30)
                .prs(vec![pr(2)])
                .position(1)
                .departed(1, Departed::Landed { batch: BatchId(1) })
                .batches(vec![
                    batch(1, 1000, &[1], BatchState::Landed { tip: sha(51) }),
                    batch(3, 1000, &[], pending(53)),
                ]),
            Plan::default(),
        ),
    ];
    let start = |batch, base, members: Vec<u64>| Kind::Start {
        batch,
        base,
        probe: members.is_empty(),
        members,
    };
    assert_eq!(
        events(&entries)[4..],
        [
            (10, start(1, None, vec![1])),
            (10, start(2, Some(1), vec![2])),
            (10, start(3, None, vec![])),
            (
                20,
                Kind::RunEnd {
                    batch: 1,
                    passed: true,
                    checks: vec![],
                }
            ),
            (
                20,
                Kind::RunEnd {
                    batch: 2,
                    passed: false,
                    checks: vec!["build".to_owned()],
                }
            ),
            (
                30,
                Kind::Land {
                    batch: 1,
                    members: vec![1],
                }
            ),
            (
                30,
                Kind::Retire {
                    batch: 2,
                    cancelled: false,
                }
            ),
            (
                30,
                Kind::Trunk {
                    position: 1,
                    sha: sha(1001).to_string(),
                    landed: vec![1],
                    foreign: 0,
                }
            ),
        ]
    );
}

#[test]
fn a_batch_first_seen_decided_starts_then_ends_and_a_refusal_and_a_conflict_are_said_once() {
    let refused = BatchState::LandRefused {
        tip: sha(52),
        reason: LandRefusal::MemberGone { pr: PrNumber(2) },
    };
    let conflict = BatchState::Conflict {
        pr: PrNumber(3),
        with: ConflictWith::Pr(PrNumber(2)),
    };
    let batches = vec![
        batch(1, 1000, &[1], testing(51, CiStatus::Passed)),
        batch(2, 1000, &[2], testing(52, CiStatus::Passed)),
        batch(4, 1000, &[2, 3], conflict),
    ];
    let mut later = batches.clone();
    later[1] = batch(2, 1000, &[2], refused);
    let entries = [
        start(),
        report(0, at(10).prs(vec![pr(1), pr(2), pr(3)]), Plan::default()),
        report(
            1,
            at(20).prs(vec![pr(1), pr(2), pr(3)]).batches(batches),
            Plan::default(),
        ),
        report(
            2,
            at(30).prs(vec![pr(1), pr(2), pr(3)]).batches(later.clone()),
            Plan::default(),
        ),
        report(
            3,
            at(40).prs(vec![pr(1), pr(2), pr(3)]).batches(later),
            Plan::default(),
        ),
    ];
    let names: Vec<(u64, String)> = events(&entries)[6..]
        .iter()
        .map(|(t, k)| (*t, kind_name(k)))
        .collect();
    assert_eq!(
        names,
        [
            (20, "start"),
            (20, "run_end"),
            (20, "start"),
            (20, "run_end"),
            (20, "start"),
            (20, "conflict"),
            (30, "refused"),
        ]
        .map(|(t, k)| (t, k.to_owned()))
    );
}

#[test]
fn trunk_moved_by_another_hand_is_foreign() {
    let entries = [
        start(),
        report(0, at(10), Plan::default()),
        report(1, at(20).position(3), Plan::default()),
    ];
    assert_eq!(
        last(&entries),
        [Kind::Trunk {
            position: 3,
            sha: sha(1003).to_string(),
            landed: vec![],
            foreign: 3,
        }]
    );
}

#[test]
fn a_plan_notice_is_a_notice_and_the_step_carries_the_plan() {
    let plan = Plan {
        notify: vec![Notice::new(
            PrNumber(1),
            NoticeReason::Broken { batch: BatchId(2) },
        )],
        eject: vec![Ejection::new(
            PrNumber(1),
            EjectReason::Conflicts {
                with: ConflictWith::Trunk,
            },
        )],
        start: vec![dorfq_core::BatchStart::new(
            BatchId(3),
            sha(1000),
            vec![PrNumber(1)],
        )],
        land: vec![BatchId(1)],
        retire: vec![BatchId(2)],
    };
    let all = derive(&[start(), report(7, at(10).prs(vec![pr(1)]), plan)]);
    let tail: Vec<Value> = all[2..]
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    assert_eq!(
        tail,
        vec![
            serde_json::json!({"t": 10, "kind": "notice", "pr": 1,
                "reason": "probably broken against trunk (batch 2)"}),
            serde_json::json!({"t": 10, "kind": "step", "queued": 1, "in_flight": 0,
                "land": [1],
                "notify": [{"pr": 1, "reason": "probably broken against trunk (batch 2)"}],
                "start": [{"batch": 3, "members": [1]}],
                "eject": [{"pr": 1, "reason": "conflicts with trunk"}],
                "retire": [2], "at": 5007, "tick": 7}),
        ]
    );
}

#[test]
fn a_step_projects_an_observation_and_plan_as_the_simulators_does() {
    let plan = Plan {
        notify: vec![Notice::new(
            PrNumber(1),
            NoticeReason::Broken { batch: BatchId(2) },
        )],
        eject: vec![Ejection::new(
            PrNumber(2),
            EjectReason::Conflicts {
                with: ConflictWith::Trunk,
            },
        )],
        start: vec![dorfq_core::BatchStart::new(
            BatchId(5),
            sha(1000),
            vec![PrNumber(1), PrNumber(2)],
        )],
        land: vec![BatchId(1)],
        retire: vec![BatchId(2)],
    };
    let failed = Failure::new(BTreeSet::new());
    let observation = at(10)
        .prs(vec![pr(1), pr(2), pr(3), pr(4)])
        .batches(vec![
            batch(1, 1000, &[1], testing(51, CiStatus::Passed)),
            batch(2, 51, &[2], testing(52, CiStatus::Failed(failed))),
            batch(3, 1000, &[3], testing(53, CiStatus::Pending)),
            batch(4, 53, &[4], testing(54, CiStatus::Pending)),
        ])
        .build();
    let mut real = serde_json::to_value(Event {
        t: 10,
        kind: step(&observation, &plan, 0, 0),
    })
    .unwrap();
    let fields = real.as_object_mut().unwrap();
    fields.remove("at");
    fields.remove("tick");
    let simulator = serde_json::to_value(dorfq_sim::step_event(10, &observation, &plan)).unwrap();
    assert_eq!(real, simulator);
}

#[test]
fn a_same_line_is_a_step_at_its_own_time() {
    let all = derive(&[
        start(),
        report(0, at(10).prs(vec![pr(1)]), Plan::default()),
        same(1, 25),
    ]);
    let step = all.last().unwrap();
    assert_eq!(step.t, 25);
    assert!(matches!(
        step.kind,
        Kind::Step {
            queued: 1,
            tick: 1,
            at: 5001,
            ..
        }
    ));
}

#[test]
fn errors_ride_on_the_tick_or_the_time_before_or_after() {
    let failing = Some(LineFailure {
        message: "transient forge error: eject".to_owned(),
        fatal: false,
    });
    let entries = [
        start(),
        error(0, "transient forge error: observe", false),
        report(1, at(10).prs(vec![pr(1)]), Plan::default()),
        report_failing(2, at(20).prs(vec![pr(1)]), Plan::default(), failing),
        error(3, "scheduler error: no", true),
    ];
    let all: Vec<(u64, String)> = derive(&entries)
        .iter()
        .map(|e| (e.t, kind_name(e)))
        .collect();
    assert_eq!(
        all,
        [
            (10, "error"),
            (10, "arrive"),
            (10, "admit"),
            (10, "step"),
            (20, "step"),
            (20, "error"),
            (20, "error"),
        ]
        .map(|(t, k)| (t, k.to_owned()))
    );
    let last = derive(&entries).pop().unwrap();
    assert_eq!(
        last.kind,
        Kind::Error {
            message: "scheduler error: no".to_owned(),
            fatal: true,
            at: 5003,
        }
    );
}

#[test]
fn an_error_after_a_rotate_header_waits_for_the_next_observation() {
    let entries = [
        start(),
        report(0, at(10), Plan::default()),
        header(Cause::Rotate, 4000, 4500),
        error(1, "transient forge error: observe", false),
    ];
    let errors = |entries: &[Entry]| -> Vec<u64> {
        derive(entries)
            .iter()
            .filter(|e| kind_name(*e) == "error")
            .map(|e| e.t)
            .collect()
    };
    // At the end of the journal it is released at the last observation's
    // time; with an observation after it, at that one's.
    assert_eq!(errors(&entries), [10]);
    let observed = [entries.as_slice(), &[report(2, at(20), Plan::default())]].concat();
    assert_eq!(errors(&observed), [20]);
    // The segment after the rotation, read alone, places it the same.
    assert_eq!(errors(&observed[2..]), [20]);
}

#[test]
fn a_batch_starts_on_a_tip_last_seen_two_observations_before() {
    let pending = |tip| testing(tip, CiStatus::Pending);
    let entries = [
        start(),
        report(
            0,
            at(10)
                .prs(vec![pr(1)])
                .batches(vec![batch(1, 1000, &[1], pending(51))]),
            Plan::default(),
        ),
        report(1, at(20).prs(vec![pr(2)]), Plan::default()),
        report(
            2,
            at(30)
                .prs(vec![pr(2)])
                .batches(vec![batch(2, 51, &[2], pending(52))]),
            Plan::default(),
        ),
    ];
    let starts: Vec<Kind> = events(&entries)
        .into_iter()
        .filter(|(_, k)| matches!(k, Kind::Start { .. }))
        .map(|(_, k)| k)
        .collect();
    assert_eq!(
        starts,
        [
            Kind::Start {
                batch: 1,
                base: None,
                probe: false,
                members: vec![1],
            },
            Kind::Start {
                batch: 2,
                base: Some(1),
                probe: false,
                members: vec![2],
            },
        ]
    );
}

#[test]
fn an_ejection_reason_planned_ticks_before_the_departure_is_kept() {
    let plan = Plan {
        eject: vec![
            Ejection::new(PrNumber(1), EjectReason::BatchFailed { batch: BatchId(3) }),
            Ejection::new(PrNumber(2), EjectReason::BatchFailed { batch: BatchId(3) }),
        ],
        ..Plan::default()
    };
    let entries = [
        start(),
        report(0, at(10).prs(vec![pr(1), pr(2)]), plan),
        report(1, at(20).prs(vec![pr(1), pr(2)]), Plan::default()),
        // Pull request 2 is gone unsaid and back: a new lifetime with no
        // reason of its own.
        report(2, at(30).prs(vec![pr(1)]), Plan::default()),
        report(3, at(40).prs(vec![pr(1), pr(2)]), Plan::default()),
        report(
            4,
            at(50)
                .departed(1, Departed::Ejected)
                .departed(2, Departed::Ejected),
            Plan::default(),
        ),
    ];
    assert_eq!(
        last(&entries),
        [
            Kind::Eject {
                pr: 1,
                reason: Some("batch 3 failed".to_owned()),
                r#final: true,
            },
            Kind::Eject {
                pr: 2,
                reason: None,
                r#final: true,
            },
        ]
    );
}

#[test]
fn within_a_tick_departures_then_endings_then_trunk_then_changes_then_starts() {
    let entries = [
        start(),
        report(
            0,
            at(10).prs(vec![pr(1), pr(2), pr(3)]).batches(vec![batch(
                1,
                1000,
                &[1],
                testing(51, CiStatus::Passed),
            )]),
            Plan::default(),
        ),
        report(
            1,
            at(20)
                .position(1)
                .prs(vec![with_head(pr(3), 33), pr(4)])
                .batches(vec![
                    batch(1, 1000, &[1], BatchState::Landed { tip: sha(51) }),
                    batch(2, 1001, &[3], testing(52, CiStatus::Pending)),
                ])
                .departed(1, Departed::Landed { batch: BatchId(1) })
                .departed(2, Departed::Merged),
            Plan {
                notify: vec![Notice::new(
                    PrNumber(4),
                    NoticeReason::Broken { batch: BatchId(1) },
                )],
                ..Plan::default()
            },
        ),
    ];
    let all = derive(&entries);
    let names: Vec<String> = all.iter().filter(|e| e.t == 20).map(kind_name).collect();
    assert_eq!(
        names,
        [
            "merged", "land", "trunk", "repush", "arrive", "admit", "start", "notice", "step"
        ]
    );
}

/// The simulator's events of every kind the real stream shares with it.
fn simulator_events() -> Vec<TraceEvent> {
    let planned = SimPr {
        pr: 1,
        reason: String::new(),
    };
    vec![
        TraceEvent::Arrive {
            t: 0,
            pr: 1,
            base: None,
            atomic: false,
            conflicts: false,
        },
        TraceEvent::Admit {
            t: 0,
            pr: 1,
            base: None,
        },
        TraceEvent::Repush { t: 0, pr: 1 },
        TraceEvent::Withdraw { t: 0, pr: 1 },
        TraceEvent::Blocked {
            t: 0,
            pr: 1,
            on: 2,
            r#final: true,
            bad: false,
        },
        TraceEvent::Start {
            t: 0,
            batch: 1,
            base: None,
            members: vec![1],
        },
        TraceEvent::RunEnd {
            t: 0,
            batch: 1,
            passed: true,
            broken: false,
            flake: false,
            outage: false,
        },
        TraceEvent::Land {
            t: 0,
            batch: 1,
            members: vec![1],
        },
        TraceEvent::Refused {
            t: 0,
            batch: 1,
            reason: String::new(),
        },
        TraceEvent::Eject {
            t: 0,
            pr: 1,
            reason: String::new(),
            justified: true,
            r#final: true,
        },
        TraceEvent::Notice {
            t: 0,
            pr: 1,
            reason: String::new(),
            justified: true,
        },
        TraceEvent::Retire {
            t: 0,
            batch: 1,
            cancelled: false,
        },
        TraceEvent::PrCi {
            t: 0,
            pr: 1,
            billed: false,
        },
        TraceEvent::PrCiEnd {
            t: 0,
            pr: 1,
            cancelled: false,
            failed: false,
        },
        TraceEvent::Step {
            t: 0,
            queued: 0,
            in_flight: 0,
            land: vec![1],
            notify: vec![planned.clone()],
            start: vec![SimStart {
                batch: 1,
                members: vec![1],
            }],
            eject: vec![planned],
            retire: vec![1],
        },
    ]
}

/// The real stream's events of every kind it shares with the simulator.
fn shared_real_events() -> Vec<Kind> {
    let planned = PlannedPr {
        pr: 1,
        reason: String::new(),
    };
    vec![
        Kind::Arrive {
            pr: 1,
            base: None,
            atomic: false,
            head: String::new(),
        },
        Kind::Admit { pr: 1, base: None },
        Kind::Repush {
            pr: 1,
            head: String::new(),
        },
        Kind::Withdraw {
            pr: 1,
            how: Withdrawal::Closed,
        },
        Kind::Blocked {
            pr: 1,
            on: 2,
            r#final: true,
        },
        Kind::Start {
            batch: 1,
            base: None,
            members: vec![1],
            probe: false,
        },
        Kind::RunEnd {
            batch: 1,
            passed: true,
            checks: vec![],
        },
        Kind::Land {
            batch: 1,
            members: vec![1],
        },
        Kind::Refused {
            batch: 1,
            reason: String::new(),
        },
        Kind::Eject {
            pr: 1,
            reason: None,
            r#final: true,
        },
        Kind::Notice {
            pr: 1,
            reason: String::new(),
        },
        Kind::Retire {
            batch: 1,
            cancelled: false,
        },
        Kind::PrCi { pr: 1 },
        Kind::PrCiEnd {
            pr: 1,
            failed: false,
            cancelled: false,
        },
        Kind::Step {
            queued: 0,
            in_flight: 0,
            land: vec![1],
            notify: vec![planned.clone()],
            start: vec![PlannedStart {
                batch: 1,
                members: vec![1],
            }],
            eject: vec![planned],
            retire: vec![1],
            at: 0,
            tick: 0,
        },
    ]
}

/// Every field name of `value`, with the names inside arrays of objects
/// as `<field>.<name>`.
fn fields(value: &Value) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for (name, field) in value.as_object().unwrap() {
        names.insert(name.clone());
        if let Some(Value::Object(item)) = field.as_array().and_then(|a| a.first()) {
            names.extend(item.keys().map(|k| format!("{name}.{k}")));
        }
    }
    names
}

#[test]
fn the_shared_vocabulary_matches_the_simulators() {
    let real_only: &[(&str, &[&str])] = &[
        ("arrive", &["head"]),
        ("repush", &["head"]),
        ("withdraw", &["how"]),
        ("start", &["probe"]),
        ("run_end", &["checks"]),
        ("step", &["at", "tick"]),
    ];
    let simulator = simulator_events();
    let real = shared_real_events();
    assert_eq!(simulator.len(), real.len());
    for (sim, kind) in simulator.iter().zip(&real) {
        let sim = serde_json::to_value(sim).unwrap();
        let real = serde_json::to_value(Event {
            t: 0,
            kind: kind.clone(),
        })
        .unwrap();
        let name = real["kind"].as_str().unwrap();
        assert_eq!(sim["kind"], real["kind"]);
        let mut allowed = fields(&sim);
        if let Some((_, extra)) = real_only.iter().find(|(k, _)| *k == name) {
            allowed.extend(extra.iter().map(|f| (*f).to_owned()));
        }
        let unknown: Vec<String> = fields(&real).difference(&allowed).cloned().collect();
        assert!(
            unknown.is_empty(),
            "{name}: {unknown:?} not in the simulator's vocabulary"
        );
    }
}
