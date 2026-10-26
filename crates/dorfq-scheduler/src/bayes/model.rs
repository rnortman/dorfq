//! Causes, beliefs and the evidence of failed runs.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use dorfq_core::{BatchId, Hold, PrNumber, PrSnapshot, PrStatus, Sha};

use crate::{ancestors, bases};

/// The belief an alibi leaves a pair, which inference treats as false; and
/// the complement of the cap on every belief, which keeps inference's
/// `ln(1 − p)` finite.
pub(crate) const FLOOR: f64 = 1e-6;

/// The most failed runs the evidence keeps; the oldest goes first.
pub(crate) const MAX_EVIDENCE: usize = 64;

/// Something that can make a run fail. Orders `Bad` before `Pair`, then by
/// number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Cause {
    /// The pull request fails against trunk on its own.
    Bad(PrNumber),
    /// The two fail only together; the smaller number first.
    Pair(PrNumber, PrNumber),
}

impl Cause {
    /// The pair cause of `a` and `b`, in canonical order.
    pub(crate) fn pair(a: PrNumber, b: PrNumber) -> Cause {
        if a < b {
            Cause::Pair(a, b)
        } else {
            Cause::Pair(b, a)
        }
    }

    /// Whether `pr` is the pull request or one of the pair.
    pub(crate) fn involves(&self, pr: PrNumber) -> bool {
        match *self {
            Cause::Bad(i) => i == pr,
            Cause::Pair(i, j) => i == pr || j == pr,
        }
    }

    /// The pull request, or the pair's two.
    pub(crate) fn prs(&self) -> Vec<PrNumber> {
        match *self {
            Cause::Bad(i) => vec![i],
            Cause::Pair(i, j) => vec![i, j],
        }
    }
}

/// The prior probabilities the model starts from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Priors {
    /// A pair fails only together; also the chance a landing breaks a
    /// queued pull request that no run has tested with it.
    pub(crate) p_interact: f64,
    /// A run fails for no reason in the tree.
    pub(crate) p_flake: f64,
}

/// A run that failed, with the causes its tree exercised and the trunk
/// position it started on, credited with its own tree's landings since.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FailedRun {
    pub(crate) batch: BatchId,
    pub(crate) exercised: BTreeSet<Cause>,
    pub(crate) at: i64,
}

/// A queued pull request at the head the model holds.
#[derive(Debug, Clone, PartialEq)]
struct Held {
    head: Sha,
    /// The trunk position of its latest alibi: the trunk it is known good
    /// against.
    cleared: i64,
}

impl Held {
    /// Moves the cleared position to `at` if that is further along.
    fn clear(&mut self, at: i64) {
        self.cleared = self.cleared.max(at);
    }
}

/// One cleared position per queued pull request, materialised pairs, and
/// the failed runs that are evidence about them.
///
/// Positions count the pull requests that reached trunk since the scheduler
/// started; one cleared before that is at a negative position.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Model {
    pub(crate) priors: Priors,
    /// The trunk position as of the latest observation.
    position: i64,
    /// Each queued pull request the model holds.
    bad: BTreeMap<PrNumber, Held>,
    /// Pairs a failed run exercised, with their belief.
    pairs: BTreeMap<Cause, f64>,
    /// Failed runs, oldest first.
    evidence: VecDeque<FailedRun>,
    /// Each queued pull request's queued ancestors, by the latest bases.
    ancestors: BTreeMap<PrNumber, BTreeSet<PrNumber>>,
}

impl Model {
    pub(crate) fn new(priors: Priors) -> Model {
        Model {
            priors,
            position: 0,
            bad: BTreeMap::new(),
            pairs: BTreeMap::new(),
            evidence: VecDeque::new(),
            ancestors: BTreeMap::new(),
        }
    }

