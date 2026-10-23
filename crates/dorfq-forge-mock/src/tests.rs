use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io;
use std::rc::Rc;

use dorfq_core::ledger::Violation;

use super::*;

/// A sink the test can read back.
#[derive(Clone, Default)]
struct Buffer(Rc<RefCell<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8(self.0.borrow().clone()).unwrap()
    }
}

fn forge(toml: &str) -> (MockForge, Buffer) {
    let buffer = Buffer::default();
    let forge = MockForge::new(Scenario::from_toml(toml).unwrap(), Box::new(buffer.clone()));
    (forge, buffer)
}

fn prs(numbers: &[u64]) -> Vec<PrNumber> {
    numbers.iter().copied().map(PrNumber).collect()
}

fn start(id: u64, base: &Sha, members: &[u64]) -> BatchStart {
    BatchStart {
        id: BatchId(id),
        base: base.clone(),
        prs: prs(members),
    }
}

fn numbers(observation: &Observation) -> Vec<u64> {
    observation.prs.iter().map(|p| p.number.0).collect()
}

fn state(observation: &Observation, id: u64) -> BatchState {
    observation
        .batches
        .iter()
        .find(|b| b.id == BatchId(id))
        .map(|b| b.state.clone())
        .expect("batch is tracked")
}

fn violation_of(result: Result<(), ForgeError>) -> Violation {
    match result {
        Err(ForgeError::Permanent(e)) => e
            .downcast_ref::<Violation>()
            .expect("a contract violation")
            .clone(),
        other => panic!("expected a permanent error, got {other:?}"),
    }
}

/// Starts batch 1 of `members` on trunk at the first observation and
/// observes until its CI has decided.
fn decided(toml: &str, members: &[u64]) -> (MockForge, Observation) {
    let (mut forge, _) = forge(toml);
    let first = forge.observe().unwrap();
    forge.start_batch(&start(1, &first.trunk, members)).unwrap();
    loop {
        let observation = forge.observe().unwrap();
        if !matches!(
            state(&observation, 1),
            BatchState::Testing {
                ci: CiStatus::Pending,
                ..
            }
        ) {
            return (forge, observation);
        }
    }
}

const THREE: &str = "ci_ticks = 1\n[[pr]]\nnumber = 1\n[[pr]]\nnumber = 2\n[[pr]]\nnumber = 3\n";

#[test]
fn synthetic_shas_are_valid_and_unique() {
    let (mut forge, _) = forge(THREE);
    let observation = forge.observe().unwrap();
    forge
        .start_batch(&start(1, &observation.trunk, &[1, 2, 3]))
        .unwrap();
    let observation = forge.observe().unwrap();
    let BatchState::Testing { tip, .. } = state(&observation, 1) else {
        panic!("batch should be testing");
    };
    let mut shas: Vec<Sha> = observation.prs.iter().map(|p| p.head.clone()).collect();
    shas.push(observation.trunk.clone());
    shas.push(tip);
    for sha in &shas {
        assert_eq!(Sha::parse(sha.as_str()).as_ref(), Ok(sha));
    }
    assert_eq!(shas.iter().collect::<BTreeSet<_>>().len(), shas.len());
}

#[test]
fn observations_are_sorted() {
    let (mut forge, _) =
        forge("ci_ticks = 1\n[[pr]]\nnumber = 3\n[[pr]]\nnumber = 1\n[[pr]]\nnumber = 2\n");
    let observation = forge.observe().unwrap();
    assert_eq!(numbers(&observation), [1, 2, 3]);
    forge
        .start_batch(&start(5, &observation.trunk, &[3]))
        .unwrap();
    forge
        .start_batch(&start(2, &observation.trunk, &[1]))
        .unwrap();
    let observation = forge.observe().unwrap();
    let ids: Vec<u64> = observation.batches.iter().map(|b| b.id.0).collect();
    assert_eq!(ids, [2, 5]);
}

#[test]
fn snapshots_report_mergeability_and_own_ci() {
    let (mut forge, _) = forge(
        r#"
        ci_ticks = 1
        [[pr]]
        number = 1
        mergeable = false
        own_ci = "pending"
        [[pr]]
        number = 2
        own_ci = "failed"
        "#,
    );
    let observation = forge.observe().unwrap();
    assert_eq!(observation.prs[0].mergeable, Mergeable::Conflicting);
    assert_eq!(observation.prs[0].head_ci, CiStatus::Pending);
    assert_eq!(observation.prs[1].mergeable, Mergeable::Clean);
    assert_eq!(observation.prs[1].head_ci, CiStatus::Failed);
}

