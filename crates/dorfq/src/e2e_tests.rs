//! The driver running a scheduler against `MockForge`, scenario to its end.

use std::cell::RefCell;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use dorfq_core::{
    BatchId, BatchState, CiStatus, ConflictWith, EjectReason, LandRefusal, PrNumber, Scheduler, Sha,
};
use dorfq_forge_mock::{Event, Message, MockForge, Scenario};
use dorfq_scheduler::{AllAtOnce, SchedulerConfig};

use crate::driver::{Driver, TickReport};
use crate::journal::events::{Kind, derive};
use crate::journal::{self, Entry, Header, LineKind, Writer};
use crate::providers::Shared;

/// The mock's synthetic commit `n`.
fn sha(n: u64) -> Sha {
    Sha::parse(&format!("{n:040x}")).unwrap()
}

struct Run {
    forge: MockForge,
    reports: Vec<TickReport>,
}

impl Run {
    fn trunk(&self) -> &Sha {
        &self.reports.last().unwrap().observation.trunk
    }

    fn landed(&self) -> Vec<u64> {
        self.forge
            .messages()
            .iter()
            .filter(|m| matches!(m.event, Event::Landed { .. }))
            .map(|m| m.pr.0)
            .collect()
    }

    fn ejected(&self) -> Vec<(u64, EjectReason)> {
        self.forge
            .messages()
            .iter()
            .filter_map(|m| match &m.event {
                Event::Ejected { reason } => Some((m.pr.0, reason.clone())),
                _ => None,
            })
            .collect()
    }

    /// The batches started, as (tick, id, members).
    fn starts(&self) -> Vec<(u64, u64, Vec<u64>)> {
        self.reports
            .iter()
            .flat_map(|r| {
                r.plan
                    .start
                    .iter()
                    .map(|s| (r.tick, s.id.0, s.prs.iter().map(|p| p.0).collect()))
            })
            .collect()
    }

    /// Every land refusal any observation reported.
    fn refusals(&self) -> Vec<LandRefusal> {
        let mut out: Vec<LandRefusal> = Vec::new();
        for report in &self.reports {
            for batch in &report.observation.batches {
                if let BatchState::LandRefused { reason, .. } = &batch.state
                    && !out.contains(reason)
                {
                    out.push(reason.clone());
                }
            }
        }
        out
    }
}

/// [`run_with`] under `AllAtOnce`, unjournaled.
fn run(scenario: &str) -> Run {
    run_with(scenario, &mut AllAtOnce::new(), None)
}

/// Ticks `scheduler` until a tick is idle and every scenario pull request
/// has arrived, recording each tick in `journal` at host time equal to its
/// tick; any forge error fails the test.
fn run_with(
    scenario: &str,
    scheduler: &mut dyn Scheduler,
    mut journal: Option<&mut Writer>,
) -> Run {
    let scenario = Scenario::from_toml(scenario).unwrap();
    let mock = Rc::new(RefCell::new(MockForge::new(scenario, Box::new(io::sink()))));
    let mut forge = Shared(Rc::clone(&mock));
    let mut reports = Vec::new();
    {
        let mut driver = Driver::new(&mut forge, scheduler);
        loop {
            let outcome = driver.tick();
            if let Some(writer) = journal.as_deref_mut() {
                let tick = driver.ticks() - 1;
                writer.record(tick, tick, &outcome).unwrap();
            }
            let report = outcome.unwrap();
            let idle = report.idle;
            reports.push(report);
            if idle && !mock.borrow().awaiting_arrivals() {
                break;
            }
            assert!(reports.len() < 100, "never idle");
        }
    }
    drop(forge);
    let forge = Rc::into_inner(mock).expect("the harness holds the last handle");
    Run {
        forge: forge.into_inner(),
        reports,
    }
}

fn message(pr: u64, event: Event) -> Message {
    Message {
        pr: PrNumber(pr),
        event,
    }
}

fn failed(batch: u64) -> EjectReason {
    EjectReason::BatchFailed {
        batch: BatchId(batch),
    }
}