    /// Aligns the model with the queued pull requests at trunk `position`.
    /// A new one, or a re-pushed one, is cleared at the trunk its own CI ran
    /// on, [`PrSnapshot::behind`] pull requests back; one whose own CI has
    /// passed on its head is cleared there if that is further along: the
    /// run is an alibi against the trunk it started on. A pull request no
    /// longer at the head the model holds leaves every pair; its causes
    /// leave every run if it is in `landed`, and otherwise every run that
    /// exercised one of them leaves with it. `landed` is what reached
    /// trunk, by the queue or by another hand. The bases of `prs` replace
    /// the stack relation the model holds.
    pub(crate) fn sync(&mut self, prs: &[PrSnapshot], landed: &BTreeSet<PrNumber>, position: i64) {
        self.position = position;
        let base = bases(prs);
        self.ancestors = prs
            .iter()
            .map(|p| (p.number, ancestors(&base, p.base).collect()))
            .collect();
        let current: BTreeMap<PrNumber, &Sha> = prs.iter().map(|p| (p.number, &p.head)).collect();
        let gone: Vec<PrNumber> = self
            .bad
            .iter()
            .filter(|(pr, held)| current.get(pr) != Some(&&held.head))
            .map(|(pr, _)| *pr)
            .collect();
        for pr in gone {
            if landed.contains(&pr) {
                for run in &mut self.evidence {
                    run.exercised.retain(|cause| !cause.involves(pr));
                }
            } else {
                self.evidence
                    .retain(|run| !run.exercised.iter().any(|cause| cause.involves(pr)));
            }
            self.pairs.retain(|cause, _| !cause.involves(pr));
            self.bad.remove(&pr);
        }
        // TODO(trunk-position-ownership): `behind` is read here at
        // first sight and afterwards only as an alibi (`clear` is a `max`);
        // a rise on an unchanged head — a trunk change the queue did not
        // make — never moves a cleared position back, and the trunk
        // position counts only observed departures, so a pull request
        // cleared at the current trunk cannot be blamed for anything that
        // breaks it after that.
        for pr in prs {
            let own = position - i64::from(pr.behind);
            let held = self.bad.entry(pr.number).or_insert_with(|| Held {
                head: pr.head.clone(),
                cleared: own,
            });
            if own_ci_passed(&pr.status) {
                held.clear(own);
            }
        }
        self.prune();
    }

    /// Whether one of `a`, `b` is a queued ancestor of the other: the
    /// child's own CI tested the two together, so they are no pair cause.
    pub(crate) fn related(&self, a: PrNumber, b: PrNumber) -> bool {
        let under =
            |x: PrNumber, y: PrNumber| self.ancestors.get(&x).is_some_and(|s| s.contains(&y));
        under(a, b) || under(b, a)
    }

    /// The causes a run over `tree` exercises: `Bad` of every member the
    /// model holds at that head, and every pair of them not in one stack.
    pub(crate) fn exercised(&self, tree: &[(PrNumber, Sha)]) -> BTreeSet<Cause> {
        let known: Vec<PrNumber> = tree
            .iter()
            .filter(|(pr, head)| self.bad.get(pr).is_some_and(|held| held.head == *head))
            .map(|(pr, _)| *pr)
            .collect();
        let mut causes: BTreeSet<Cause> = known.iter().map(|&pr| Cause::Bad(pr)).collect();
        for (n, &a) in known.iter().enumerate() {
            for &b in known[n + 1..].iter().filter(|&&b| !self.related(a, b)) {
                causes.insert(Cause::pair(a, b));
            }
        }
        causes
    }

    /// The chance that `k` landings, each breaking with `p_interact`, broke
    /// a pull request: 0 for no landing, up to `1 − FLOOR`.
    pub(crate) fn regrown(&self, k: i64) -> f64 {
        (1.0 - (1.0 - self.priors.p_interact).powf(k.max(0) as f64)).min(1.0 - FLOOR)
    }

    /// The trunk position as of the latest observation.
    pub(crate) fn position(&self) -> i64 {
        self.position
    }

