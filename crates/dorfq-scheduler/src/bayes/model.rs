//! Causes, beliefs and the evidence of failed runs.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use dorfq_core::{BatchId, PrNumber, PrSnapshot, Sha};

use crate::{Landing, ancestors, bases};

/// The belief an alibi leaves: inference treats a cause at the floor as
/// false, and decay can raise it again.
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
    /// A pull request fails on its own against trunk.
    pub(crate) p_bad: f64,
    /// A pair fails only together; also the per-landing regrowth.
    pub(crate) p_interact: f64,
    /// A run fails for no reason in the tree.
    pub(crate) p_flake: f64,
}

/// A run that failed, with the causes its tree exercised.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FailedRun {
    pub(crate) batch: BatchId,
    pub(crate) exercised: BTreeSet<Cause>,
}

/// One belief per queued pull request, materialised pairs, and the failed
/// runs that are evidence about them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Model {
    pub(crate) priors: Priors,
    /// What a landing does to the beliefs of what stays queued.
    landing: Landing,
    /// Each queued pull request's head and its `Bad` belief.
    bad: BTreeMap<PrNumber, (Sha, f64)>,
    /// Pairs a failed run exercised, with their belief.
    pairs: BTreeMap<Cause, f64>,
    /// Failed runs, oldest first.
    evidence: VecDeque<FailedRun>,
    /// Each queued pull request's queued ancestors, by the latest bases.
    ancestors: BTreeMap<PrNumber, BTreeSet<PrNumber>>,
}

impl Model {
    pub(crate) fn new(priors: Priors, landing: Landing) -> Model {
        Model {
            priors,
            landing,
            bad: BTreeMap::new(),
            pairs: BTreeMap::new(),
            evidence: VecDeque::new(),
            ancestors: BTreeMap::new(),
        }
    }

    /// Aligns the beliefs with the queued pull requests: a new one starts at
    /// the prior, and a re-pushed one is reset as a new one. A pull request
    /// no longer at the head the model holds leaves every pair; its causes
    /// leave every run if it is in `landed`, and otherwise every run that
    /// exercised one of them leaves with it. `landed` is what reached trunk,
    /// by the queue or by another hand. The bases of `prs` replace the stack
    /// relation the model holds. Under [`Landing::Transfer`], first, every
    /// pull request the model holds takes into its `Bad` belief its
    /// pair belief with each one in `landed`, judged by the stack relation
    /// held before this observation: once one of a pair is trunk, the pair
    /// failing is the other failing on trunk.
    pub(crate) fn sync(&mut self, prs: &[PrSnapshot], landed: &BTreeSet<PrNumber>) {
        if self.landing == Landing::Transfer {
            self.transfer(landed);
        }
        let base = bases(prs);
        self.ancestors = prs
            .iter()
            .map(|p| (p.number, ancestors(&base, p.base).collect()))
            .collect();
        let current: BTreeMap<PrNumber, &Sha> = prs.iter().map(|p| (p.number, &p.head)).collect();
        let gone: Vec<PrNumber> = self
            .bad
            .iter()
            .filter(|(pr, (head, _))| current.get(pr) != Some(&head))
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
        for (pr, head) in current {
            self.bad
                .entry(pr)
                .or_insert_with(|| (head.clone(), self.priors.p_bad));
        }
        self.prune();
    }