#[test]
fn arrives_at_delays_appearance() {
    let (mut forge, _) = forge("ci_ticks = 1\n[[pr]]\nnumber = 1\narrives_at = 2\n");
    assert!(forge.observe().unwrap().prs.is_empty());
    assert!(forge.observe().unwrap().prs.is_empty());
    assert_eq!(numbers(&forge.observe().unwrap()), [1]);
}

#[test]
fn build_conflicts_with_trunk() {
    let (mut forge, _) =
        forge("ci_ticks = 1\n[[pr]]\nnumber = 1\n[[pr]]\nnumber = 2\nmergeable = false\n");
    let observation = forge.observe().unwrap();
    forge
        .start_batch(&start(1, &observation.trunk, &[1, 2]))
        .unwrap();
    assert_eq!(
        state(&forge.observe().unwrap(), 1),
        BatchState::Conflict {
            pr: PrNumber(2),
            with: ConflictWith::Trunk
        }
    );
}

#[test]
fn build_conflicts_with_an_earlier_member_and_tells_no_one() {
    let (mut forge, buffer) = forge(
        "ci_ticks = 1\n[[pr]]\nnumber = 1\n[[pr]]\nnumber = 2\n[[pr]]\nnumber = 3\nconflicts_with = [1]\n",
    );
    let observation = forge.observe().unwrap();
    let queued = forge.messages().len();
    forge
        .start_batch(&start(1, &observation.trunk, &[1, 2, 3]))
        .unwrap();
    let observation = forge.observe().unwrap();
    assert_eq!(
        state(&observation, 1),
        BatchState::Conflict {
            pr: PrNumber(3),
            with: ConflictWith::Pr(PrNumber(1))
        }
    );
    assert_eq!(forge.messages().len(), queued);
    assert!(!buffer.text().contains("testing"));
}

#[test]
fn ci_decides_after_exactly_ci_ticks_observations() {
    let (mut forge, _) = forge("ci_ticks = 3\n[[pr]]\nnumber = 1\n");
    let observation = forge.observe().unwrap();
    forge
        .start_batch(&start(1, &observation.trunk, &[1]))
        .unwrap();
    for _ in 0..2 {
        assert!(matches!(
            state(&forge.observe().unwrap(), 1),
            BatchState::Testing {
                ci: CiStatus::Pending,
                ..
            }
        ));
    }
    assert!(matches!(
        state(&forge.observe().unwrap(), 1),
        BatchState::Testing {
            ci: CiStatus::Passed,
            ..
        }
    ));
}

#[test]
fn zero_ci_ticks_decides_at_start() {
    let (mut forge, _) = forge("ci_ticks = 0\n[[pr]]\nnumber = 1\n");
    let observation = forge.observe().unwrap();
    forge
        .start_batch(&start(1, &observation.trunk, &[1]))
        .unwrap();
    assert!(matches!(
        state(&forge.observe().unwrap(), 1),
        BatchState::Testing {
            ci: CiStatus::Passed,
            ..
        }
    ));
}

#[test]
fn a_bad_member_fails_the_batch() {
    let (_, observation) = decided(
        "ci_ticks = 2\n[[pr]]\nnumber = 1\n[[pr]]\nnumber = 2\nbad = true\n",
        &[1, 2],
    );
    assert!(matches!(
        state(&observation, 1),
        BatchState::Testing {
            ci: CiStatus::Failed,
            ..
        }
    ));
}

#[test]
fn a_breaks_with_pair_fails_the_batch_and_either_alone_passes() {
    let scenario = "ci_ticks = 1\n[[pr]]\nnumber = 1\nbreaks_with = [2]\n[[pr]]\nnumber = 2\n[[pr]]\nnumber = 3\n";
    let (_, both) = decided(scenario, &[2, 3, 1]);
    assert!(matches!(
        state(&both, 1),
        BatchState::Testing {
            ci: CiStatus::Failed,
            ..
        }
    ));
    for alone in [[1, 3], [2, 3]] {
        let (_, observation) = decided(scenario, &alone);
        assert!(matches!(
            state(&observation, 1),
            BatchState::Testing {
                ci: CiStatus::Passed,
                ..
            }
        ));
    }
}