    /// The position queued pull request `pr` is cleared at, if the model
    /// holds it.
    pub(crate) fn cleared(&self, pr: PrNumber) -> Option<i64> {
        self.bad.get(&pr).map(|held| held.cleared)
    }

    /// A run over `tree` passed, started on the trunk at position `covered`
    /// credited with its tree's landings since: every `Bad` it exercised is
    /// cleared there unless a newer alibi cleared it further along, and
    /// every pair it exercised is set to the floor.
    pub(crate) fn passed(&mut self, tree: &[(PrNumber, Sha)], covered: i64) {
        // TODO(bayes-false-green): a pass clears outright; CI can miss a
        // break, so a pass should be an observation that lowers belief.
        for cause in self.exercised(tree) {
            match cause {
                Cause::Bad(pr) => {
                    if let Some(held) = self.bad.get_mut(&pr) {
                        held.clear(covered);
                    }
                }
                Cause::Pair(..) => {
                    if let Some(p) = self.pairs.get_mut(&cause) {
                        *p = FLOOR;
                    }
                }
            }
        }
        self.prune();
    }

    /// Batch `batch` failed over `tree`, started at position `at`: the run
    /// joins the evidence, and the pairs it exercised that were not yet
    /// materialised start at the prior.
    pub(crate) fn failed(&mut self, batch: BatchId, tree: &[(PrNumber, Sha)], at: i64) {
        let exercised = self.exercised(tree);
        for cause in &exercised {
            if let Cause::Pair(..) = cause {
                self.pairs.entry(*cause).or_insert(self.priors.p_interact);
            }
        }
        self.evidence.push_back(FailedRun {
            batch,
            exercised,
            at,
        });
        while self.evidence.len() > MAX_EVIDENCE {
            self.evidence.pop_front();
        }
        self.prune();
    }

    /// The belief in `cause` now: for `Bad`, the chance a landing since its
    /// cleared position broke it; a pair not materialised is at the prior;
    /// a pull request not queued, and a pair within one stack, are not
    /// causes.
    pub(crate) fn belief(&self, cause: Cause) -> f64 {
        match cause {
            Cause::Bad(pr) => self
                .bad
                .get(&pr)
                .map_or(0.0, |held| self.regrown(self.position - held.cleared)),
            Cause::Pair(i, j) => {
                if self.bad.contains_key(&i) && self.bad.contains_key(&j) && !self.related(i, j) {
                    self.pairs
                        .get(&cause)
                        .copied()
                        .unwrap_or(self.priors.p_interact)
                } else {
                    0.0
                }
            }
        }
    }

    /// The failed runs, oldest first.
    pub(crate) fn evidence(&self) -> &VecDeque<FailedRun> {
        &self.evidence
    }

    /// Whether `cause` could have failed a run started at position `at`:
    /// a `Bad` cleared before that trunk, or a pair above the floor.
    fn blames(&self, cause: Cause, at: i64) -> bool {
        match cause {
            Cause::Bad(pr) => self.cleared(pr).is_some_and(|cleared| cleared < at),
            Cause::Pair(..) => self.belief(cause) > FLOOR,
        }
    }

    /// The causes of `run` that could have failed it.
    pub(crate) fn effective<'a>(&'a self, run: &'a FailedRun) -> impl Iterator<Item = Cause> + 'a {
        run.exercised
            .iter()
            .copied()
            .filter(|&cause| self.blames(cause, run.at))
    }

    /// The causes a run over `tree` started now would blame, were it to
    /// fail.
    pub(crate) fn would_blame(&self, tree: &[(PrNumber, Sha)]) -> BTreeSet<Cause> {
        self.exercised(tree)
            .into_iter()
            .filter(|&cause| self.blames(cause, self.position))
            .collect()
    }