#[test]
fn all_good_lands_in_one_batch() {
    let run = run("ci_ticks = 2\n[[pr]]\nnumber = 1\n[[pr]]\nnumber = 2\n[[pr]]\nnumber = 3\n");
    // Trunk is commit 1, heads 2-4, the batch tip 5.
    assert_eq!(run.trunk(), &sha(5));
    assert_eq!(run.starts(), [(0, 1, vec![1, 2, 3])]);
    assert_eq!(
        run.forge.messages(),
        [
            message(1, Event::Queued),
            message(2, Event::Queued),
            message(3, Event::Queued),
            message(1, Event::Testing { batch: BatchId(1) }),
            message(2, Event::Testing { batch: BatchId(1) }),
            message(3, Event::Testing { batch: BatchId(1) }),
            message(1, Event::Landed { tip: sha(5) }),
            message(2, Event::Landed { tip: sha(5) }),
            message(3, Event::Landed { tip: sha(5) }),
        ]
    );
    // Start, pending, land, retire, idle.
    assert_eq!(run.reports.len(), 5);
}

#[test]
fn trunk_conflict_is_ejected_first_and_the_rest_land() {
    let run = run(
        "ci_ticks = 2\n[[pr]]\nnumber = 1\n[[pr]]\nnumber = 2\nmergeable = false\n\
         [[pr]]\nnumber = 3\n",
    );
    let first = &run.reports[0].plan;
    assert_eq!(first.eject.len(), 1);
    assert_eq!(first.eject[0].pr, PrNumber(2));
    assert_eq!(run.starts(), [(0, 1, vec![1, 3])]);
    assert_eq!(
        run.ejected(),
        [(
            2,
            EjectReason::Conflicts {
                with: ConflictWith::Trunk,
            }
        )]
    );
    assert_eq!(run.landed(), [1, 3]);
    assert_eq!(run.trunk(), &sha(5));
    assert_eq!(run.reports.len(), 5);
}

#[test]
fn one_bad_member_ejects_the_whole_batch() {
    let run = run(
        "ci_ticks = 2\n[[pr]]\nnumber = 1\n[[pr]]\nnumber = 2\nbad = true\n\
         [[pr]]\nnumber = 3\n",
    );
    assert_eq!(
        run.ejected(),
        [(1, failed(1)), (2, failed(1)), (3, failed(1))]
    );
    assert!(run.landed().is_empty());
    assert_eq!(run.trunk(), &sha(1));
    // Start, pending, failed (eject and retire), idle.
    assert_eq!(run.reports.len(), 4);
}

#[test]
fn breaking_pair_fails_the_batch_as_a_ci_failure() {
    let run = run(
        "ci_ticks = 1\n[[pr]]\nnumber = 1\nbreaks_with = [2]\n[[pr]]\nnumber = 2\n\
         [[pr]]\nnumber = 3\n",
    );
    assert_eq!(
        run.ejected(),
        [(1, failed(1)), (2, failed(1)), (3, failed(1))]
    );
    assert!(run.landed().is_empty());
    assert_eq!(run.trunk(), &sha(1));
    assert_eq!(run.reports.len(), 3);
}

#[test]
fn member_conflict_ejects_the_later_member_and_the_rebuild_lands() {
    let run = run(
        "ci_ticks = 2\n[[pr]]\nnumber = 1\n[[pr]]\nnumber = 2\nconflicts_with = [1]\n\
         [[pr]]\nnumber = 3\n",
    );
    assert_eq!(run.starts(), [(0, 1, vec![1, 2, 3]), (2, 2, vec![1, 3])]);
    assert_eq!(
        run.ejected(),
        [(
            2,
            EjectReason::Conflicts {
                with: ConflictWith::Pr(PrNumber(1)),
            }
        )]
    );
    // A batch that ends in a conflict tells no one it is testing.
    let testing: Vec<u64> = run
        .forge
        .messages()
        .iter()
        .filter_map(|m| match m.event {
            Event::Testing { batch } => Some(batch.0),
            _ => None,
        })
        .collect();
    assert_eq!(testing, [2, 2]);
    assert_eq!(run.landed(), [1, 3]);
    // Heads 2-4; batch 1 has no tip; batch 2's tip is 5.
    assert_eq!(run.trunk(), &sha(5));
    assert_eq!(run.reports.len(), 7);
}