#[test]
fn land_fast_forwards_trunk_and_merges_members() {
    let (mut forge, observation) = decided(THREE, &[1, 3]);
    let BatchState::Testing { tip, .. } = state(&observation, 1) else {
        panic!("batch should be testing");
    };
    forge.land(BatchId(1)).unwrap();
    let observation = forge.observe().unwrap();
    assert_eq!(observation.trunk, tip);
    assert_eq!(numbers(&observation), [2]);
    assert_eq!(
        state(&observation, 1),
        BatchState::Landed { tip: tip.clone() }
    );
    let landed: Vec<&Message> = forge
        .messages()
        .iter()
        .filter(|m| matches!(m.event, Event::Landed { .. }))
        .collect();
    assert_eq!(
        landed,
        [
            &Message {
                pr: PrNumber(1),
                event: Event::Landed { tip: tip.clone() }
            },
            &Message {
                pr: PrNumber(3),
                event: Event::Landed { tip }
            }
        ]
    );
}

#[test]
fn land_after_trunk_moved_is_refused() {
    let (mut forge, _) = forge(THREE);
    let observation = forge.observe().unwrap();
    forge
        .start_batch(&start(1, &observation.trunk, &[1]))
        .unwrap();
    forge
        .start_batch(&start(2, &observation.trunk, &[2]))
        .unwrap();
    let observation = forge.observe().unwrap();
    let BatchState::Testing { tip: first, .. } = state(&observation, 1) else {
        panic!("batch should be testing");
    };
    let BatchState::Testing { tip: second, .. } = state(&observation, 2) else {
        panic!("batch should be testing");
    };
    forge.land(BatchId(1)).unwrap();
    forge.land(BatchId(2)).unwrap();
    let observation = forge.observe().unwrap();
    assert_eq!(observation.trunk, first);
    assert_eq!(
        state(&observation, 2),
        BatchState::LandRefused {
            tip: second,
            reason: LandRefusal::TrunkMoved { trunk: first }
        }
    );
    assert_eq!(numbers(&observation), [2, 3]);
}

#[test]
fn land_after_a_member_withdrew_is_refused() {
    let (mut forge, buffer) =
        forge("ci_ticks = 2\n[[pr]]\nnumber = 1\n[[pr]]\nnumber = 2\ndequeue_at = 1\n");
    let observation = forge.observe().unwrap();
    forge
        .start_batch(&start(1, &observation.trunk, &[1, 2]))
        .unwrap();
    let observation = forge.observe().unwrap();
    assert_eq!(numbers(&observation), [1]);
    assert_eq!(
        forge.messages().last(),
        Some(&Message {
            pr: PrNumber(2),
            event: Event::Withdrawn
        })
    );
    assert!(buffer.text().contains("message to pr 2: withdrawn\n"));
    let observation = forge.observe().unwrap();
    let BatchState::Testing { tip, .. } = state(&observation, 1) else {
        panic!("batch should be testing");
    };
    let trunk = observation.trunk.clone();
    forge.land(BatchId(1)).unwrap();
    let observation = forge.observe().unwrap();
    assert_eq!(observation.trunk, trunk);
    assert_eq!(
        state(&observation, 1),
        BatchState::LandRefused {
            tip,
            reason: LandRefusal::MemberGone { pr: PrNumber(2) }
        }
    );
}

#[test]
fn land_after_a_member_was_repushed_is_refused() {
    let (mut forge, _) = forge("ci_ticks = 1\n[[pr]]\nnumber = 1\nrepush_at = 1\n");
    let observation = forge.observe().unwrap();
    let old_head = observation.prs[0].head.clone();
    forge
        .start_batch(&start(1, &observation.trunk, &[1]))
        .unwrap();
    let observation = forge.observe().unwrap();
    assert_ne!(observation.prs[0].head, old_head);
    let batch = &observation.batches[0];
    assert_eq!(batch.members[0].head, old_head);
    forge.land(BatchId(1)).unwrap();
    assert!(matches!(
        state(&forge.observe().unwrap(), 1),
        BatchState::LandRefused {
            reason: LandRefusal::MemberGone { pr: PrNumber(1) },
            ..
        }
    ));
}

#[test]
fn batches_echo_base_and_members_until_retired() {
    let (mut forge, observation) = decided(THREE, &[2, 1]);
    let batch = &observation.batches[0];
    let heads: Vec<(u64, Sha)> = batch
        .members
        .iter()
        .map(|m| (m.pr.0, m.head.clone()))
        .collect();
    assert_eq!(
        heads,
        [
            (2, observation.prs[1].head.clone()),
            (1, observation.prs[0].head.clone())
        ]
    );
    forge.land(BatchId(1)).unwrap();
    assert_eq!(forge.observe().unwrap().batches.len(), 1);
    forge.retire(BatchId(1)).unwrap();
    assert!(forge.observe().unwrap().batches.is_empty());
}