    /// Drops every run with nothing left to blame: it was a flake.
    fn prune(&mut self) {
        let empty: Vec<BatchId> = self
            .evidence
            .iter()
            .filter(|run| self.effective(run).next().is_none())
            .map(|run| run.batch)
            .collect();
        self.evidence.retain(|run| !empty.contains(&run.batch));
    }
}

/// Whether the forge reports the pull request's own CI passed on its head:
/// ready, or held for something other than its own CI. A conflicting pull
/// request's own CI is not reported.
fn own_ci_passed(status: &PrStatus) -> bool {
    match status {
        PrStatus::Ready => true,
        PrStatus::Waiting(holds) => {
            !holds.contains(&Hold::OwnCiPending) && !holds.contains(&Hold::OwnCiFailed)
        }
        PrStatus::Conflicting(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{bad, members, model, pair, prior, priors};
    use super::*;
    use crate::test_support::{
        BEHIND, fresh, held, pr, ready, ready_after, ready_all, repushed, stacked, stale,
    };
    use dorfq_core::PrBase;

    fn batches(model: &Model) -> Vec<u64> {
        model.evidence().iter().map(|run| run.batch.0).collect()
    }

    fn none() -> BTreeSet<PrNumber> {
        BTreeSet::new()
    }

    fn landing(prs: &[u64]) -> BTreeSet<PrNumber> {
        prs.iter().copied().map(PrNumber).collect()
    }

    fn effective(model: &Model, run: usize) -> BTreeSet<Cause> {
        model.effective(&model.evidence()[run]).collect()
    }

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-15, "{a} against {b}");
    }

    #[test]
    fn new_pull_requests_start_at_the_prior_and_pairs_at_theirs() {
        let model = model(&[1, 2]);
        assert_eq!(model.belief(bad(1)), prior());
        assert_eq!(model.belief(pair(2, 1)), 0.002);
        assert_eq!(model.belief(bad(3)), 0.0);
        assert_eq!(model.belief(pair(1, 3)), 0.0);
    }

    #[test]
    fn causes_order_bad_before_pair_then_by_number() {
        assert_eq!(pair(2, 1), Cause::Pair(PrNumber(1), PrNumber(2)));
        let mut causes = vec![pair(1, 2), bad(3), bad(1), pair(1, 3)];
        causes.sort();
        assert_eq!(causes, vec![bad(1), bad(3), pair(1, 2), pair(1, 3)]);
    }

    #[test]
    fn a_run_exercises_every_member_and_every_pair_of_its_tree() {
        let mut model = model(&[1, 2, 3]);
        model.sync(
            &[ready_all(&[1, 3]), vec![repushed(2)]].concat(),
            &none(),
            0,
        );
        // 2's batch head is gone, and 4 is not queued.
        let causes = model.exercised(&members(&[1, 2, 3, 4]));
        assert_eq!(causes, BTreeSet::from([bad(1), bad(3), pair(1, 3)]));
    }

    #[test]
    fn regrowth_is_per_landing_and_clamped() {
        let model = model(&[1]);
        close(model.regrown(5), 1.0 - 0.998f64.powi(5));
        close(model.regrown(3), 1.0 - 0.998f64.powi(3));
        // No landing, or a span that runs backwards, regrows nothing.
        assert_eq!(model.regrown(0), 0.0);
        assert_eq!(model.regrown(-1), 0.0);
        assert_eq!(model.regrown(i64::from(u32::MAX)), 1.0 - FLOOR);
        // A held pull request's belief is its regrowth since it was cleared.
        close(model.belief(bad(1)), model.regrown(i64::from(BEHIND)));
    }

    #[test]
    fn a_pull_request_is_cleared_at_the_trunk_its_own_ci_ran_on() {
        let mut model = Model::new(priors());
        model.sync(&[fresh(1), stale(ready(2), 7), ready(3)], &none(), 10);
        assert_eq!(model.cleared(PrNumber(2)), Some(3));
        close(model.belief(bad(2)), 1.0 - 0.998f64.powi(7));
        assert_eq!(model.belief(bad(3)), prior());
        // Cleared against the current trunk: no cause at all.
        assert_eq!(model.belief(bad(1)), 0.0);
        assert!(model.would_blame(&members(&[1])).is_empty());
        // A re-push starts again at the new head's staleness.
        model.sync(&[fresh(1), stale(repushed(2), 2), ready(3)], &none(), 10);
        assert_eq!(model.cleared(PrNumber(2)), Some(8));
    }

    #[test]
    fn a_passing_own_ci_run_on_the_same_head_moves_the_cleared_position_forward() {
        let mut model = model(&[1, 2, 3, 4, 5]);
        let prs = |first: PrSnapshot| {
            let mut prs = vec![first];
            prs.extend((2..=5).map(|n| stale(ready(n), 3)));
            prs
        };
        // Pending, failed or conflicting: no run passed, nothing moves.
        for status in [
            PrStatus::Waiting(BTreeSet::from([Hold::OwnCiPending])),
            PrStatus::Waiting(BTreeSet::from([Hold::OwnCiFailed])),
            PrStatus::Conflicting(dorfq_core::ConflictWith::Trunk),
        ] {
            let mut waiting = model.clone();
            waiting.sync(&prs(stale(pr(1, status.clone()), 3)), &none(), 0);
            assert_eq!(waiting.cleared(PrNumber(1)), Some(-25), "{status:?}");
            assert_eq!(waiting.cleared(PrNumber(2)), Some(-3), "{status:?}");
        }
        // Held for review, its own CI passed: moved like a ready one.
        model.passed(&members(&[5]), 0);
        model.sync(&prs(stale(held(1, Hold::Review), 3)), &none(), 0);
        assert_eq!(model.cleared(PrNumber(1)), Some(-3));
        assert_eq!(model.cleared(PrNumber(4)), Some(-3));
        // A newer batch alibi already covered more: never back.
        assert_eq!(model.cleared(PrNumber(5)), Some(0));
        // Staleness that grows with the landings moves nothing.
        model.sync(
            &(1..=4).map(|n| stale(ready(n), 4)).collect::<Vec<_>>(),
            &none(),
            1,
        );
        assert_eq!(model.cleared(PrNumber(4)), Some(-3));
        assert_eq!(model.belief(bad(4)), model.regrown(4));
    }

    #[test]
    fn an_alibi_clears_every_exercised_cause() {
        let mut model = model(&[1, 2, 3]);
        model.failed(BatchId(1), &members(&[1, 2, 3]), 0);
        model.passed(&members(&[1, 2]), 0);
        assert_eq!(model.belief(bad(1)), 0.0);
        assert_eq!(model.belief(bad(2)), 0.0);
        assert_eq!(model.belief(pair(1, 2)), FLOOR);
        assert_eq!(model.belief(bad(3)), prior());
        assert_eq!(model.belief(pair(1, 3)), 0.002);
        assert_eq!(
            effective(&model, 0),
            BTreeSet::from([bad(3), pair(1, 3), pair(2, 3)])
        );
    }

    #[test]
    fn an_alibi_from_a_moved_trunk_clears_at_its_covered_position_but_floors_pairs() {
        let mut model = model(&[1, 2, 3]);
        model.failed(BatchId(1), &members(&[1, 2, 3]), 0);
        model.sync(&ready_after(&[1, 2, 3], 2), &none(), 2);
        model.passed(&members(&[1, 2]), 0);
        assert_eq!(model.cleared(PrNumber(1)), Some(0));
        close(model.belief(bad(1)), model.regrown(2));
        close(model.belief(bad(2)), model.regrown(2));
        assert_eq!(model.belief(pair(1, 2)), FLOOR);
        close(model.belief(bad(3)), model.regrown(i64::from(BEHIND) + 2));
        // An older alibi never moves a newer one back.
        model.passed(&members(&[1]), -4);
        assert_eq!(model.cleared(PrNumber(1)), Some(0));
    }

    #[test]
    fn an_alibi_after_a_failure_stops_it_blaming_and_no_landing_restores_the_blame() {
        // 41, 42 and 43 on trunk; {41, 42, 43} fails at position 0 with each
        // five landings behind; 41 then passes alone, covered at 0.
        let prs = |landings: u32| -> Vec<PrSnapshot> {
            [41, 42, 43]
                .iter()
                .map(|&n| stale(ready(n), 5 + landings))
                .collect()
        };
        let mut model = Model::new(priors());
        model.sync(&prs(0), &none(), 0);
        model.failed(BatchId(1), &members(&[41, 42, 43]), 0);
        model.passed(&members(&[41]), 0);
        // A singleton exercises no pair: the pairs stay at the prior.
        let blamed = BTreeSet::from([bad(42), bad(43), pair(41, 42), pair(41, 43), pair(42, 43)]);
        assert_eq!(effective(&model, 0), blamed);
        model.sync(&prs(3), &landing(&[7, 8, 9]), 3);
        assert_eq!(effective(&model, 0), blamed);
        assert_eq!(
            model.would_blame(&members(&[41])),
            BTreeSet::from([bad(41)])
        );
        close(model.belief(bad(41)), model.regrown(3));
    }

    #[test]
    fn a_failed_singleton_of_a_pull_request_cleared_against_its_trunk_is_a_flake() {
        let mut model = Model::new(priors());
        model.sync(&[fresh(1)], &none(), 4);
        model.failed(BatchId(1), &members(&[1]), 4);
        assert!(model.evidence().is_empty());
    }

    #[test]
    fn an_alibi_does_not_materialise_a_pair() {
        let mut model = model(&[1, 2]);
        model.passed(&members(&[1, 2]), 0);
        // Not materialised, so still the prior for a pair never seen failing.
        assert_eq!(model.belief(pair(1, 2)), 0.002);
    }

    #[test]
    fn a_repush_resets_the_belief_and_takes_its_runs_with_it() {
        let mut model = model(&[1, 2, 3]);
        model.failed(BatchId(1), &members(&[1, 2, 3]), 0);
        model.failed(BatchId(2), &members(&[1, 3]), 0);
        model.passed(&members(&[2]), 0);
        model.sync(
            &[ready_all(&[1, 3]), vec![repushed(2)]].concat(),
            &none(),
            0,
        );
        assert_eq!(model.belief(bad(2)), prior());
        assert_eq!(model.belief(pair(1, 2)), 0.002);
        assert_eq!(batches(&model), vec![2]);
    }

    #[test]
    fn a_failed_run_materialises_its_pairs_at_the_prior() {
        let mut model = model(&[1, 2, 3]);
        model.failed(BatchId(1), &members(&[1, 2]), 0);
        model.passed(&members(&[1, 2, 3]), 0);
        // Materialised by the failure, so the pass reached it.
        assert_eq!(model.belief(pair(1, 2)), FLOOR);
        // Never in a failed run: the pass left it unmaterialised.
        assert_eq!(model.belief(pair(1, 3)), 0.002);
    }

    #[test]
    fn a_run_whose_effective_set_empties_leaves() {
        let mut model = model(&[1, 2, 3]);
        model.failed(BatchId(1), &members(&[1, 2]), 0);
        model.failed(BatchId(2), &members(&[2, 3]), 0);
        model.passed(&members(&[1, 2]), 0);
        assert_eq!(batches(&model), vec![2]);
        // 3's landing strips its causes, leaving only the cleared Bad(2).
        model.sync(&ready_after(&[1, 2], 1), &landing(&[3]), 1);
        assert_eq!(batches(&model), Vec::<u64>::new());
    }

    #[test]
    fn a_landing_strips_its_causes_and_any_other_departure_takes_its_runs() {
        let failed = || {
            let mut model = model(&[1, 2, 3]);
            model.failed(BatchId(1), &members(&[1, 2]), 0);
            model.failed(BatchId(2), &members(&[1, 3]), 0);
            model.failed(BatchId(3), &members(&[2, 3]), 0);
            model
        };
        let mut withdrawn = failed();
        withdrawn.sync(&ready_all(&[2, 3]), &none(), 0);
        assert_eq!(batches(&withdrawn), vec![3]);
        assert_eq!(withdrawn.belief(bad(1)), 0.0);

        let mut landed = failed();
        landed.sync(&ready_after(&[2, 3], 1), &landing(&[1]), 1);
        let exercised: Vec<BTreeSet<Cause>> = landed
            .evidence()
            .iter()
            .map(|run| run.exercised.clone())
            .collect();
        assert_eq!(
            exercised,
            vec![
                BTreeSet::from([bad(2)]),
                BTreeSet::from([bad(3)]),
                BTreeSet::from([bad(2), bad(3), pair(2, 3)]),
            ]
        );
    }

    #[test]
    fn a_pair_within_one_stack_is_never_exercised_materialised_or_believed() {
        // 3 on 2 on 1, and 4 on trunk.
        let prs = vec![ready(1), stacked(2, 1), stacked(3, 2), ready(4)];
        let mut model = Model::new(priors());
        model.sync(&prs, &none(), 0);
        assert!(model.related(PrNumber(1), PrNumber(3)));
        assert!(model.related(PrNumber(3), PrNumber(2)));
        assert!(!model.related(PrNumber(3), PrNumber(4)));
        assert_eq!(
            model.exercised(&members(&[1, 2, 3, 4])),
            BTreeSet::from([
                bad(1),
                bad(2),
                bad(3),
                bad(4),
                pair(1, 4),
                pair(2, 4),
                pair(3, 4)
            ])
        );
        assert_eq!(model.belief(pair(1, 3)), 0.0);
        model.failed(BatchId(1), &members(&[1, 2, 3]), 0);
        assert_eq!(model.belief(pair(1, 2)), 0.0);
        assert_eq!(
            model.would_blame(&members(&[1, 2, 3])),
            BTreeSet::from([bad(1), bad(2), bad(3)])
        );

        // 1 lands and 2 is re-targeted on trunk: 2 and 3 are still one
        // stack, and 1 is no longer anything's ancestor.
        let mut retargeted = stacked(2, 1);
        retargeted.base = PrBase::Trunk;
        model.sync(&[retargeted, stacked(3, 2), ready(4)], &landing(&[1]), 1);
        assert!(!model.related(PrNumber(1), PrNumber(2)));
        assert!(model.related(PrNumber(2), PrNumber(3)));
    }

    #[test]
    fn a_belief_stops_below_one() {
        let mut model = Model::new(Priors {
            p_interact: 0.5,
            ..priors()
        });
        model.sync(
            &(1..=3).map(|n| stale(ready(n), 60)).collect::<Vec<_>>(),
            &none(),
            0,
        );
        assert_eq!(model.belief(bad(1)), 1.0 - FLOOR);
        model.failed(BatchId(1), &members(&[1, 2]), 0);
        model.failed(BatchId(2), &members(&[2, 3]), 0);
        let inference = super::super::infer::Inference::of(&model).unwrap();
        for n in 1..=3 {
            let p = inference.posterior(&model, bad(n));
            assert!(p.is_finite() && p > 0.99, "{n}: {p}");
        }
    }

    #[test]
    fn evidence_keeps_the_newest_64_runs() {
        let mut model = model(&[1, 2]);
        for id in 1..=70 {
            model.failed(BatchId(id), &members(&[1, 2]), 0);
        }
        assert_eq!(model.evidence().len(), MAX_EVIDENCE);
        assert_eq!(batches(&model), (7..=70).collect::<Vec<_>>());
    }
}
