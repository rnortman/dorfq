//! [`AllAtOnce`], the stub scheduler.

use dorfq_core::{
    BatchStart, BatchState, BatchStatus, CiStatus, EjectReason, Observation, Plan, PrNumber,
    Scheduler,
};

use crate::{Ejections, Ids, conflict_reason, is_ready};

/// Batches every ready pull request into one batch on trunk, one batch at a
/// time.
///
/// Its only state is the next batch id to use, so ids never repeat within a
/// run; it is raised above every batch id an observation reports, so a
/// restart against a forge that still tracks batches never reuses one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllAtOnce {
    ids: Ids,
}

impl AllAtOnce {
    pub fn new() -> AllAtOnce {
        AllAtOnce { ids: Ids::new() }
    }
}

impl Default for AllAtOnce {
    fn default() -> AllAtOnce {
        AllAtOnce::new()
    }
}

impl Scheduler for AllAtOnce {
    fn step(&mut self, observation: &Observation) -> Plan {
        self.ids.raise(observation);

        let mut plan = Plan::default();
        let mut ejections = Ejections::new(observation, &mut plan.eject);

        let lands = |batch: &BatchStatus| {
            batch.base == observation.trunk
                && matches!(
                    batch.state,
                    BatchState::Testing {
                        ci: CiStatus::Passed,
                        ..
                    }
                )
        };
        for batch in observation.batches.iter().filter(|b| lands(b)) {
            ejections.landing(batch);
        }

        for batch in &observation.batches {
            match &batch.state {
                BatchState::Conflict { pr, with } => {
                    ejections.eject(*pr, conflict_reason(*with));
                    plan.retire.push(batch.id);
                }
                BatchState::Testing {
                    ci: CiStatus::Pending,
                    ..
                } => {}
                BatchState::Testing {
                    ci: CiStatus::Failed,
                    ..
                } => {
                    for member in &batch.members {
                        ejections.eject(member.pr, EjectReason::BatchFailed { batch: batch.id });
                    }
                    plan.retire.push(batch.id);
                }
                BatchState::Testing {
                    ci: CiStatus::Passed,
                    ..
                } => {
                    if lands(batch) {
                        plan.land.push(batch.id);
                    } else {
                        plan.retire.push(batch.id);
                    }
                }
                BatchState::Landed { .. } | BatchState::LandRefused { .. } => {
                    plan.retire.push(batch.id);
                }
            }
        }

        ejections.eject_conflicting(observation);

        if observation.batches.is_empty() {
            let ready: Vec<PrNumber> = observation
                .prs
                .iter()
                .filter(|p| is_ready(p))
                .map(|p| p.number)
                .collect();
            if !ready.is_empty() {
                plan.start.push(BatchStart {
                    id: self.ids.mint(),
                    base: observation.trunk.clone(),
                    prs: ready,
                });
            }
        }

        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use dorfq_core::{BatchId, ConflictWith, LandRefusal, Mergeable};

    #[test]
    fn first_observation_batches_ready_prs_in_number_order() {
        let mut s = AllAtOnce::new();
        let plan = s.step(&observation(vec![ready(1), ready(2), ready(5)], vec![]));
        assert_eq!(
            plan,
            Plan {
                start: vec![BatchStart {
                    id: BatchId(1),
                    base: trunk(),
                    prs: numbers(&[1, 2, 5]),
                }],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn conflicting_pr_is_ejected_without_a_batch() {
        let mut s = AllAtOnce::new();
        let plan = s.step(&observation(
            vec![ready(1), pr(2, Mergeable::Conflicting, CiStatus::Passed)],
            vec![],
        ));
        assert_eq!(
            plan.eject,
            vec![ejection(2, EjectReason::ConflictsWithTrunk)]
        );
        assert_eq!(plan.start.len(), 1);
        assert_eq!(plan.start[0].prs, numbers(&[1]));
    }

    #[test]
    fn unknown_mergeability_and_unpassed_own_ci_wait() {
        let mut s = AllAtOnce::new();
        let plan = s.step(&observation(
            vec![
                pr(1, Mergeable::Unknown, CiStatus::Passed),
                pr(2, Mergeable::Clean, CiStatus::Pending),
                pr(3, Mergeable::Clean, CiStatus::Failed),
                ready(4),
            ],
            vec![],
        ));
        assert!(plan.eject.is_empty());
        assert_eq!(plan.start.len(), 1);
        assert_eq!(plan.start[0].prs, numbers(&[4]));
    }

    #[test]
    fn nothing_qualifying_starts_nothing() {
        let mut s = AllAtOnce::new();
        let plan = s.step(&observation(
            vec![pr(1, Mergeable::Unknown, CiStatus::Pending)],
            vec![],
        ));
        assert!(plan.is_empty());
    }

    #[test]
    fn empty_queue_gives_empty_plan() {
        let mut s = AllAtOnce::new();
        assert!(s.step(&observation(vec![], vec![])).is_empty());
    }

    #[test]
    fn no_batch_starts_while_one_is_tracked() {
        let mut s = AllAtOnce::new();
        let plan = s.step(&observation(
            vec![ready(1), ready(2)],
            vec![testing(1, trunk(), &[1], CiStatus::Pending)],
        ));
        assert!(plan.is_empty());
    }

    #[test]
    fn failed_batch_ejects_every_member_and_retires() {
        let mut s = AllAtOnce::new();
        let plan = s.step(&observation(
            vec![ready(1), ready(2), ready(3)],
            vec![testing(4, trunk(), &[1, 2], CiStatus::Failed)],
        ));
        let reason = EjectReason::BatchFailed { batch: BatchId(4) };
        assert_eq!(
            plan,
            Plan {
                eject: vec![ejection(1, reason.clone()), ejection(2, reason)],
                retire: vec![BatchId(4)],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn failed_member_also_conflicting_is_ejected_once_for_the_batch() {
        let mut s = AllAtOnce::new();
        let plan = s.step(&observation(
            vec![pr(1, Mergeable::Conflicting, CiStatus::Passed), ready(2)],
            vec![testing(1, trunk(), &[1, 2], CiStatus::Failed)],
        ));
        let reason = EjectReason::BatchFailed { batch: BatchId(1) };
        assert_eq!(
            plan.eject,
            vec![ejection(1, reason.clone()), ejection(2, reason)]
        );
    }

    #[test]
    fn member_already_gone_is_not_ejected() {
        let mut s = AllAtOnce::new();
        let plan = s.step(&observation(
            vec![ready(2)],
            vec![testing(1, trunk(), &[1, 2], CiStatus::Failed)],
        ));
        assert_eq!(
            plan.eject,
            vec![ejection(2, EjectReason::BatchFailed { batch: BatchId(1) })]
        );
        assert_eq!(plan.retire, vec![BatchId(1)]);
    }

    #[test]
    fn passed_batch_on_current_trunk_is_landed() {
        let mut s = AllAtOnce::new();
        let plan = s.step(&observation(
            vec![ready(1)],
            vec![testing(1, trunk(), &[1], CiStatus::Passed)],
        ));
        assert_eq!(
            plan,
            Plan {
                land: vec![BatchId(1)],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn landing_member_also_conflicting_is_not_ejected() {
        let mut s = AllAtOnce::new();
        let plan = s.step(&observation(
            vec![
                pr(1, Mergeable::Conflicting, CiStatus::Passed),
                ready(2),
                pr(3, Mergeable::Conflicting, CiStatus::Passed),
            ],
            vec![testing(1, trunk(), &[1, 2], CiStatus::Passed)],
        ));
        assert_eq!(
            plan,
            Plan {
                land: vec![BatchId(1)],
                eject: vec![ejection(3, EjectReason::ConflictsWithTrunk)],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn member_of_a_landing_batch_is_not_ejected_for_an_earlier_batch() {
        let mut s = AllAtOnce::new();
        let plan = s.step(&observation(
            vec![ready(1), ready(2)],
            vec![
                testing(1, trunk(), &[1], CiStatus::Failed),
                testing(2, trunk(), &[1, 2], CiStatus::Passed),
            ],
        ));
        assert_eq!(
            plan,
            Plan {
                land: vec![BatchId(2)],
                retire: vec![BatchId(1)],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn passed_batch_on_stale_base_is_retired() {
        let mut s = AllAtOnce::new();
        let plan = s.step(&observation(
            vec![ready(1)],
            vec![testing(1, sha(999), &[1], CiStatus::Passed)],
        ));
        assert_eq!(
            plan,
            Plan {
                retire: vec![BatchId(1)],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn landed_batch_is_retired() {
        let mut s = AllAtOnce::new();
        let plan = s.step(&observation(
            vec![],
            vec![batch(
                1,
                sha(999),
                &[1],
                BatchState::Landed { tip: trunk() },
            )],
        ));
        assert_eq!(
            plan,
            Plan {
                retire: vec![BatchId(1)],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn refused_batch_is_retired_and_ejects_nothing() {
        let mut s = AllAtOnce::new();
        let refused = BatchState::LandRefused {
            tip: sha(2000),
            reason: LandRefusal::MemberGone { pr: PrNumber(2) },
        };
        let plan = s.step(&observation(
            vec![ready(1)],
            vec![batch(1, trunk(), &[1, 2], refused)],
        ));
        assert_eq!(
            plan,
            Plan {
                retire: vec![BatchId(1)],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn conflict_with_trunk_ejects_the_named_member() {
        let mut s = AllAtOnce::new();
        let conflict = BatchState::Conflict {
            pr: PrNumber(2),
            with: ConflictWith::Trunk,
        };
        let plan = s.step(&observation(
            vec![ready(1), ready(2)],
            vec![batch(1, trunk(), &[1, 2], conflict)],
        ));
        assert_eq!(
            plan,
            Plan {
                eject: vec![ejection(2, EjectReason::ConflictsWithTrunk)],
                retire: vec![BatchId(1)],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn conflict_with_earlier_member_names_it() {
        let mut s = AllAtOnce::new();
        let conflict = BatchState::Conflict {
            pr: PrNumber(3),
            with: ConflictWith::Pr(PrNumber(1)),
        };
        let plan = s.step(&observation(
            vec![ready(1), ready(3)],
            vec![batch(1, trunk(), &[1, 3], conflict)],
        ));
        assert_eq!(
            plan.eject,
            vec![ejection(3, EjectReason::ConflictsWith { pr: PrNumber(1) })]
        );
        assert_eq!(plan.retire, vec![BatchId(1)]);
    }

    #[test]
    fn ids_never_repeat_within_a_run() {
        let mut s = AllAtOnce::new();
        let first = s.step(&observation(vec![ready(1)], vec![]));
        assert_eq!(first.start[0].id, BatchId(1));
        // Batch 1 was never observed: the start did not run. A fresh id is used.
        let second = s.step(&observation(vec![ready(1)], vec![]));
        assert_eq!(second.start[0].id, BatchId(2));
    }

    #[test]
    fn tracked_batch_raises_the_next_id() {
        let mut s = AllAtOnce::new();
        let plan = s.step(&observation(
            vec![ready(1)],
            vec![batch(
                7,
                sha(999),
                &[1],
                BatchState::Landed { tip: trunk() },
            )],
        ));
        assert_eq!(plan.retire, vec![BatchId(7)]);
        let plan = s.step(&observation(vec![ready(1)], vec![]));
        assert_eq!(plan.start[0].id, BatchId(8));
    }

    #[test]
    fn equal_inputs_give_equal_plans() {
        let input = observation(
            vec![
                ready(1),
                pr(2, Mergeable::Conflicting, CiStatus::Passed),
                ready(3),
            ],
            vec![],
        );
        let mut a = AllAtOnce::new();
        let mut b = AllAtOnce::new();
        assert_eq!(a.step(&input), b.step(&input));
        assert_eq!(a, b);
    }
}