#[test]
fn eject_removes_and_tells_the_reason() {
    let (mut forge, buffer) = forge(THREE);
    forge.observe().unwrap();
    forge
        .eject(PrNumber(2), &EjectReason::BatchFailed { batch: BatchId(4) })
        .unwrap();
    forge
        .eject(PrNumber(3), &EjectReason::ConflictsWith { pr: PrNumber(1) })
        .unwrap();
    assert_eq!(numbers(&forge.observe().unwrap()), [1]);
    assert_eq!(
        forge.messages()[3..],
        [
            Message {
                pr: PrNumber(2),
                event: Event::Ejected {
                    reason: EjectReason::BatchFailed { batch: BatchId(4) }
                }
            },
            Message {
                pr: PrNumber(3),
                event: Event::Ejected {
                    reason: EjectReason::ConflictsWith { pr: PrNumber(1) }
                }
            }
        ]
    );
    let text = buffer.text();
    assert!(
        text.contains("message to pr 2: ejected: batch 4 failed\n"),
        "{text}"
    );
    assert!(
        text.contains("message to pr 3: ejected: conflicts with pr 1\n"),
        "{text}"
    );
}

#[test]
fn messages_follow_a_pull_request_through_the_queue() {
    let (mut forge, buffer) = forge("ci_ticks = 1\n[[pr]]\nnumber = 1\narrives_at = 1\n");
    let trunk = forge.observe().unwrap().trunk;
    assert!(forge.messages().is_empty());
    forge.observe().unwrap();
    forge.start_batch(&start(1, &trunk, &[1])).unwrap();
    let observation = forge.observe().unwrap();
    let BatchState::Testing { tip, .. } = state(&observation, 1) else {
        panic!("batch should be testing");
    };
    forge.land(BatchId(1)).unwrap();
    let pr = PrNumber(1);
    assert_eq!(
        forge.messages(),
        [
            Message {
                pr,
                event: Event::Queued
            },
            Message {
                pr,
                event: Event::Testing { batch: BatchId(1) }
            },
            Message {
                pr,
                event: Event::Landed { tip: tip.clone() }
            },
        ]
    );
    assert_eq!(
        buffer.text(),
        format!(
            "message to pr 1: queued\nmessage to pr 1: testing in batch 1\nmessage to pr 1: landed as {tip}\n"
        )
    );
}

#[test]
fn no_withdrawal_message_after_leaving_the_queue() {
    let (mut forge, _) = forge("ci_ticks = 1\n[[pr]]\nnumber = 1\ndequeue_at = 1\n");
    forge.observe().unwrap();
    forge
        .eject(PrNumber(1), &EjectReason::ConflictsWithTrunk)
        .unwrap();
    forge.observe().unwrap();
    assert!(!forge.messages().iter().any(|m| m.event == Event::Withdrawn));
}

#[test]
fn start_on_a_batch_tip_is_accepted() {
    let (mut forge, observation) = decided(THREE, &[1]);
    let BatchState::Testing { tip, .. } = state(&observation, 1) else {
        panic!("batch should be testing");
    };
    forge.start_batch(&start(2, &tip, &[2])).unwrap();
    assert_eq!(forge.observe().unwrap().batches[1].base, tip);
}

#[test]
fn rule_5_violations_are_permanent() {
    let (mut forge, _) = forge(THREE);
    let observation = forge.observe().unwrap();
    let trunk = observation.trunk.clone();
    forge.start_batch(&start(1, &trunk, &[1])).unwrap();
    assert_eq!(
        violation_of(forge.start_batch(&start(1, &trunk, &[2]))),
        Violation::BatchIdInUse { batch: BatchId(1) }
    );
    forge.observe().unwrap();
    assert_eq!(
        violation_of(forge.start_batch(&start(1, &trunk, &[2]))),
        Violation::BatchIdInUse { batch: BatchId(1) }
    );
    assert_eq!(
        violation_of(forge.start_batch(&start(2, &trunk, &[]))),
        Violation::EmptyBatch { batch: BatchId(2) }
    );
    assert_eq!(
        violation_of(forge.start_batch(&start(2, &trunk, &[2, 3, 2]))),
        Violation::DuplicateMember {
            batch: BatchId(2),
            pr: PrNumber(2)
        }
    );
    assert_eq!(
        violation_of(forge.start_batch(&start(2, &trunk, &[2, 9]))),
        Violation::MemberNotKnown {
            batch: BatchId(2),
            pr: PrNumber(9)
        }
    );
    forge
        .eject(PrNumber(3), &EjectReason::ConflictsWithTrunk)
        .unwrap();
    assert_eq!(
        violation_of(forge.start_batch(&start(2, &trunk, &[2, 3]))),
        Violation::MemberNotKnown {
            batch: BatchId(2),
            pr: PrNumber(3)
        }
    );
    let stray = Sha::parse(&"f".repeat(40)).unwrap();
    assert_eq!(
        violation_of(forge.start_batch(&start(2, &stray, &[2]))),
        Violation::UnknownBase {
            batch: BatchId(2),
            base: stray
        }
    );
}

