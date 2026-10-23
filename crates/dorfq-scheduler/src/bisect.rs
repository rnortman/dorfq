//! [`Bisect`], the bisecting baseline.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use dorfq_core::{
    BatchId, BatchStart, BatchState, CiStatus, ConflictWith, EjectReason, Observation, Plan,
    PrNumber, Scheduler, Sha,
};

use crate::{Ejections, Ids, Member, chain, conflict_reason, is_pending, is_ready};

/// The conventional pipelined bisect: batches in queue order on one chain,
/// halves a failed batch, and never runs a member set known to fail.
///
/// Every batch it starts is built on the chain end: the newest tip that
/// still stands once this step's landings run, or trunk. A failed batch
/// whose tree below it passed is its own failure: one member is ejected,
/// several are split into two retest groups, which become the chain's next
/// entries ahead of fresh work. A failed batch whose ancestors are still
/// running is held until their verdicts are in; one whose base can no longer
/// land is retired with nothing learned, and everything above a failure is
/// cancelled. A retest batch retired without a verdict of its own puts its
/// group back at the front of the retest queue. No fresh batch is built on a
/// chain end whose tree holds a member set known to fail; that lane idles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bisect {
    lanes: usize,
    max_batch: usize,
    ids: Ids,
    /// Groups awaiting a run, front first.
    retests: VecDeque<Vec<Member>>,
    /// Retest batches started and not yet retired, with their groups.
    retesting: BTreeMap<BatchId, Vec<Member>>,
    /// Member sets of failed runs.
    failed: Vec<BTreeSet<Member>>,
    /// Failed batches already split into retest groups, so a failure seen
    /// again before its retirement ran is not split twice.
    split: BTreeSet<BatchId>,
}

impl Bisect {
    /// A bisecting scheduler keeping up to `lanes` batches pending, with at
    /// most `max_batch` members in a fresh batch.
    pub fn new(lanes: usize, max_batch: usize) -> Bisect {
        Bisect {
            lanes,
            max_batch,
            ids: Ids::new(),
            retests: VecDeque::new(),
            retesting: BTreeMap::new(),
            failed: Vec::new(),
            split: BTreeSet::new(),
        }
    }

    /// True when `members` includes every member of some failed set.
    fn contains_failed(&self, members: &[Member]) -> bool {
        self.failed
            .iter()
            .any(|set| set.iter().all(|m| members.contains(m)))
    }

    /// Puts `groups` at the front of the retest queue, in their order.
    fn requeue(&mut self, groups: Vec<Vec<Member>>) {
        for group in groups.into_iter().rev() {
            self.retests.push_front(group);
        }
    }
}

/// What this step does with one tracked batch.
enum Fate {
    /// Landable and landed by this plan.
    Land,
    /// Landable; left running or waiting for its base to land.
    Wait,
    /// Failed with every batch beneath it passed: its own members broke it.
    OwnFailure,
    /// Failed above a batch still pending: read again next step.
    Hold,
    /// Conflicted while building.
    Conflict(PrNumber, ConflictWith),
    /// Dead, landed or refused.
    Retire,
}