#[test]
fn withdrawal_during_testing_refuses_the_landing_and_the_rest_land() {
    let run = run(
        "ci_ticks = 2\n[[pr]]\nnumber = 1\n[[pr]]\nnumber = 2\ndequeue_at = 1\n\
         [[pr]]\nnumber = 3\n",
    );
    assert_eq!(
        run.refusals(),
        [LandRefusal::MemberGone { pr: PrNumber(2) }]
    );
    assert_eq!(run.starts(), [(0, 1, vec![1, 2, 3]), (4, 2, vec![1, 3])]);
    let withdrawn: Vec<u64> = run
        .forge
        .messages()
        .iter()
        .filter(|m| m.event == Event::Withdrawn)
        .map(|m| m.pr.0)
        .collect();
    assert_eq!(withdrawn, [2]);
    assert!(run.ejected().is_empty());
    assert_eq!(run.landed(), [1, 3]);
    // Heads 2-4, batch 1's tip 5, batch 2's tip 6.
    assert_eq!(run.trunk(), &sha(6));
    assert_eq!(run.reports.len(), 9);
}

#[test]
fn repush_during_testing_refuses_the_landing_then_lands_the_new_head() {
    let run = run("ci_ticks = 2\n[[pr]]\nnumber = 1\nrepush_at = 1\n[[pr]]\nnumber = 2\n");
    assert_eq!(
        run.refusals(),
        [LandRefusal::MemberGone { pr: PrNumber(1) }]
    );
    assert_eq!(run.starts(), [(0, 1, vec![1, 2]), (4, 2, vec![1, 2])]);
    // Heads 2-3, batch 1's tip 4, the re-push 5, batch 2's tip 6.
    let rebuilt = run
        .reports
        .iter()
        .flat_map(|r| &r.observation.batches)
        .find(|b| b.id == BatchId(2))
        .unwrap();
    assert_eq!(rebuilt.members[0].head, sha(5));
    assert_eq!(run.landed(), [1, 2]);
    assert_eq!(run.trunk(), &sha(6));
}

#[test]
fn late_arrival_after_a_quiet_spell_still_lands() {
    let run = run("ci_ticks = 1\n[[pr]]\nnumber = 1\n[[pr]]\nnumber = 2\narrives_at = 6\n");
    assert_eq!(run.starts(), [(0, 1, vec![1]), (6, 2, vec![2])]);
    assert_eq!(run.landed(), [1, 2]);
    let idle: Vec<u64> = run
        .reports
        .iter()
        .filter(|r| r.idle)
        .map(|r| r.tick)
        .collect();
    // Batch 1 retires at tick 2; ticks 3-5 are quiet; batch 2 retires at 8.
    assert_eq!(idle, [3, 4, 5, 9]);
}

#[test]
fn shipped_example_runs_to_idle() {
    let run = run(include_str!("../examples/scenario.toml"));
    // Tick 0: 4 is ejected with no run and [1, 2, 3] starts as batch 1.
    let first = &run.reports[0].plan;
    assert_eq!(first.eject.len(), 1);
    assert_eq!(first.eject[0].pr, PrNumber(4));
    assert_eq!(run.starts(), [(0, 1, vec![1, 2, 3]), (4, 2, vec![5])]);
    assert_eq!(run.reports[2].plan.land, [BatchId(1)]);
    assert_eq!(
        run.ejected(),
        [
            (
                4,
                EjectReason::Conflicts {
                    with: ConflictWith::Trunk,
                }
            ),
            (5, failed(2))
        ]
    );
    // Heads 2-5, batch 1's tip 6, pr 5's head 7, batch 2's tip 8.
    assert_eq!(run.landed(), [1, 2, 3]);
    assert_eq!(run.trunk(), &sha(6));
    assert!(run.reports.last().unwrap().idle);
    assert_eq!(run.reports.len(), 8);
}