#[test]
fn rule_6_violations_are_permanent() {
    let (mut forge, _) = forge("ci_ticks = 2\n[[pr]]\nnumber = 1\n");
    let observation = forge.observe().unwrap();
    assert_eq!(
        violation_of(forge.land(BatchId(1))),
        Violation::UnknownBatch {
            effect: "land",
            batch: BatchId(1)
        }
    );
    assert_eq!(
        violation_of(forge.retire(BatchId(1))),
        Violation::UnknownBatch {
            effect: "retire",
            batch: BatchId(1)
        }
    );
    assert_eq!(
        violation_of(forge.eject(PrNumber(9), &EjectReason::ConflictsWithTrunk)),
        Violation::UnknownPr { pr: PrNumber(9) }
    );
    forge
        .start_batch(&start(1, &observation.trunk, &[1]))
        .unwrap();
    // Started since the latest observation, so not yet known.
    assert_eq!(
        violation_of(forge.land(BatchId(1))),
        Violation::UnknownBatch {
            effect: "land",
            batch: BatchId(1)
        }
    );
    forge.observe().unwrap();
    assert_eq!(
        violation_of(forge.land(BatchId(1))),
        Violation::NotPassed { batch: BatchId(1) }
    );
    forge.observe().unwrap();
    forge.land(BatchId(1)).unwrap();
    assert_eq!(
        violation_of(forge.land(BatchId(1))),
        Violation::LandedTwice { batch: BatchId(1) }
    );
    assert_eq!(
        violation_of(forge.eject(PrNumber(1), &EjectReason::ConflictsWithTrunk)),
        Violation::LandedInPlan {
            effect: "eject",
            pr: PrNumber(1)
        }
    );
    forge.retire(BatchId(1)).unwrap();
    assert_eq!(
        violation_of(forge.retire(BatchId(1))),
        Violation::UnknownBatch {
            effect: "retire",
            batch: BatchId(1)
        }
    );
}

#[test]
fn a_plan_neither_builds_on_nor_ejects_what_it_lands() {
    let landed_in_plan = |effect, pr| Violation::LandedInPlan {
        effect,
        pr: PrNumber(pr),
    };

    // The landing succeeds.
    let (mut landed, observation) = decided(THREE, &[1, 2]);
    landed.land(BatchId(1)).unwrap();
    assert_eq!(
        violation_of(landed.start_batch(&start(2, &observation.trunk, &[3, 2]))),
        landed_in_plan("start_batch", 2)
    );

    // Refused because trunk moved: batch 1 lands first, batch 2 is refused.
    let (mut moved, _) = forge(THREE);
    let first = moved.observe().unwrap();
    moved.start_batch(&start(1, &first.trunk, &[1])).unwrap();
    moved.start_batch(&start(2, &first.trunk, &[2])).unwrap();
    let observation = moved.observe().unwrap();
    moved.land(BatchId(1)).unwrap();
    moved.land(BatchId(2)).unwrap();
    assert!(matches!(
        moved.batches[&BatchId(2)].status.state,
        BatchState::LandRefused {
            reason: LandRefusal::TrunkMoved { .. },
            ..
        }
    ));
    assert_eq!(
        violation_of(moved.start_batch(&start(3, &observation.trunk, &[2]))),
        landed_in_plan("start_batch", 2)
    );
    assert_eq!(
        violation_of(moved.eject(PrNumber(2), &EjectReason::ConflictsWithTrunk)),
        landed_in_plan("eject", 2)
    );

    // Refused because a member withdrew; the other member is still known.
    let (mut withdrawn, _) =
        forge("ci_ticks = 2\n[[pr]]\nnumber = 1\n[[pr]]\nnumber = 2\ndequeue_at = 1\n");
    let first = withdrawn.observe().unwrap();
    withdrawn
        .start_batch(&start(1, &first.trunk, &[1, 2]))
        .unwrap();
    withdrawn.observe().unwrap();
    let observation = withdrawn.observe().unwrap();
    withdrawn.land(BatchId(1)).unwrap();
    assert!(matches!(
        withdrawn.batches[&BatchId(1)].status.state,
        BatchState::LandRefused {
            reason: LandRefusal::MemberGone { .. },
            ..
        }
    ));
    assert_eq!(
        violation_of(withdrawn.start_batch(&start(2, &observation.trunk, &[1]))),
        landed_in_plan("start_batch", 1)
    );
}