    /// Each pull request the model holds, other than those in `landed`,
    /// takes in its pair belief with each of `landed`, up to `1 − FLOOR`.
    fn transfer(&mut self, landed: &BTreeSet<PrNumber>) {
        let survive: Vec<(PrNumber, f64)> = self
            .bad
            .keys()
            .filter(|pr| !landed.contains(pr))
            .map(|&b| {
                let survive = landed
                    .iter()
                    .map(|&a| 1.0 - self.belief(Cause::pair(a, b)))
                    .product();
                (b, survive)
            })
            .collect();
        for (pr, survive) in survive {
            if let Some((_, p)) = self.bad.get_mut(&pr) {
                *p = (1.0 - (1.0 - *p) * survive).min(1.0 - FLOOR);
            }
        }
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
            .filter(|(pr, head)| self.bad.get(pr).is_some_and(|(h, _)| h == head))
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

    /// A run over `tree` passed: every cause it exercised is set to the floor.
    pub(crate) fn passed(&mut self, tree: &[(PrNumber, Sha)]) {
        for cause in self.exercised(tree) {
            match cause {
                Cause::Bad(pr) => {
                    if let Some((_, p)) = self.bad.get_mut(&pr) {
                        *p = FLOOR;
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

    /// Batch `batch` failed over `tree`: the run joins the evidence, and the
    /// pairs it exercised that were not yet materialised start at the prior.
    pub(crate) fn failed(&mut self, batch: BatchId, tree: &[(PrNumber, Sha)]) {
        let exercised = self.exercised(tree);
        for cause in &exercised {
            if let Cause::Pair(..) = cause {
                self.pairs.entry(*cause).or_insert(self.priors.p_interact);
            }
        }
        self.evidence.push_back(FailedRun { batch, exercised });
        while self.evidence.len() > MAX_EVIDENCE {
            self.evidence.pop_front();
        }
        self.prune();
    }

    /// `landed` pull requests reached trunk: every other queued pull
    /// request's `Bad` belief regrows by one interaction chance per landing,
    /// up to `1 − FLOOR`, so inference's `ln(1 − p)` stays finite.
    pub(crate) fn decay(&mut self, landed: &BTreeSet<PrNumber>) {
        let survive = (1.0 - self.priors.p_interact).powi(landed.len() as i32);
        for (pr, (_, p)) in &mut self.bad {
            if !landed.contains(pr) {
                *p = (1.0 - (1.0 - *p) * survive).min(1.0 - FLOOR);
            }
        }
    }

    /// The belief in `cause`: a pair not materialised is at the prior; a
    /// pull request not queued, and a pair within one stack, are not causes.
    pub(crate) fn belief(&self, cause: Cause) -> f64 {
        match cause {
            Cause::Bad(pr) => self.bad.get(&pr).map_or(0.0, |(_, p)| *p),
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

    /// The causes of `run` above the floor.
    pub(crate) fn effective<'a>(&'a self, run: &'a FailedRun) -> impl Iterator<Item = Cause> + 'a {
        run.exercised
            .iter()
            .copied()
            .filter(|&cause| self.belief(cause) > FLOOR)
    }

    /// The causes above the floor that a failed run over `tree` would blame.
    pub(crate) fn would_blame(&self, tree: &[(PrNumber, Sha)]) -> BTreeSet<Cause> {
        self.exercised(tree)
            .into_iter()
            .filter(|&cause| self.belief(cause) > FLOOR)
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

#[cfg(test)]
mod tests {
    use super::super::test_support::{bad, members, model, pair, priors};
    use super::*;
    use crate::test_support::{ready, ready_all, repushed, stacked};
    use dorfq_core::PrBase;

    fn batches(model: &Model) -> Vec<u64> {
        model.evidence().iter().map(|run| run.batch.0).collect()
    }

    fn none() -> BTreeSet<PrNumber> {
        BTreeSet::new()
    }

    #[test]
    fn new_pull_requests_start_at_the_prior_and_pairs_at_theirs() {
        let model = model(&[1, 2]);
        assert_eq!(model.belief(bad(1)), 0.05);
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
        model.sync(&[ready_all(&[1, 3]), vec![repushed(2)]].concat(), &none());
        // 2's batch head is gone, and 4 is not queued.
        let causes = model.exercised(&members(&[1, 2, 3, 4]));
        assert_eq!(causes, BTreeSet::from([bad(1), bad(3), pair(1, 3)]));
    }

    #[test]
    fn alibi_sets_every_exercised_cause_to_the_floor() {
        let mut model = model(&[1, 2, 3]);
        model.failed(BatchId(1), &members(&[1, 2, 3]));
        model.passed(&members(&[1, 2]));
        assert_eq!(model.belief(bad(1)), FLOOR);
        assert_eq!(model.belief(bad(2)), FLOOR);
        assert_eq!(model.belief(pair(1, 2)), FLOOR);
        assert_eq!(model.belief(bad(3)), 0.05);
        assert_eq!(model.belief(pair(1, 3)), 0.002);
        let run = &model.evidence()[0];
        let effective: Vec<Cause> = model.effective(run).collect();
        assert_eq!(effective, vec![bad(3), pair(1, 3), pair(2, 3)]);
    }

    #[test]
    fn an_alibi_does_not_materialise_a_pair() {
        let mut model = model(&[1, 2]);
        model.passed(&members(&[1, 2]));
        // Not materialised, so still the prior for a pair never seen failing.
        assert_eq!(model.belief(pair(1, 2)), 0.002);
    }

    #[test]
    fn decay_after_a_landing_of_k_raises_every_other_belief() {
        let mut model = model(&[1, 2, 3, 4]);
        model.passed(&members(&[4]));
        model.decay(&BTreeSet::from([PrNumber(1), PrNumber(2)]));
        let survive = 0.998f64 * 0.998;
        assert_eq!(model.belief(bad(1)), 0.05);
        assert!((model.belief(bad(3)) - (1.0 - 0.95 * survive)).abs() < 1e-15);
        assert!((model.belief(bad(4)) - (1.0 - (1.0 - FLOOR) * survive)).abs() < 1e-15);
        assert!(model.belief(bad(4)) > FLOOR);
    }

    #[test]
    fn a_repush_resets_the_belief_and_takes_its_runs_with_it() {
        let mut model = model(&[1, 2, 3]);
        model.failed(BatchId(1), &members(&[1, 2, 3]));
        model.failed(BatchId(2), &members(&[1, 3]));
        model.passed(&members(&[2]));
        model.sync(&[ready_all(&[1, 3]), vec![repushed(2)]].concat(), &none());
        assert_eq!(model.belief(bad(2)), 0.05);
        assert_eq!(model.belief(pair(1, 2)), 0.002);
        assert_eq!(batches(&model), vec![2]);
    }

    #[test]
    fn a_failed_run_materialises_its_pairs_at_the_prior() {
        let mut model = model(&[1, 2, 3]);
        model.failed(BatchId(1), &members(&[1, 2]));
        model.passed(&members(&[1, 2, 3]));
        // Materialised by the failure, so the pass reached it.
        assert_eq!(model.belief(pair(1, 2)), FLOOR);
        // Never in a failed run: the pass left it unmaterialised.
        assert_eq!(model.belief(pair(1, 3)), 0.002);
    }

    #[test]
    fn a_run_whose_effective_set_empties_leaves() {
        let mut model = model(&[1, 2, 3]);
        model.failed(BatchId(1), &members(&[1, 2]));
        model.failed(BatchId(2), &members(&[2, 3]));
        model.passed(&members(&[1, 2]));
        assert_eq!(batches(&model), vec![2]);
        // 3's landing strips its causes, leaving only the floored Bad(2).
        model.sync(&ready_all(&[1, 2]), &BTreeSet::from([PrNumber(3)]));
        assert_eq!(batches(&model), Vec::<u64>::new());
    }

    #[test]
    fn a_landing_strips_its_causes_and_any_other_departure_takes_its_runs() {
        let failed = || {
            let mut model = model(&[1, 2, 3]);
            model.failed(BatchId(1), &members(&[1, 2]));
            model.failed(BatchId(2), &members(&[1, 3]));
            model.failed(BatchId(3), &members(&[2, 3]));
            model
        };
        let mut withdrawn = failed();
        withdrawn.sync(&ready_all(&[2, 3]), &none());
        assert_eq!(batches(&withdrawn), vec![3]);
        assert_eq!(withdrawn.belief(bad(1)), 0.0);

        let mut landed = failed();
        landed.sync(&ready_all(&[2, 3]), &BTreeSet::from([PrNumber(1)]));
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
        let mut model = Model::new(priors(), Landing::Decay);
        model.sync(&prs, &none());
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
        model.failed(BatchId(1), &members(&[1, 2, 3]));
        assert_eq!(model.belief(pair(1, 2)), 0.0);
        assert_eq!(
            model.would_blame(&members(&[1, 2, 3])),
            BTreeSet::from([bad(1), bad(2), bad(3)])
        );

        // 1 lands and 2 is re-targeted on trunk: 2 and 3 are still one
        // stack, and 1 is no longer anything's ancestor.
        let mut retargeted = stacked(2, 1);
        retargeted.base = PrBase::Trunk;
        model.sync(
            &[retargeted, stacked(3, 2), ready(4)],
            &BTreeSet::from([PrNumber(1)]),
        );
        assert!(!model.related(PrNumber(1), PrNumber(2)));
        assert!(model.related(PrNumber(2), PrNumber(3)));
    }

    #[test]
    fn decay_stops_below_one() {
        let mut model = Model::new(
            Priors {
                p_interact: 0.5,
                ..priors()
            },
            Landing::Decay,
        );
        model.sync(&ready_all(&[1, 2, 3]), &none());
        for _ in 0..60 {
            model.decay(&BTreeSet::from([PrNumber(9)]));
        }
        assert_eq!(model.belief(bad(1)), 1.0 - FLOOR);
        model.failed(BatchId(1), &members(&[1, 2]));
        model.failed(BatchId(2), &members(&[2, 3]));
        let inference = super::super::infer::Inference::of(&model).unwrap();
        for n in 1..=3 {
            let p = inference.posterior(&model, bad(n));
            assert!(p.is_finite() && p > 0.99, "{n}: {p}");
        }
    }

    /// A model at [`priors`] under `transfer`, holding `prs`.
    fn transferring(prs: &[PrSnapshot]) -> Model {
        let mut model = Model::new(priors(), Landing::Transfer);
        model.sync(prs, &none());
        model
    }

    fn landing(prs: &[u64]) -> BTreeSet<PrNumber> {
        prs.iter().copied().map(PrNumber).collect()
    }

    #[test]
    fn transfer_regrows_an_untouched_pair_exactly_as_decay_does() {
        let mut decayed = model(&[1, 2, 3, 4]);
        decayed.passed(&members(&[4]));
        decayed.decay(&landing(&[1, 2]));
        let mut transferred = transferring(&ready_all(&[1, 2, 3, 4]));
        transferred.passed(&members(&[4]));
        transferred.sync(&ready_all(&[3, 4]), &landing(&[1, 2]));
        for n in [3, 4] {
            let (d, t) = (decayed.belief(bad(n)), transferred.belief(bad(n)));
            assert!((d - t).abs() < 1e-15, "{n}: {d} {t}");
        }
        assert_eq!(transferred.belief(bad(1)), 0.0);
    }

    #[test]
    fn transfer_takes_a_pairs_held_belief_so_an_alibied_pair_barely_regrows() {
        let mut model = transferring(&ready_all(&[1, 2, 3]));
        // The failure materialises every pair at the prior; the pass alibis
        // 1, 2 and their pair.
        model.failed(BatchId(1), &members(&[1, 2, 3]));
        model.passed(&members(&[1, 2]));
        model.sync(&ready_all(&[2, 3]), &landing(&[1]));
        let floor_twice = 1.0 - (1.0 - FLOOR) * (1.0 - FLOOR);
        assert!((model.belief(bad(2)) - floor_twice).abs() < 1e-15);
        // Pair (1, 3) held the prior, so 3 regrows by it.
        assert!((model.belief(bad(3)) - (1.0 - 0.95 * 0.998)).abs() < 1e-15);
    }

    #[test]
    fn transfer_leaves_a_child_whose_base_landed_and_a_repushed_one_alone() {
        // 2 on 1, and 3 on trunk.
        let mut model = transferring(&[ready(1), stacked(2, 1), ready(3)]);
        let mut retargeted = stacked(2, 1);
        retargeted.base = PrBase::Trunk;
        model.sync(&[retargeted, repushed(3)], &landing(&[1]));
        // The child's own CI tested it on its base, so nothing transfers.
        assert!((model.belief(bad(2)) - 0.05).abs() < 1e-15);
        // A re-pushed pull request starts again at the prior.
        assert_eq!(model.belief(bad(3)), 0.05);
    }

    #[test]
    fn transfer_stops_below_one() {
        let mut model = Model::new(
            Priors {
                p_interact: 0.9,
                ..priors()
            },
            Landing::Transfer,
        );
        let prs: Vec<u64> = (1..=40).collect();
        model.sync(&ready_all(&prs), &none());
        model.sync(&ready_all(&[40]), &landing(&prs[..39]));
        assert_eq!(model.belief(bad(40)), 1.0 - FLOOR);
    }

    #[test]
    fn evidence_keeps_the_newest_64_runs() {
        let mut model = model(&[1, 2]);
        for id in 1..=70 {
            model.failed(BatchId(id), &members(&[1, 2]));
        }
        assert_eq!(model.evidence().len(), MAX_EVIDENCE);
        assert_eq!(batches(&model), (7..=70).collect::<Vec<_>>());
    }
}