impl Scheduler for Bisect {
    fn step(&mut self, observation: &Observation) -> Plan {
        self.ids.raise(observation);
        let heads: BTreeMap<PrNumber, &Sha> = observation
            .prs
            .iter()
            .map(|p| (p.number, &p.head))
            .collect();
        let queued = |(pr, head): &Member| heads.get(pr) == Some(&head);
        let ready: BTreeSet<PrNumber> = observation
            .prs
            .iter()
            .filter(|p| is_ready(p))
            .map(|p| p.number)
            .collect();

        let tracked: BTreeSet<BatchId> = observation.batches.iter().map(|b| b.id).collect();
        self.split.retain(|id| tracked.contains(id));
        // A retest batch not observed was never started (rule 3); its group
        // is queued again below, with those retired without a verdict.
        let (started, mut requeued): (BTreeMap<_, _>, BTreeMap<_, _>) =
            std::mem::take(&mut self.retesting)
                .into_iter()
                .partition(|(id, _)| tracked.contains(id));
        self.retesting = started;
        self.failed.retain(|set| set.iter().all(queued));

        let mut plan = Plan::default();

        // Live tips: each landable batch's tip, and whether it and every
        // batch beneath it passed. Parents precede children in id order.
        // `after` is trunk once this plan's landings run: the plan lands a
        // single chain from trunk, so of two passed batches on one base only
        // the first lands, and the other waits to be retired as dead.
        let mut live: BTreeMap<&Sha, bool> = BTreeMap::new();
        let mut after = &observation.trunk;
        let mut fates = Vec::with_capacity(observation.batches.len());
        for batch in &observation.batches {
            let on_trunk = batch.base == observation.trunk;
            let base_live = on_trunk || live.contains_key(&batch.base);
            let below_passed = on_trunk || live.get(&batch.base) == Some(&true);
            let fate = match &batch.state {
                BatchState::Testing {
                    tip,
                    ci: CiStatus::Pending,
                } if base_live => {
                    live.insert(tip, false);
                    Fate::Wait
                }
                BatchState::Testing {
                    tip,
                    ci: CiStatus::Passed,
                } if base_live => {
                    live.insert(tip, below_passed);
                    if batch.base == *after {
                        after = tip;
                        plan.land.push(batch.id);
                        Fate::Land
                    } else {
                        Fate::Wait
                    }
                }
                BatchState::Testing {
                    ci: CiStatus::Failed,
                    ..
                } if base_live => {
                    if below_passed {
                        Fate::OwnFailure
                    } else {
                        Fate::Hold
                    }
                }
                BatchState::Conflict { pr, with } => Fate::Conflict(*pr, *with),
                BatchState::Testing { .. }
                | BatchState::Landed { .. }
                | BatchState::LandRefused { .. } => Fate::Retire,
            };
            fates.push(fate);
        }

        let mut ejections = Ejections::new(observation, &mut plan.eject);
        for (batch, fate) in observation.batches.iter().zip(&fates) {
            if matches!(fate, Fate::Land) {
                ejections.landing(batch);
            }
        }

        let mut in_flight: BTreeSet<PrNumber> = BTreeSet::new();
        let mut pending = 0;
        for (batch, fate) in observation.batches.iter().zip(&fates) {
            match fate {
                Fate::Land | Fate::Wait | Fate::Hold => {
                    in_flight.extend(batch.members.iter().map(|m| m.pr));
                    if is_pending(&batch.state) {
                        pending += 1;
                    }
                }
                Fate::OwnFailure => {
                    self.retesting.remove(&batch.id);
                    let members: Vec<Member> = batch
                        .members
                        .iter()
                        .map(|m| (m.pr, m.head.clone()))
                        .collect();
                    if let [single] = members.as_slice() {
                        if queued(single) {
                            ejections.eject(single.0, EjectReason::BatchFailed { batch: batch.id });
                        }
                    } else if self.split.insert(batch.id) {
                        let (first, rest) = members.split_at(members.len().div_ceil(2));
                        self.retests.push_front(rest.to_vec());
                        self.retests.push_front(first.to_vec());
                        self.failed.push(members.into_iter().collect());
                    }
                    plan.retire.push(batch.id);
                }
                Fate::Conflict(..) | Fate::Retire => {
                    if let Fate::Conflict(pr, with) = fate {
                        ejections.eject(*pr, conflict_reason(*with));
                    }
                    if let Some(group) = self.retesting.remove(&batch.id)
                        && !matches!(batch.state, BatchState::Landed { .. })
                    {
                        requeued.insert(batch.id, group);
                    }
                    plan.retire.push(batch.id);
                }
            }
        }
        // Ahead of any halves a split pushed this step, oldest first.
        self.requeue(requeued.into_values().collect());
        for group in &mut self.retests {
            group.retain(queued);
        }
        self.retests.retain(|group| !group.is_empty());

        ejections.eject_conflicting(observation);

        // The chain end: the newest tip still standing once this plan's
        // landings run, i.e. built on `after` through standing tips.
        let (chain_end, path) = chain(
            after,
            observation
                .batches
                .iter()
                .zip(&fates)
                .filter(|(_, fate)| matches!(fate, Fate::Wait))
                .map(|(batch, _)| batch),
        );

        // Start one batch on the chain end: the front retest group, else a
        // fresh batch.
        let mut taken = in_flight;
        taken.extend(plan.eject.iter().map(|e| e.pr));
        if pending >= self.lanes {
            return plan;
        }
        while let Some(group) = self.retests.pop_front() {
            let group: Vec<Member> = group
                .into_iter()
                .filter(|m| queued(m) && ready.contains(&m.0) && !taken.contains(&m.0))
                .collect();
            if group.is_empty() {
                continue;
            }
            if self.contains_failed(&group) {
                let (first, rest) = group.split_at(group.len().div_ceil(2));
                self.requeue(vec![first.to_vec(), rest.to_vec()]);
                continue;
            }
            let id = self.ids.mint();
            plan.start.push(BatchStart {
                id,
                base: chain_end.clone(),
                prs: group.iter().map(|(pr, _)| *pr).collect(),
            });
            self.retesting.insert(id, group);
            return plan;
        }

        // A batch built on a tree holding a known-failed set runs that set
        // again, so the lane idles.
        let tree: Vec<Member> = path
            .iter()
            .flat_map(|batch| batch.members.iter().map(|m| (m.pr, m.head.clone())))
            .collect();
        if self.contains_failed(&tree) {
            return plan;
        }
        let mut members: Vec<Member> = Vec::new();
        for pr in &observation.prs {
            if members.len() == self.max_batch {
                break;
            }
            if !ready.contains(&pr.number) || taken.contains(&pr.number) {
                continue;
            }
            members.push((pr.number, pr.head.clone()));
            if self.contains_failed(&members) {
                members.pop();
            }
        }
        if !members.is_empty() {
            plan.start.push(BatchStart {
                id: self.ids.mint(),
                base: chain_end.clone(),
                prs: members.into_iter().map(|(pr, _)| pr).collect(),
            });
        }

        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use dorfq_core::{LandRefusal, Mergeable};

    fn bisect() -> Bisect {
        Bisect::new(2, 16)
    }

    #[test]
    fn fresh_batch_takes_ready_prs_in_number_order_up_to_max_batch() {
        let mut s = Bisect::new(1, 3);
        let mut prs = ready_all(&[4, 1, 3, 2]);
        prs.sort_by_key(|p| p.number);
        prs.push(pr(5, Mergeable::Unknown, CiStatus::Passed));
        prs.push(pr(6, Mergeable::Clean, CiStatus::Pending));
        let plan = step(&mut s, &observation(prs, vec![]));
        assert_eq!(
            plan,
            Plan {
                start: vec![start(1, trunk(), &[1, 2, 3])],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn second_fresh_batch_bases_on_the_first_tip_one_per_step() {
        let mut s = Bisect::new(3, 2);
        let prs = ready_all(&[1, 2, 3, 4, 5, 6]);
        let plan = step(&mut s, &observation(prs.clone(), vec![]));
        assert_eq!(plan.start, vec![start(1, trunk(), &[1, 2])]);
        let first = testing(1, trunk(), &[1, 2], CiStatus::Pending);
        let plan = step(&mut s, &observation(prs.clone(), vec![first.clone()]));
        assert_eq!(plan.start, vec![start(2, tip(1), &[3, 4])]);
        let second = testing(2, tip(1), &[3, 4], CiStatus::Pending);
        let plan = step(
            &mut s,
            &observation(prs.clone(), vec![first.clone(), second.clone()]),
        );
        assert_eq!(plan.start, vec![start(3, tip(2), &[5, 6])]);
        let third = testing(3, tip(2), &[5, 6], CiStatus::Pending);
        let plan = step(&mut s, &observation(prs, vec![first, second, third]));
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[test]
    fn failed_batch_of_one_ejects_it() {
        let mut s = bisect();
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[1, 2]),
                vec![testing(4, trunk(), &[1], CiStatus::Failed)],
            ),
        );
        assert_eq!(plan.eject, vec![ejection(1, failed_by(4))]);
        assert_eq!(plan.retire, vec![BatchId(4)]);
        assert_eq!(plan.start, vec![start(5, trunk(), &[2])]);
    }

    #[test]
    fn after_a_split_the_halves_chain_ahead_of_fresh_work() {
        let mut s = Bisect::new(2, 16);
        let prs = ready_all(&[1, 2, 3, 4, 5, 6]);
        let plan = step(
            &mut s,
            &observation(
                prs.clone(),
                vec![testing(1, trunk(), &[1, 2, 3, 4, 5], CiStatus::Failed)],
            ),
        );
        assert!(plan.eject.is_empty());
        assert_eq!(plan.retire, vec![BatchId(1)]);
        assert_eq!(plan.start, vec![start(2, trunk(), &[1, 2, 3])]);
        let first = testing(2, trunk(), &[1, 2, 3], CiStatus::Pending);
        let plan = step(&mut s, &observation(prs.clone(), vec![first.clone()]));
        assert_eq!(plan.start, vec![start(3, tip(2), &[4, 5])]);
        let second = testing(3, tip(2), &[4, 5], CiStatus::Pending);
        let plan = step(&mut s, &observation(prs, vec![first, second]));
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[test]
    fn third_lane_idles_while_the_upper_half_stands() {
        let mut s = Bisect::new(3, 16);
        let prs = ready_all(&[1, 2, 3, 4, 5, 6]);
        let plan = step(
            &mut s,
            &observation(
                prs.clone(),
                vec![testing(1, trunk(), &[1, 2, 3, 4], CiStatus::Failed)],
            ),
        );
        assert_eq!(plan.start, vec![start(2, trunk(), &[1, 2])]);
        let first = testing(2, trunk(), &[1, 2], CiStatus::Pending);
        let plan = step(&mut s, &observation(prs.clone(), vec![first.clone()]));
        assert_eq!(plan.start, vec![start(3, tip(2), &[3, 4])]);
        // The upper half's tree is the whole that failed: nothing is built
        // on it although a lane is free.
        let second = testing(3, tip(2), &[3, 4], CiStatus::Pending);
        let plan = step(&mut s, &observation(prs, vec![first, second]));
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[test]
    fn fresh_batch_bases_on_the_lower_half_once_the_upper_group_empties() {
        let mut s = Bisect::new(3, 16);
        step(
            &mut s,
            &observation(
                ready_all(&[1, 2, 3, 4, 5, 6]),
                vec![testing(1, trunk(), &[1, 2, 3, 4], CiStatus::Failed)],
            ),
        );
        // 3 and 4 are withdrawn before their group runs: the lower half is
        // the newest standing tip and its tree holds no failed set.
        let first = testing(2, trunk(), &[1, 2], CiStatus::Pending);
        let plan = step(&mut s, &observation(ready_all(&[1, 2, 5, 6]), vec![first]));
        assert_eq!(plan.start, vec![start(3, tip(2), &[5, 6])]);
    }

    #[test]
    fn half_above_a_failing_half_runs_again_ahead_of_the_new_halves() {
        let mut s = Bisect::new(2, 16);
        let prs = ready_all(&[1, 2, 3, 4]);
        step(
            &mut s,
            &observation(
                prs.clone(),
                vec![testing(1, trunk(), &[1, 2, 3, 4], CiStatus::Failed)],
            ),
        );
        step(
            &mut s,
            &observation(
                prs.clone(),
                vec![testing(2, trunk(), &[1, 2], CiStatus::Pending)],
            ),
        );
        // The lower half fails: the upper half is cancelled, and its group
        // starts next on trunk, ahead of {1} and {2}.
        let plan = step(
            &mut s,
            &observation(
                prs.clone(),
                vec![
                    testing(2, trunk(), &[1, 2], CiStatus::Failed),
                    testing(3, tip(2), &[3, 4], CiStatus::Pending),
                ],
            ),
        );
        assert!(plan.eject.is_empty());
        assert_eq!(plan.retire, vec![BatchId(2), BatchId(3)]);
        assert_eq!(plan.start, vec![start(4, trunk(), &[3, 4])]);
        let plan = step(
            &mut s,
            &observation(prs, vec![testing(4, trunk(), &[3, 4], CiStatus::Pending)]),
        );
        assert_eq!(plan.start, vec![start(5, tip(4), &[1])]);
    }

    #[test]
    fn held_half_above_a_failing_half_is_retired_with_nothing_recorded() {
        let mut s = Bisect::new(2, 16);
        let prs = ready_all(&[1, 2, 3, 4]);
        step(
            &mut s,
            &observation(
                prs.clone(),
                vec![testing(1, trunk(), &[1, 2, 3, 4], CiStatus::Failed)],
            ),
        );
        step(
            &mut s,
            &observation(
                prs.clone(),
                vec![testing(2, trunk(), &[1, 2], CiStatus::Pending)],
            ),
        );
        let lower = testing(2, trunk(), &[1, 2], CiStatus::Pending);
        let upper = testing(3, tip(2), &[3, 4], CiStatus::Failed);
        let plan = step(
            &mut s,
            &observation(prs.clone(), vec![lower, upper.clone()]),
        );
        assert!(plan.is_empty(), "{plan:?}");
        let lower = testing(2, trunk(), &[1, 2], CiStatus::Failed);
        let plan = step(&mut s, &observation(prs, vec![lower, upper]));
        assert_eq!(plan.retire, vec![BatchId(2), BatchId(3)]);
        // {3, 4} is not split: its failure was the lower half's.
        assert_eq!(plan.start, vec![start(4, trunk(), &[3, 4])]);
    }

    #[test]
    fn half_above_a_half_this_plan_lands_is_read_in_the_same_plan() {
        let mut s = Bisect::new(2, 16);
        let prs = ready_all(&[1, 2, 3, 4]);
        step(
            &mut s,
            &observation(
                prs.clone(),
                vec![testing(1, trunk(), &[1, 2, 3, 4], CiStatus::Failed)],
            ),
        );
        step(
            &mut s,
            &observation(
                prs.clone(),
                vec![testing(2, trunk(), &[1, 2], CiStatus::Pending)],
            ),
        );
        let plan = step(
            &mut s,
            &observation(
                prs,
                vec![
                    testing(2, trunk(), &[1, 2], CiStatus::Passed),
                    testing(3, tip(2), &[3, 4], CiStatus::Failed),
                ],
            ),
        );
        assert_eq!(plan.land, vec![BatchId(2)]);
        assert_eq!(plan.retire, vec![BatchId(3)]);
        assert_eq!(plan.start, vec![start(4, tip(2), &[3])]);
    }

    #[test]
    fn group_whose_start_was_never_observed_runs_again() {
        let mut s = Bisect::new(1, 16);
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[1, 2]),
                vec![testing(1, trunk(), &[1, 2], CiStatus::Failed)],
            ),
        );
        assert_eq!(plan.start, vec![start(2, trunk(), &[1])]);
        // Batch 1 was retired but batch 2 never appears (rule 3).
        let plan = step(&mut s, &observation(ready_all(&[1, 2]), vec![]));
        assert_eq!(plan.start, vec![start(3, trunk(), &[1])]);
    }

    #[test]
    fn retest_groups_wait_front_first_for_a_free_lane() {
        let mut s = Bisect::new(1, 16);
        let prs = ready_all(&[1, 2, 3, 4]);
        let plan = step(
            &mut s,
            &observation(
                prs.clone(),
                vec![testing(1, trunk(), &[1, 2, 3, 4], CiStatus::Failed)],
            ),
        );
        assert_eq!(plan.start, vec![start(2, trunk(), &[1, 2])]);
        // The first half passes and lands; the second half runs next, on
        // the tip being landed, and no fresh batch takes its members.
        let plan = step(
            &mut s,
            &observation(prs, vec![testing(2, trunk(), &[1, 2], CiStatus::Passed)]),
        );
        assert_eq!(plan.land, vec![BatchId(2)]);
        assert_eq!(plan.start, vec![start(3, tip(2), &[3, 4])]);
    }

    #[test]
    fn failed_retest_group_is_recorded_and_split_again() {
        let mut s = Bisect::new(1, 16);
        let prs = ready_all(&[1, 2, 3, 4]);
        step(
            &mut s,
            &observation(
                prs.clone(),
                vec![testing(1, trunk(), &[1, 2, 3, 4], CiStatus::Failed)],
            ),
        );
        // Halves {1, 2} and {3, 4} are queued; {1, 2} runs and fails too,
        // so {1, 2} is known to fail and splits into {1} and {2}.
        let plan = step(
            &mut s,
            &observation(
                prs.clone(),
                vec![testing(2, trunk(), &[1, 2], CiStatus::Failed)],
            ),
        );
        assert_eq!(plan.start, vec![start(3, trunk(), &[1])]);
        // {1} passes and lands; {2} runs next, then {3, 4}.
        let plan = step(
            &mut s,
            &observation(prs, vec![testing(3, trunk(), &[1], CiStatus::Passed)]),
        );
        assert_eq!(plan.land, vec![BatchId(3)]);
        assert_eq!(plan.start, vec![start(4, tip(3), &[2])]);
        let plan = step(
            &mut s,
            &Observation {
                trunk: tip(3),
                ..observation(
                    ready_all(&[2, 3, 4]),
                    vec![
                        batch(3, trunk(), &[1], BatchState::Landed { tip: tip(3) }),
                        testing(4, tip(3), &[2], CiStatus::Failed),
                    ],
                )
            },
        );
        assert_eq!(plan.eject, vec![ejection(2, failed_by(4))]);
        assert_eq!(plan.retire, vec![BatchId(3), BatchId(4)]);
        assert_eq!(plan.start, vec![start(5, tip(3), &[3, 4])]);
    }

    #[test]
    fn group_split_again_when_it_holds_a_failed_set() {
        // The memory is seeded: no short sequence of observations leaves a
        // queued group holding a recorded failed set.
        let mut s = Bisect::new(1, 16);
        s.failed.push(
            [(PrNumber(1), sha(1)), (PrNumber(2), sha(2))]
                .into_iter()
                .collect(),
        );
        s.retests.push_back(vec![
            (PrNumber(1), sha(1)),
            (PrNumber(2), sha(2)),
            (PrNumber(3), sha(3)),
        ]);
        let plan = step(&mut s, &observation(ready_all(&[1, 2, 3]), vec![]));
        // {1, 2, 3} holds {1, 2}: split into {1, 2} and {3}; {1, 2} is the
        // failed set itself, split into {1} and {2}; {1} runs.
        assert_eq!(plan.start, vec![start(1, trunk(), &[1])]);
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[1, 2, 3]),
                vec![testing(1, trunk(), &[1], CiStatus::Passed)],
            ),
        );
        assert_eq!(plan.start, vec![start(2, tip(1), &[2])]);
        let plan = step(
            &mut s,
            &Observation {
                trunk: tip(1),
                ..observation(
                    ready_all(&[2, 3]),
                    vec![
                        batch(1, trunk(), &[1], BatchState::Landed { tip: tip(1) }),
                        testing(2, tip(1), &[2], CiStatus::Passed),
                    ],
                )
            },
        );
        assert_eq!(plan.start, vec![start(3, tip(2), &[3])]);
    }

    #[test]
    fn fresh_batch_skips_a_member_that_completes_a_failed_set() {
        let mut s = Bisect::new(2, 16);
        // {1, 3} fails while both are briefly not mergeable, so their groups
        // empty out and they return to the fresh pool still known to fail
        // together.
        let unknown = |n| pr(n, Mergeable::Unknown, CiStatus::Passed);
        let plan = step(
            &mut s,
            &observation(
                vec![unknown(1), ready(2), unknown(3), ready(4)],
                vec![testing(1, trunk(), &[1, 3], CiStatus::Failed)],
            ),
        );
        assert_eq!(plan.start, vec![start(2, trunk(), &[2, 4])]);
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[1, 2, 3, 4, 5]),
                vec![testing(2, trunk(), &[2, 4], CiStatus::Pending)],
            ),
        );
        assert_eq!(plan.start, vec![start(3, tip(2), &[1, 5])]);
    }

    #[test]
    fn passed_chain_lands_in_one_plan_in_id_order() {
        let mut s = bisect();
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[1, 2, 3]),
                vec![
                    testing(1, trunk(), &[1], CiStatus::Passed),
                    testing(2, tip(1), &[2], CiStatus::Passed),
                    testing(3, tip(2), &[3], CiStatus::Passed),
                ],
            ),
        );
        assert_eq!(
            plan,
            Plan {
                land: vec![BatchId(1), BatchId(2), BatchId(3)],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn passed_child_of_a_pending_parent_waits() {
        let mut s = Bisect::new(2, 16);
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[1, 2]),
                vec![
                    testing(1, trunk(), &[1], CiStatus::Pending),
                    testing(2, tip(1), &[2], CiStatus::Passed),
                ],
            ),
        );
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[test]
    fn passed_batch_on_a_dead_base_is_retired() {
        let mut s = bisect();
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[1]),
                vec![testing(1, sha(999), &[1], CiStatus::Passed)],
            ),
        );
        assert_eq!(plan.retire, vec![BatchId(1)]);
        assert!(plan.land.is_empty());
        // Its member returns to the pool.
        assert_eq!(plan.start, vec![start(2, trunk(), &[1])]);
    }

    #[test]
    fn pending_child_of_a_failed_parent_is_retired() {
        let mut s = bisect();
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[1, 2, 3]),
                vec![
                    testing(1, trunk(), &[1, 2], CiStatus::Failed),
                    testing(2, tip(1), &[3], CiStatus::Pending),
                ],
            ),
        );
        assert_eq!(plan.retire, vec![BatchId(1), BatchId(2)]);
        assert_eq!(plan.start, vec![start(3, trunk(), &[1])]);
        assert!(plan.eject.is_empty());
    }

    #[test]
    fn failed_child_of_a_pending_parent_is_held_then_read_as_its_own() {
        let mut s = Bisect::new(3, 16);
        let prs = ready_all(&[1, 2, 3, 4]);
        let parent = testing(1, trunk(), &[1], CiStatus::Pending);
        let child = testing(2, tip(1), &[2, 3], CiStatus::Failed);
        let plan = step(
            &mut s,
            &observation(prs.clone(), vec![parent, child.clone()]),
        );
        assert!(plan.retire.is_empty());
        assert!(plan.eject.is_empty());
        // The held child holds no lane and is not a standing tip: the fresh
        // batch takes 4 only, on the parent's tip.
        assert_eq!(plan.start, vec![start(3, tip(1), &[4])]);

        let parent = testing(1, trunk(), &[1], CiStatus::Passed);
        let fresh = testing(3, tip(1), &[4], CiStatus::Pending);
        let plan = step(&mut s, &observation(prs, vec![parent, child, fresh]));
        assert_eq!(plan.land, vec![BatchId(1)]);
        assert_eq!(plan.retire, vec![BatchId(2)]);
        assert_eq!(plan.start, vec![start(4, tip(3), &[2])]);
    }

    #[test]
    fn failed_child_of_a_failed_parent_is_retired_with_nothing_recorded() {
        let mut s = bisect();
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[1, 2, 3]),
                vec![
                    testing(1, trunk(), &[1], CiStatus::Failed),
                    testing(2, tip(1), &[2, 3], CiStatus::Failed),
                ],
            ),
        );
        assert_eq!(plan.eject, vec![ejection(1, failed_by(1))]);
        assert_eq!(plan.retire, vec![BatchId(1), BatchId(2)]);
        // Nothing recorded: 2 and 3 run together again as a fresh batch.
        assert_eq!(plan.start, vec![start(3, trunk(), &[2, 3])]);
    }

    #[test]
    fn of_two_passed_siblings_one_lands_and_the_other_runs_again() {
        // Seeded: the chained layout never puts two batches on one base,
        // but the landing rule must hold if something does.
        let mut s = Bisect::new(2, 16);
        s.retesting.insert(BatchId(3), vec![(PrNumber(2), sha(2))]);
        // Both pass on one base: only the first lands, and nothing is built
        // on the second's tip, which can never become trunk.
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[1, 2]),
                vec![
                    testing(2, trunk(), &[1], CiStatus::Passed),
                    testing(3, trunk(), &[2], CiStatus::Passed),
                ],
            ),
        );
        assert_eq!(
            plan,
            Plan {
                land: vec![BatchId(2)],
                ..Plan::default()
            }
        );
        // The second is now on a dead base: retired, and its group runs
        // again on the new trunk.
        let plan = step(
            &mut s,
            &Observation {
                trunk: tip(2),
                ..observation(
                    ready_all(&[2]),
                    vec![
                        batch(2, trunk(), &[1], BatchState::Landed { tip: tip(2) }),
                        testing(3, trunk(), &[2], CiStatus::Passed),
                    ],
                )
            },
        );
        assert_eq!(plan.retire, vec![BatchId(2), BatchId(3)]);
        assert_eq!(plan.start, vec![start(4, tip(2), &[2])]);
    }

    #[test]
    fn fresh_batch_is_not_built_on_a_tip_this_plan_strands() {
        // Seeded siblings, as above: 3 lands while 4, on the same trunk, is
        // pending, so 4's base will not be trunk and the fresh batch builds
        // on 3's tip.
        let mut s = Bisect::new(3, 16);
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[1, 2, 3, 4, 5]),
                vec![
                    testing(2, trunk(), &[1, 2], CiStatus::Pending),
                    testing(3, trunk(), &[3], CiStatus::Passed),
                    testing(4, trunk(), &[4], CiStatus::Pending),
                ],
            ),
        );
        assert_eq!(
            plan,
            Plan {
                land: vec![BatchId(3)],
                start: vec![start(5, tip(3), &[5])],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn withdrawn_prs_are_neither_started_nor_ejected() {
        let mut s = Bisect::new(1, 16);
        let busy = testing(9, trunk(), &[5], CiStatus::Pending);
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[1, 2, 3, 4, 5]),
                vec![
                    testing(1, trunk(), &[1, 2, 3, 4], CiStatus::Failed),
                    busy.clone(),
                ],
            ),
        );
        assert!(plan.start.is_empty(), "{plan:?}");
        // 2 is withdrawn from its group {1, 2}; {1} runs alone.
        let plan = step(&mut s, &observation(ready_all(&[1, 3, 4]), vec![]));
        assert_eq!(plan.start, vec![start(10, trunk(), &[1])]);
        // 1 is withdrawn while its singleton failed: no ejection.
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[3, 4]),
                vec![testing(10, trunk(), &[1], CiStatus::Failed)],
            ),
        );
        assert_eq!(
            plan,
            Plan {
                retire: vec![BatchId(10)],
                start: vec![start(11, trunk(), &[3, 4])],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn conflicts_and_finished_batches_are_handled() {
        let mut s = bisect();
        let refused = BatchState::LandRefused {
            tip: tip(3),
            reason: LandRefusal::TrunkMoved { trunk: trunk() },
        };
        let plan = step(
            &mut s,
            &observation(
                vec![
                    ready(1),
                    ready(2),
                    ready(3),
                    pr(4, Mergeable::Conflicting, CiStatus::Passed),
                    ready(5),
                ],
                vec![
                    batch(
                        1,
                        trunk(),
                        &[1, 2],
                        BatchState::Conflict {
                            pr: PrNumber(2),
                            with: ConflictWith::Pr(PrNumber(1)),
                        },
                    ),
                    batch(2, sha(999), &[6], BatchState::Landed { tip: trunk() }),
                    batch(3, sha(999), &[3], refused),
                    batch(
                        4,
                        trunk(),
                        &[5],
                        BatchState::Conflict {
                            pr: PrNumber(5),
                            with: ConflictWith::Trunk,
                        },
                    ),
                ],
            ),
        );
        assert_eq!(
            plan.eject,
            vec![
                ejection(2, EjectReason::ConflictsWith { pr: PrNumber(1) }),
                ejection(5, EjectReason::ConflictsWithTrunk),
                ejection(4, EjectReason::ConflictsWithTrunk),
            ]
        );
        assert_eq!(
            plan.retire,
            vec![BatchId(1), BatchId(2), BatchId(3), BatchId(4)]
        );
        assert_eq!(plan.start, vec![start(5, trunk(), &[1, 3])]);
    }

    #[test]
    fn members_of_a_landing_batch_are_never_ejected() {
        let mut s = bisect();
        let plan = step(
            &mut s,
            &observation(
                vec![pr(1, Mergeable::Conflicting, CiStatus::Passed), ready(2)],
                vec![
                    testing(1, trunk(), &[1, 2], CiStatus::Passed),
                    testing(2, trunk(), &[2], CiStatus::Failed),
                ],
            ),
        );
        assert_eq!(plan.land, vec![BatchId(1)]);
        assert_eq!(plan.retire, vec![BatchId(2)]);
        assert!(plan.eject.is_empty(), "{plan:?}");
    }

    #[test]
    fn repushed_prs_leave_groups_and_memory() {
        let mut s = Bisect::new(3, 16);
        let busy = vec![
            testing(10, trunk(), &[5], CiStatus::Pending),
            testing(11, trunk(), &[6], CiStatus::Pending),
            testing(12, trunk(), &[7], CiStatus::Pending),
        ];
        let mut batches = vec![testing(1, trunk(), &[1, 2, 3, 4], CiStatus::Failed)];
        batches.extend(busy.iter().cloned());
        let plan = step(
            &mut s,
            &observation(ready_all(&[1, 2, 3, 4, 5, 6, 7]), batches),
        );
        assert!(plan.start.is_empty(), "{plan:?}");
        // 2 is re-pushed: it leaves its group {1, 2}, and joins the fresh
        // pool once the lanes free.
        let prs = vec![
            ready(1),
            repushed(2),
            ready(3),
            ready(4),
            ready(5),
            ready(6),
            ready(7),
        ];
        let plan = step(&mut s, &observation(prs.clone(), busy));
        assert!(plan.is_empty(), "{plan:?}");
        let plan = step(&mut s, &observation(prs.clone(), vec![]));
        assert_eq!(plan.start, vec![start(13, trunk(), &[1])]);
        let lower = testing(13, trunk(), &[1], CiStatus::Pending);
        let plan = step(&mut s, &observation(prs.clone(), vec![lower.clone()]));
        assert_eq!(plan.start, vec![start(14, tip(13), &[3, 4])]);
        // The tree {1, 3, 4} would hold the failed set {1, 2, 3, 4} at 2's
        // old head; that set was forgotten, so the third lane is not idled.
        let upper = testing(14, tip(13), &[3, 4], CiStatus::Pending);
        let plan = step(&mut s, &observation(prs, vec![lower, upper]));
        assert_eq!(plan.start, vec![start(15, tip(14), &[2, 5, 6, 7])]);
    }

    #[test]
    fn repushed_singleton_is_not_ejected_for_its_old_head() {
        let mut s = bisect();
        let plan = step(
            &mut s,
            &observation(
                vec![repushed(1)],
                vec![testing(1, trunk(), &[1], CiStatus::Failed)],
            ),
        );
        assert!(plan.eject.is_empty());
        assert_eq!(plan.retire, vec![BatchId(1)]);
        assert_eq!(plan.start, vec![start(2, trunk(), &[1])]);
    }

    #[test]
    fn failure_seen_again_is_not_split_twice() {
        let mut s = Bisect::new(1, 16);
        let failed = testing(1, trunk(), &[1, 2], CiStatus::Failed);
        let prs = ready_all(&[1, 2]);
        let plan = step(&mut s, &observation(prs.clone(), vec![failed.clone()]));
        assert_eq!(plan.start, vec![start(2, trunk(), &[1])]);
        // The plan was cut short: the retire and the start of {1} never
        // ran. {1} is queued again and runs; the failure is not split again.
        let plan = step(&mut s, &observation(prs, vec![failed]));
        assert_eq!(plan.retire, vec![BatchId(1)]);
        assert_eq!(plan.start, vec![start(3, trunk(), &[1])]);
        // A second split would only duplicate queued groups and the failed
        // set, which no plan tells apart; the memory is read directly.
        assert_eq!(s.failed.len(), 1);
        assert_eq!(s.retests, VecDeque::from(vec![vec![(PrNumber(2), sha(2))]]));
    }

    #[test]
    fn run_started_beside_a_landing_bases_on_the_landing_tip() {
        let mut s = bisect();
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[1, 2]),
                vec![testing(1, trunk(), &[1], CiStatus::Passed)],
            ),
        );
        assert_eq!(plan.land, vec![BatchId(1)]);
        assert_eq!(plan.start, vec![start(2, tip(1), &[2])]);
    }

    #[test]
    fn lanes_count_pending_batches_only() {
        let mut s = Bisect::new(1, 16);
        let plan = step(
            &mut s,
            &observation(
                ready_all(&[1, 2]),
                vec![testing(1, trunk(), &[1], CiStatus::Pending)],
            ),
        );
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[test]
    fn no_plan_starts_a_member_in_flight() {
        let mut s = Bisect::new(4, 16);
        let batches = vec![
            testing(1, trunk(), &[1, 2], CiStatus::Pending),
            testing(2, tip(1), &[3], CiStatus::Passed),
            testing(3, tip(1), &[4, 5], CiStatus::Failed),
        ];
        // Seeded: a queued group holds a member in flight; the start must
        // skip it whatever put it there.
        s.retests
            .push_back(vec![(PrNumber(3), sha(3)), (PrNumber(6), sha(6))]);
        let plan = step(
            &mut s,
            &observation(ready_all(&[1, 2, 3, 4, 5, 6, 7]), batches),
        );
        assert_eq!(plan.start, vec![start(4, tip(2), &[6])]);
    }

    #[test]
    fn a_fresh_scheduler_starts_above_every_tracked_id() {
        let mut s = bisect();
        let landed = batch(7, trunk(), &[9], BatchState::Landed { tip: tip(7) });
        let plan = step(&mut s, &observation(ready_all(&[1, 2]), vec![landed]));
        assert_eq!(plan.start, vec![start(8, trunk(), &[1, 2])]);
    }

    #[test]
    fn equal_inputs_give_equal_plans() {
        let input = observation(
            vec![
                ready(1),
                pr(2, Mergeable::Conflicting, CiStatus::Passed),
                ready(3),
                ready(4),
            ],
            vec![testing(7, trunk(), &[3, 4], CiStatus::Failed)],
        );
        let mut a = bisect();
        let mut b = bisect();
        assert_eq!(a.step(&input), b.step(&input));
        assert_eq!(a, b);
    }
}