#[test]
fn land_on_a_batch_that_did_not_pass_is_permanent() {
    let (mut failed, observation) = decided("ci_ticks = 1\n[[pr]]\nnumber = 1\nbad = true\n", &[1]);
    assert_eq!(
        state(&observation, 1),
        BatchState::Testing {
            tip: commit(3),
            ci: CiStatus::Failed
        }
    );
    assert_eq!(
        violation_of(failed.land(BatchId(1))),
        Violation::NotPassed { batch: BatchId(1) }
    );

    let (mut conflicted, _) = forge("ci_ticks = 1\n[[pr]]\nnumber = 1\nmergeable = false\n");
    let first = conflicted.observe().unwrap();
    conflicted
        .start_batch(&start(1, &first.trunk, &[1]))
        .unwrap();
    assert!(matches!(
        state(&conflicted.observe().unwrap(), 1),
        BatchState::Conflict { .. }
    ));
    assert_eq!(
        violation_of(conflicted.land(BatchId(1))),
        Violation::NotPassed { batch: BatchId(1) }
    );

    let (mut landed, _) = decided("ci_ticks = 1\n[[pr]]\nnumber = 1\n", &[1]);
    landed.land(BatchId(1)).unwrap();
    assert!(matches!(
        state(&landed.observe().unwrap(), 1),
        BatchState::Landed { .. }
    ));
    assert_eq!(
        violation_of(landed.land(BatchId(1))),
        Violation::NotPassed { batch: BatchId(1) }
    );
}

#[test]
fn awaiting_arrivals_until_the_last_pull_request_arrives() {
    let (mut forge, _) = forge("ci_ticks = 1\n[[pr]]\nnumber = 1\narrives_at = 2\n");
    assert!(forge.awaiting_arrivals());
    assert!(forge.observe().unwrap().prs.is_empty());
    forge.observe().unwrap();
    assert!(forge.awaiting_arrivals());
    assert_eq!(numbers(&forge.observe().unwrap()), [1]);
    assert!(!forge.awaiting_arrivals());
}

/// A sink whose every write fails.
struct Broken;

impl Write for Broken {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "sink closed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_message_that_cannot_be_written_is_permanent() {
    let mut forge = MockForge::new(
        Scenario::from_toml("ci_ticks = 1\n[[pr]]\nnumber = 1\n").unwrap(),
        Box::new(Broken),
    );
    match forge.observe() {
        Err(ForgeError::Permanent(e)) => {
            let error = e.downcast_ref::<io::Error>().expect("the sink's error");
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        }
        other => panic!("expected a permanent error, got {other:?}"),
    }
}

#[test]
fn ejecting_the_same_pull_request_twice_after_one_observation_is_permanent() {
    let (mut forge, _) = forge(THREE);
    forge.observe().unwrap();
    forge
        .eject(PrNumber(2), &EjectReason::ConflictsWithTrunk)
        .unwrap();
    assert_eq!(
        violation_of(forge.eject(PrNumber(2), &EjectReason::ConflictsWithTrunk)),
        Violation::UnknownPr { pr: PrNumber(2) }
    );
}

#[test]
fn violations_say_what_was_wrong() {
    let message =
        ForgeError::Permanent(Box::new(Violation::UnknownPr { pr: PrNumber(9) })).to_string();
    assert!(message.contains("eject: pr 9 is not known"), "{message}");
    let landed = Violation::LandedInPlan {
        effect: "start_batch",
        pr: PrNumber(4),
    }
    .to_string();
    assert_eq!(
        landed,
        "start_batch: pr 4 is a member of a batch this plan lands"
    );
    let unknown = Violation::MemberNotKnown {
        batch: BatchId(2),
        pr: PrNumber(4),
    }
    .to_string();
    assert_eq!(
        unknown,
        "start_batch: batch 2 names pr 4, which is not known"
    );
}