#[test]
fn shipped_scenario_runs_to_idle_under_bayes() {
    let shipped: toml::Table = toml::from_str(include_str!("../examples/github.toml")).unwrap();
    let config: SchedulerConfig = shipped["scheduler"].clone().try_into().unwrap();
    config.validate().unwrap();
    let mut scheduler = config.build();
    let run = run_with(
        include_str!("../examples/scenario.toml"),
        scheduler.as_mut(),
        None,
    );
    assert_eq!(run.starts()[0], (0, 1, vec![1, 2, 3]));
    assert_eq!(run.landed(), [1, 2, 3]);
    // 5 fails alone until probes, batches with no members, have passed on
    // trunk and its failures on both sides of a stretch boundary single it
    // out. How many failures and probes that takes depends on the priors and
    // on `MAX_COMPONENT_RUNS`, so it is not asserted.
    let last = run.starts().last().unwrap().clone();
    assert_eq!(last.2, [5]);
    assert_eq!(
        run.ejected(),
        [
            (
                4,
                EjectReason::Conflicts {
                    with: ConflictWith::Trunk,
                }
            ),
            (5, failed(last.1))
        ]
    );
    let probes: Vec<u64> = run
        .starts()
        .iter()
        .filter(|s| s.2.is_empty())
        .map(|s| s.1)
        .collect();
    assert!(!probes.is_empty());
    let passed = |id: u64| {
        run.reports.iter().any(|r| {
            r.observation.batches.iter().any(|b| {
                b.id == BatchId(id)
                    && matches!(
                        b.state,
                        BatchState::Testing {
                            ci: CiStatus::Passed,
                            ..
                        }
                    )
            })
        })
    };
    assert!(probes.iter().all(|&id| passed(id)));
    assert!(run.reports.last().unwrap().idle);
}

#[test]
fn a_stack_lands_in_one_batch_with_its_authors_told_in_order() {
    let run = run(
        "ci_ticks = 1\n[[pr]]\nnumber = 1\n[[pr]]\nnumber = 2\nbase = 1\n\
         [[pr]]\nnumber = 3\nbase = 2\n",
    );
    assert_eq!(run.starts(), [(0, 1, vec![1, 2, 3])]);
    assert_eq!(run.landed(), [1, 2, 3]);
    assert!(run.ejected().is_empty());
}

#[test]
fn a_base_dequeued_while_waiting_leaves_its_child_blocked_and_told() {
    // 2 waits on its own CI until it withdraws at tick 2; 3 is ready but
    // its base never is.
    let run = run("ci_ticks = 1\n[[pr]]\nnumber = 1\n\
         [[pr]]\nnumber = 2\nbase = 1\nown_ci = \"pending\"\ndequeue_at = 2\n\
         [[pr]]\nnumber = 3\nbase = 2\n");
    assert_eq!(run.starts(), [(0, 1, vec![1])]);
    assert_eq!(run.landed(), [1]);
    let to_three: Vec<&Event> = run
        .forge
        .messages()
        .iter()
        .filter(|m| m.pr == PrNumber(3))
        .map(|m| &m.event)
        .collect();
    assert_eq!(
        to_three,
        [&Event::Queued, &Event::Blocked { on: PrNumber(2) }]
    );
}

#[test]
fn a_journaled_run_records_what_the_driver_returned() {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"))
        .join("e2e-journal");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("q.journal");
    let header = Header::start("mock", &SchedulerConfig::AllAtOnce {}, 1000);
    let (mut writer, _) = Writer::open(&path, header).unwrap();
    let run = run_with(
        include_str!("../examples/scenario.toml"),
        &mut AllAtOnce::new(),
        Some(&mut writer),
    );
    drop(writer);
    // The final idle tick closed the segment the run began.
    let segments = [dir.join("q.journal.1000"), path];
    let read = journal::read(&segments).unwrap();
    let mut ticks = Vec::new();
    for entry in &read.entries {
        let Entry::Tick(line) = entry else { continue };
        let returned = &run.reports[line.tick as usize];
        match &line.kind {
            LineKind::Report { report, failure } => {
                assert_eq!(**report, *returned);
                assert_eq!(*failure, None);
            }
            LineKind::Same { now } => assert_eq!(*now, returned.observation.now),
            LineKind::Error(failure) => panic!("tick {}: {failure:?}", line.tick),
        }
        ticks.push(line.tick);
    }
    let every: Vec<u64> = (0..run.reports.len() as u64).collect();
    assert_eq!(ticks, every);
    let events = derive(&read.entries);
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.kind, Kind::Land { batch: 1, .. })),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            &e.kind,
            Kind::Eject { pr: 5, reason: Some(reason), .. } if reason == "batch 2 failed"
        )),
        "{events:?}"
    );
}
