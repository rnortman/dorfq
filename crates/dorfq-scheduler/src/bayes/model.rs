//! Causes, beliefs and the evidence of failed runs.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use dorfq_core::{BatchId, Hold, PrNumber, PrSnapshot, PrStatus, Sha};

use super::infer::MAX_COMPONENT_RUNS;
use crate::{ancestors, bases};

/// The belief below which a pair is no cause, and the complement of the cap
/// on every belief, which keeps inference's `ln(1 − p)` finite.
pub(crate) const FLOOR: f64 = 1e-6;

/// The most failed runs the evidence keeps; the oldest goes first.
pub(crate) const MAX_EVIDENCE: usize = 64;

/// Something that can make a run fail. Orders `Bad` before `Pair` before
/// `Trunk`, then by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Cause {
    /// The pull request fails against trunk on its own.
    Bad(PrNumber),
    /// The two fail only together; the smaller number first.
    Pair(PrNumber, PrNumber),
    /// Trunk, or CI, broke in the stretch opened by this passing run on
    /// trunk: every run started after it and before the next fails. `None`
    /// is the stretch before any pass this process read.
    Trunk(Option<BatchId>),
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
            Cause::Trunk(_) => false,
        }
    }

    /// The pull request, the pair's two, or none for trunk.
    pub(crate) fn prs(&self) -> Vec<PrNumber> {
        match *self {
            Cause::Bad(i) => vec![i],
            Cause::Pair(i, j) => vec![i, j],
            Cause::Trunk(_) => Vec::new(),
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
    /// CI misses a real break: a pass lowers a cause's odds by this factor.
    pub(crate) p_miss: f64,
    /// Trunk or CI breaks between two passes on trunk for a reason the
    /// queue did not test; each change by another hand counts once more.
    pub(crate) p_trunk: f64,
}

/// A run that failed, with the pull-request causes its tree exercised and
/// the trunk position it started on, credited with its own tree's landings
/// since. Its trunk cause is its stretch's, by its id.
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
    /// The trunk position its own CI covered when this head was first
    /// seen, `behind` pull requests back. Before it nothing is a cause;
    /// at it, its own content is one, at the prior `p_interact` that run
    /// missed at `p_miss`; after it, each landing regrows one.
    origin: i64,
    /// The position of its latest own-CI alibi on this head, `origin`
    /// until a later run passes on a later trunk.
    own: i64,
    /// The covered position of every passing run over it since, own CI
    /// or the queue's, each an alibi that lowers by `p_miss` the chance
    /// that a landing at or before it broke the pull request.
    alibis: Vec<i64>,
    /// Per trunk position, the chance that what reached trunk there broke
    /// this pull request beyond the regrowth prior: the posterior a pair
    /// with a pull request that reached trunk there had earned.
    suspected: BTreeMap<i64, f64>,
}

impl Held {
    /// Records a pass covering position `at`; one that covered less than
    /// the origin says nothing about this head.
    fn alibi(&mut self, at: i64) {
        if at >= self.origin {
            self.alibis.push(at);
        }
    }
}

/// A pair a failed run exercised.
#[derive(Debug, Clone, Copy, PartialEq)]
struct HeldPair {
    /// Its belief: the prior when materialised, discounted once per pass
    /// that exercised it.
    belief: f64,
    /// The covered position of the latest passing run that exercised it,
    /// if one has: the pair has been tested against a trunk at least as
    /// new as any run at or before it saw.
    covered: Option<i64>,
}

/// A window of a pull request's trunk, `(from, to]`, with the belief that
/// it broke the pull request; the origin window is `(origin, origin]`, the
/// pull request's own content.
pub(crate) type Window = (i64, i64, f64);

/// An origin and alibis per queued pull request, materialised pairs, the
/// stretches of trunk between passes, and the failed runs that are evidence
/// about them.
///
/// Positions are the forge's count of the pull requests that reached trunk
/// since its first observation; an origin before that is at a negative
/// position.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Model {
    pub(crate) priors: Priors,
    /// The trunk position as of the latest observation.
    position: i64,
    /// Each queued pull request the model holds.
    bad: BTreeMap<PrNumber, Held>,
    /// Pairs a failed run exercised.
    pairs: BTreeMap<Cause, HeldPair>,
    /// Failed runs, oldest first.
    evidence: VecDeque<FailedRun>,
    /// Each queued pull request's queued ancestors, by the latest bases.
    ancestors: BTreeMap<PrNumber, BTreeSet<PrNumber>>,
    /// The batches whose run passed with its tree on trunk, by id: each
    /// opens a stretch, and a failed run's stretch is the greatest of these
    /// below its own id.
    passes: BTreeSet<BatchId>,
    /// Per stretch, the chance something the queue did not test broke trunk
    /// in it, beyond the floor `p_trunk`: landings by their chance of being
    /// bad, and changes by another hand at `p_trunk` each.
    breaks: BTreeMap<Option<BatchId>, f64>,
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
            passes: BTreeSet::new(),
            breaks: BTreeMap::new(),
        }
    }

    /// Aligns the model with the queued pull requests at trunk `position`.
    /// A new one, or a re-pushed one, has its origin at the trunk its own
    /// CI ran on, [`PrSnapshot::behind`] pull requests back; one whose own
    /// CI has passed on its head on a later trunk than its last gains one
    /// alibi there. A pull request no
    /// longer at the head the model holds leaves every pair; its causes
    /// leave every run if it is in `landed`, and otherwise every run that
    /// exercised one of them leaves with it. `landed` is what reached
    /// trunk, by the queue or by another hand. The bases of `prs` replace
    /// the stack relation the model holds. `foreign` changes reached trunk
    /// that the queue did not land, each raising the current stretch's
    /// chance of a break by `p_trunk`.
    pub(crate) fn sync(
        &mut self,
        prs: &[PrSnapshot],
        landed: &BTreeSet<PrNumber>,
        position: i64,
        foreign: u32,
    ) {
        self.position = position;
        if foreign > 0 {
            let p = 1.0 - (1.0 - self.priors.p_trunk).powf(f64::from(foreign));
            self.broke(self.current(), p);
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
        for pr in prs {
            let own = position - i64::from(pr.behind);
            let held = self.bad.entry(pr.number).or_insert_with(|| Held {
                head: pr.head.clone(),
                origin: own,
                own,
                alibis: Vec::new(),
                suspected: BTreeMap::new(),
            });
            if own_ci_passed(&pr.status) && own > held.own {
                held.alibi(own);
                held.own = own;
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

    /// `p` after `n` passes, each missing a real break at `p_miss`: the
    /// odds of `p` times `p_miss^n`.
    pub(crate) fn discounted(&self, p: f64, n: usize) -> f64 {
        if n == 0 {
            return p;
        }
        let miss = self
            .priors
            .p_miss
            .powi(i32::try_from(n).unwrap_or(i32::MAX));
        p * miss / (p * miss + 1.0 - p)
    }

    /// The head the model holds queued pull request `pr` at.
    pub(crate) fn head(&self, pr: PrNumber) -> Option<&Sha> {
        self.bad.get(&pr).map(|held| &held.head)
    }

    /// The materialised pair causes.
    pub(crate) fn pairs(&self) -> impl Iterator<Item = Cause> + '_ {
        self.pairs.keys().copied()
    }

    /// What reached trunk at position `at` broke queued pull request `pr`
    /// with probability `p`, beyond the regrowth prior; a pull request the
    /// model does not hold is left alone.
    pub(crate) fn suspected(&mut self, pr: PrNumber, at: i64, p: f64) {
        if let Some(held) = self.bad.get_mut(&pr) {
            let s = held.suspected.entry(at).or_insert(0.0);
            *s = 1.0 - (1.0 - *s) * (1.0 - p);
        }
    }

    /// The origin of queued pull request `pr`, if the model holds it.
    pub(crate) fn origin(&self, pr: PrNumber) -> Option<i64> {
        self.bad.get(&pr).map(|held| held.origin)
    }

    /// Whether a pass has tested `cause` against a trunk at least as new
    /// as a run at `at` saw: an alibi of the pull request, or a pass of the
    /// pair since it was materialised, at or after `at`. `false` for a
    /// cause the model does not hold, and for trunk, which a pass opens a
    /// stretch for instead.
    pub(crate) fn covered(&self, cause: Cause, at: i64) -> bool {
        match cause {
            Cause::Bad(pr) => self
                .bad
                .get(&pr)
                .is_some_and(|held| held.alibis.iter().any(|&alibi| alibi >= at)),
            Cause::Pair(..) => self
                .pairs
                .get(&cause)
                .and_then(|pair| pair.covered)
                .is_some_and(|covered| covered >= at),
            Cause::Trunk(_) => false,
        }
    }

    /// The alibis of queued pull request `pr`, in the order they were read.
    #[cfg(test)]
    pub(crate) fn alibis(&self, pr: PrNumber) -> Option<&[i64]> {
        self.bad.get(&pr).map(|held| held.alibis.as_slice())
    }

    /// The windows of queued pull request `pr` cut at its origin, its
    /// alibis, the trunk position and every position in `cuts`, each with
    /// its belief, origin window first and then oldest first: the origin
    /// window at `p_interact`, and `(a, b]` at the regrowth of its
    /// landings folded with what is suspected at the positions in it, each
    /// discounted once per alibi at or after its end.
    pub(crate) fn windows(
        &self,
        pr: PrNumber,
        cuts: impl IntoIterator<Item = i64>,
    ) -> Option<Vec<Window>> {
        let held = self.bad.get(&pr)?;
        let mut at: BTreeSet<i64> = cuts.into_iter().collect();
        at.extend(&held.alibis);
        at.insert(held.origin);
        at.insert(self.position);
        let covering = |b: i64| held.alibis.iter().filter(|&&a| a >= b).count();
        let origin = self.discounted(self.priors.p_interact, 1 + held.alibis.len());
        let at: Vec<i64> = at.into_iter().filter(|&p| p >= held.origin).collect();
        Some(
            [(held.origin, held.origin, origin)]
                .into_iter()
                .chain(at.windows(2).map(|w| {
                    let (a, b) = (w[0], w[1]);
                    let clear = held
                        .suspected
                        .range(a + 1..=b)
                        .map(|(_, &s)| 1.0 - s)
                        .product::<f64>();
                    let p = (1.0 - (1.0 - self.regrown(b - a)) * clear).min(1.0 - FLOOR);
                    (a, b, self.discounted(p, covering(b)))
                }))
                .collect(),
        )
    }

    /// Batch `batch`'s run over `tree` passed, started on the trunk at
    /// position `covered` credited with its tree's landings since: every
    /// `Bad` it exercised gains an alibi there, and every materialised pair
    /// it exercised is discounted once and covered there. A run whose tree
    /// stood on the observation's trunk, `on_trunk`, opens a stretch.
    pub(crate) fn passed(
        &mut self,
        batch: BatchId,
        tree: &[(PrNumber, Sha)],
        covered: i64,
        on_trunk: bool,
    ) {
        for cause in self.exercised(tree) {
            match cause {
                Cause::Bad(pr) => {
                    if let Some(held) = self.bad.get_mut(&pr) {
                        held.alibi(covered);
                    }
                }
                Cause::Pair(..) => {
                    if let Some(&pair) = self.pairs.get(&cause) {
                        let held = HeldPair {
                            belief: self.discounted(pair.belief, 1),
                            covered: Some(pair.covered.map_or(covered, |c| c.max(covered))),
                        };
                        self.pairs.insert(cause, held);
                    }
                }
                Cause::Trunk(_) => unreachable!("a tree exercises no trunk cause"),
            }
        }
        if on_trunk {
            self.passes.insert(batch);
        }
        self.prune();
    }

    /// [`Model::passed`] on trunk for a batch whose id is above every
    /// other a test uses: it opens the newest stretch.
    #[cfg(test)]
    pub(crate) fn passed_after(&mut self, tree: &[(PrNumber, Sha)], covered: i64) {
        let id = self
            .current()
            .map_or(1_000_000, |pass| pass.0.max(999_999) + 1);
        self.passed(BatchId(id), tree, covered, true);
    }

    /// The stretch a run of batch `id` started in: the latest passing run
    /// on trunk that started before it.
    pub(crate) fn stretch(&self, id: BatchId) -> Option<BatchId> {
        self.passes.range(..id).next_back().copied()
    }

    /// The stretch a run started now falls in.
    pub(crate) fn current(&self) -> Option<BatchId> {
        self.passes.last().copied()
    }

    /// Something the queue did not test broke trunk in `stretch` with
    /// probability `p`.
    pub(crate) fn broke(&mut self, stretch: Option<BatchId>, p: f64) {
        let breaks = self.breaks.entry(stretch).or_insert(0.0);
        *breaks = 1.0 - (1.0 - *breaks) * (1.0 - p);
    }

    /// Batch `batch` failed over `tree`, started at position `at`: the run
    /// joins the evidence, and the pairs it exercised that were not yet
    /// materialised start at the prior.
    pub(crate) fn failed(&mut self, batch: BatchId, tree: &[(PrNumber, Sha)], at: i64) {
        let exercised = self.exercised(tree);
        for cause in &exercised {
            if let Cause::Pair(..) = cause {
                self.pairs.entry(*cause).or_insert(HeldPair {
                    belief: self.priors.p_interact,
                    covered: None,
                });
            }
        }
        self.evidence.push_back(FailedRun {
            batch,
            exercised,
            at,
        });
        self.prune();
    }

    /// The belief in `cause` now: for `Bad`, the chance that its own
    /// content or a landing since its origin broke it, given its alibis; a
    /// pair not materialised is at the prior; a pull request not queued,
    /// and a pair within one stack, are not causes.
    pub(crate) fn belief(&self, cause: Cause) -> f64 {
        match cause {
            Cause::Bad(pr) => self.windows(pr, []).map_or(0.0, |windows| {
                1.0 - windows.iter().map(|&(_, _, p)| 1.0 - p).product::<f64>()
            }),
            Cause::Pair(i, j) => {
                if self.bad.contains_key(&i) && self.bad.contains_key(&j) && !self.related(i, j) {
                    self.pairs
                        .get(&cause)
                        .map_or(self.priors.p_interact, |pair| pair.belief)
                } else {
                    0.0
                }
            }
            Cause::Trunk(stretch) => {
                let breaks = self.breaks.get(&stretch).copied().unwrap_or(0.0);
                (1.0 - (1.0 - self.priors.p_trunk) * (1.0 - breaks)).min(1.0 - FLOOR)
            }
        }
    }

    /// The failed runs, oldest first.
    pub(crate) fn evidence(&self) -> &VecDeque<FailedRun> {
        &self.evidence
    }

    /// Whether `cause` could have failed a run started at position `at`:
    /// a `Bad` whose origin is at or before that trunk, a pair above the
    /// floor, or trunk.
    fn blames(&self, cause: Cause, at: i64) -> bool {
        match cause {
            Cause::Bad(pr) => self.origin(pr).is_some_and(|origin| origin <= at),
            Cause::Pair(..) => self.belief(cause) > FLOOR,
            Cause::Trunk(_) => true,
        }
    }

    /// The causes of `run` that could have failed it: its pull-request
    /// causes that blame it, then its stretch's trunk cause.
    pub(crate) fn effective<'a>(&'a self, run: &'a FailedRun) -> impl Iterator<Item = Cause> + 'a {
        run.exercised
            .iter()
            .copied()
            .filter(|&cause| self.blames(cause, run.at))
            .chain([Cause::Trunk(self.stretch(run.batch))])
    }

    /// The causes a run over `tree` started now would blame, were it to
    /// fail: its pull-request causes and the current stretch's trunk.
    pub(crate) fn would_blame(&self, tree: &[(PrNumber, Sha)]) -> BTreeSet<Cause> {
        self.exercised(tree)
            .into_iter()
            .filter(|&cause| self.blames(cause, self.position))
            .chain([Cause::Trunk(self.current())])
            .collect()
    }

    /// Drops every run that blames no pull request and no pair and whose
    /// stretch a later pass on trunk closed: it teaches nothing about the
    /// queue, nor about trunk now. Of such runs in the current stretch it
    /// keeps the newest [`MAX_COMPONENT_RUNS`]: they share one component
    /// through the stretch's trunk, so inference never reads an older one.
    /// Then drops the oldest runs past [`MAX_EVIDENCE`], and every stretch
    /// no run falls in that is not the current one.
    fn prune(&mut self) {
        let current = self.current();
        let mut newer = 0;
        let mut empty: BTreeSet<BatchId> = BTreeSet::new();
        for run in self.evidence.iter().rev() {
            if !self
                .effective(run)
                .all(|cause| matches!(cause, Cause::Trunk(_)))
            {
                continue;
            }
            if self.stretch(run.batch) != current || newer == MAX_COMPONENT_RUNS {
                empty.insert(run.batch);
            } else {
                newer += 1;
            }
        }
        self.evidence.retain(|run| !empty.contains(&run.batch));
        while self.evidence.len() > MAX_EVIDENCE {
            self.evidence.pop_front();
        }
        let oldest = self.evidence.iter().map(|run| run.batch).min();
        let first = oldest.map(|id| self.stretch(id));
        let keep = |stretch: Option<BatchId>| {
            stretch == current
                || Some(stretch) == first
                || matches!((stretch, oldest), (Some(pass), Some(id)) if pass > id)
        };
        self.passes.retain(|&pass| keep(Some(pass)));
        self.breaks.retain(|&stretch, _| keep(stretch));
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
        BEHIND, fresh, held, pr, ready, ready_after, ready_all, repushed, sha, stacked, stale,
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

    fn trunk(stretch: Option<BatchId>) -> Cause {
        Cause::Trunk(stretch)
    }

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-15, "{a} against {b}");
    }

    /// `p` after `n` passes at [`priors`].
    fn d(p: f64, n: usize) -> f64 {
        Model::new(priors()).discounted(p, n)
    }

    fn regrown(k: i64) -> f64 {
        Model::new(priors()).regrown(k)
    }

    /// The belief in a pull request whose windows are at `ps`, origin
    /// window first.
    fn any(ps: &[f64]) -> f64 {
        1.0 - ps.iter().map(|p| 1.0 - p).product::<f64>()
    }

    /// The origin window's belief with no alibi.
    fn o() -> f64 {
        d(0.002, 1)
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
        // A held pull request's belief is its own content, which its own CI
        // missed, and its regrowth since its origin.
        close(
            model.belief(bad(1)),
            any(&[o(), model.regrown(i64::from(BEHIND))]),
        );
    }

    #[test]
    fn discounting_lowers_the_odds_by_the_miss_rate_per_pass() {
        close(d(0.3, 0), 0.3);
        let odds = |p: f64| p / (1.0 - p);
        for n in 1..=3 {
            let want = odds(0.3) * 0.02f64.powi(n);
            assert!((odds(d(0.3, n as usize)) / want - 1.0).abs() < 1e-12);
        }
        close(o(), 0.002 * 0.02 / (0.002 * 0.02 + 0.998));
    }

    #[test]
    fn a_pull_request_has_its_origin_at_the_trunk_its_own_ci_ran_on() {
        let mut model = Model::new(priors());
        model.sync(&[fresh(1), stale(ready(2), 7), ready(3)], &none(), 10, 0);
        assert_eq!(model.origin(PrNumber(2)), Some(3));
        close(model.belief(bad(2)), any(&[o(), 1.0 - 0.998f64.powi(7)]));
        assert_eq!(model.belief(bad(3)), prior());
        // At the current trunk: its own content alone, which a run blames.
        close(model.belief(bad(1)), o());
        assert_eq!(
            model.would_blame(&members(&[1])),
            BTreeSet::from([bad(1), trunk(None)])
        );
        // A re-push starts again at the new head's staleness.
        model.sync(&[fresh(1), stale(repushed(2), 2), ready(3)], &none(), 10, 0);
        assert_eq!(model.origin(PrNumber(2)), Some(8));
        assert_eq!(model.alibis(PrNumber(2)), Some(&[][..]));
    }

    #[test]
    fn a_passing_own_ci_run_on_the_same_head_is_one_more_alibi() {
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
            waiting.sync(&prs(stale(pr(1, status.clone()), 3)), &none(), 0, 0);
            assert_eq!(waiting.alibis(PrNumber(1)), Some(&[][..]), "{status:?}");
            assert_eq!(waiting.alibis(PrNumber(2)), Some(&[-3][..]), "{status:?}");
            assert_eq!(waiting.origin(PrNumber(1)), Some(-25), "{status:?}");
        }
        // Held for review, its own CI passed: an alibi like a ready one's.
        model.passed_after(&members(&[5]), 0);
        model.sync(&prs(stale(held(1, Hold::Protection), 3)), &none(), 0, 0);
        assert_eq!(model.alibis(PrNumber(1)), Some(&[-3][..]));
        assert_eq!(model.alibis(PrNumber(4)), Some(&[-3][..]));
        // The batch alibi and the own-CI one each count.
        assert_eq!(model.alibis(PrNumber(5)), Some(&[0, -3][..]));
        // The same run seen again is no second alibi.
        model.sync(&prs(stale(held(1, Hold::Protection), 3)), &none(), 0, 0);
        assert_eq!(model.alibis(PrNumber(1)), Some(&[-3][..]));
        // Staleness that grows with the landings adds nothing.
        model.sync(
            &(1..=4).map(|n| stale(ready(n), 4)).collect::<Vec<_>>(),
            &none(),
            1,
            0,
        );
        assert_eq!(model.alibis(PrNumber(4)), Some(&[-3][..]));
        close(
            model.belief(bad(4)),
            any(&[d(0.002, 2), d(regrown(22), 1), regrown(4)]),
        );
    }

    #[test]
    fn an_alibi_discounts_every_exercised_cause() {
        let mut model = model(&[1, 2, 3]);
        model.failed(BatchId(1), &members(&[1, 2, 3]), 0);
        model.passed_after(&members(&[1, 2]), 0);
        let once = any(&[d(0.002, 2), d(regrown(25), 1)]);
        close(model.belief(bad(1)), once);
        close(model.belief(bad(2)), once);
        assert!((once - 1.03e-3).abs() < 1e-5, "{once}");
        close(model.belief(pair(1, 2)), d(0.002, 1));
        assert_eq!(model.belief(bad(3)), prior());
        assert_eq!(model.belief(pair(1, 3)), 0.002);
        assert_eq!(
            effective(&model, 0),
            BTreeSet::from([
                bad(1),
                bad(2),
                bad(3),
                pair(1, 2),
                pair(1, 3),
                pair(2, 3),
                trunk(None)
            ])
        );
    }

    #[test]
    fn an_alibi_from_a_moved_trunk_discounts_up_to_its_covered_position() {
        let mut model = model(&[1, 2, 3]);
        model.failed(BatchId(1), &members(&[1, 2, 3]), 0);
        model.sync(&ready_after(&[1, 2, 3], 2), &none(), 2, 0);
        model.passed_after(&members(&[1, 2]), 0);
        assert_eq!(model.origin(PrNumber(1)), Some(-25));
        assert_eq!(model.alibis(PrNumber(1)), Some(&[0][..]));
        let windows = [d(0.002, 2), d(regrown(25), 1), regrown(2)];
        close(model.belief(bad(1)), any(&windows));
        close(model.belief(bad(2)), any(&windows));
        close(model.belief(pair(1, 2)), d(0.002, 1));
        close(
            model.belief(bad(3)),
            any(&[o(), model.regrown(i64::from(BEHIND) + 2)]),
        );
        // An older alibi cuts the window it falls in: what lies before it
        // is discounted twice.
        model.passed_after(&members(&[1]), -4);
        assert_eq!(model.alibis(PrNumber(1)), Some(&[0, -4][..]));
        close(
            model.belief(bad(1)),
            any(&[d(0.002, 3), d(regrown(21), 2), d(regrown(4), 1), regrown(2)]),
        );
        // A pass that covered less than the origin says nothing about it.
        model.passed_after(&members(&[1]), -30);
        assert_eq!(model.alibis(PrNumber(1)), Some(&[0, -4][..]));
    }

    #[test]
    fn an_alibi_after_a_failure_lowers_its_blame_and_landings_restore_it() {
        // 41, 42 and 43 on trunk; {41, 42, 43} fails at position 0 with each
        // five landings behind; 41 then passes alone, covered at 0.
        let prs = |landings: u32| -> Vec<PrSnapshot> {
            [41, 42, 43]
                .iter()
                .map(|&n| stale(ready(n), 5 + landings))
                .collect()
        };
        let mut model = Model::new(priors());
        model.sync(&prs(0), &none(), 0, 0);
        model.failed(BatchId(1), &members(&[41, 42, 43]), 0);
        model.passed_after(&members(&[41]), 0);
        // A singleton exercises no pair: the pairs stay at the prior.
        let blamed = BTreeSet::from([
            bad(41),
            bad(42),
            bad(43),
            pair(41, 42),
            pair(41, 43),
            pair(42, 43),
            trunk(None),
        ]);
        assert_eq!(effective(&model, 0), blamed);
        let inference = super::super::infer::Inference::of(&model).unwrap();
        let posterior: BTreeMap<Cause, f64> = inference.run_posteriors(0).into_iter().collect();
        assert!(posterior[&bad(41)] < posterior[&bad(42)], "{posterior:?}");
        model.sync(&prs(3), &landing(&[7, 8, 9]), 3, 0);
        assert_eq!(effective(&model, 0), blamed);
        assert_eq!(
            model.would_blame(&members(&[41])),
            BTreeSet::from([bad(41), trunk(Some(BatchId(1_000_000)))])
        );
        close(
            model.belief(bad(41)),
            any(&[d(0.002, 2), d(regrown(5), 1), regrown(3)]),
        );
    }

    #[test]
    fn a_failed_singleton_at_its_origin_blames_its_own_content_and_trunk() {
        let mut model = Model::new(priors());
        model.sync(&[fresh(1)], &none(), 4, 0);
        model.failed(BatchId(1), &members(&[1]), 4);
        assert_eq!(effective(&model, 0), BTreeSet::from([bad(1), trunk(None)]));
        close(model.belief(bad(1)), o());
    }

    #[test]
    fn an_alibi_does_not_materialise_a_pair() {
        let mut model = model(&[1, 2]);
        model.passed_after(&members(&[1, 2]), 0);
        // Not materialised, so still the prior for a pair never seen failing.
        assert_eq!(model.belief(pair(1, 2)), 0.002);
    }

    #[test]
    fn a_repush_resets_the_belief_and_takes_its_runs_with_it() {
        let mut model = model(&[1, 2, 3]);
        model.failed(BatchId(1), &members(&[1, 2, 3]), 0);
        model.failed(BatchId(2), &members(&[1, 3]), 0);
        model.passed_after(&members(&[2]), 0);
        model.sync(
            &[ready_all(&[1, 3]), vec![repushed(2)]].concat(),
            &none(),
            0,
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
        model.passed_after(&members(&[1, 2, 3]), 0);
        // Materialised by the failure, so the pass discounted it.
        close(model.belief(pair(1, 2)), d(0.002, 1));
        // Discounted past the floor, it is no cause.
        model.passed_after(&members(&[1, 2]), 0);
        assert!(model.belief(pair(1, 2)) < FLOOR);
        assert!(!model.would_blame(&members(&[1, 2])).contains(&pair(1, 2)));
        // Never in a failed run: the pass left it unmaterialised.
        assert_eq!(model.belief(pair(1, 3)), 0.002);
    }

    #[test]
    fn a_pass_covers_the_pairs_it_exercised() {
        let mut model = model(&[1, 2, 3]);
        model.failed(BatchId(1), &members(&[1, 2, 3]), 0);
        model.passed_after(&members(&[1, 2]), 3);
        assert!(model.covered(pair(1, 2), 3));
        assert!(model.covered(pair(1, 2), 0));
        assert!(!model.covered(pair(1, 2), 4));
        assert!(!model.covered(pair(1, 3), 0));
        close(model.belief(pair(1, 2)), d(0.002, 1));
        // A later pass at an older position discounts once more and keeps
        // the newest covered position.
        model.passed_after(&members(&[1, 2]), 1);
        close(model.belief(pair(1, 2)), d(0.002, 2));
        assert!(model.covered(pair(1, 2), 3));
        assert!(model.covered(bad(1), 3));
        assert!(!model.covered(Cause::Trunk(None), 0));
        // A re-pushed head's pair starts uncovered.
        model.sync(
            &[vec![repushed(1)], ready_all(&[2, 3])].concat(),
            &none(),
            0,
            0,
        );
        assert!(!model.covered(pair(1, 2), 0));

        // A pass read before the failure that materialises the pair covers
        // nothing: the pair starts at the prior, uncovered.
        let mut late = super::super::test_support::model(&[1, 2, 3]);
        late.passed_after(&members(&[1, 2]), 0);
        late.failed(BatchId(1), &members(&[1, 2, 3]), 0);
        assert!(!late.covered(pair(1, 2), 0));
        assert_eq!(late.belief(pair(1, 2)), 0.002);
    }

    #[test]
    fn a_run_whose_effective_set_empties_leaves() {
        let mut model = model(&[1, 2, 3]);
        model.failed(BatchId(1), &members(&[1, 2]), 0);
        model.failed(BatchId(2), &members(&[2, 3]), 0);
        model.passed_after(&members(&[1, 2]), 0);
        // A pass discounts: both runs stay.
        assert_eq!(batches(&model), vec![1, 2]);
        // 3's landing strips its causes, leaving run 2 on Bad(2).
        model.sync(&ready_after(&[1, 2], 1), &landing(&[3]), 1, 0);
        assert_eq!(batches(&model), vec![1, 2]);
        assert_eq!(effective(&model, 1), BTreeSet::from([bad(2), trunk(None)]));
        // 1 and 2 land: nothing is left to blame in either but a stretch the
        // pass closed.
        model.sync(&[], &landing(&[1, 2]), 3, 0);
        assert_eq!(batches(&model), Vec::<u64>::new());
        // Without the pass, they stay as the current stretch's evidence.
        let mut unpassed_model = Model::new(priors());
        unpassed_model.sync(&ready_all(&[1, 2, 3]), &none(), 0, 0);
        unpassed_model.failed(BatchId(1), &members(&[1, 2]), 0);
        unpassed_model.failed(BatchId(2), &members(&[2, 3]), 0);
        unpassed_model.sync(&[], &landing(&[1, 2, 3]), 3, 0);
        assert_eq!(batches(&unpassed_model), vec![1, 2]);
        assert_eq!(effective(&unpassed_model, 0), BTreeSet::from([trunk(None)]));
        // A pass on trunk then closes their stretch, and they leave.
        unpassed_model.passed(BatchId(3), &[], 3, true);
        assert_eq!(batches(&unpassed_model), Vec::<u64>::new());
    }

    #[test]
    fn a_pass_on_trunk_opens_a_stretch_and_a_failed_run_blames_the_stretch_it_started_in() {
        let mut model = model(&[1, 2]);
        model.failed(BatchId(1), &members(&[1]), 0);
        model.passed(BatchId(2), &members(&[2]), 0, true);
        model.failed(BatchId(3), &members(&[1]), 0);
        assert_eq!(effective(&model, 0), BTreeSet::from([bad(1), trunk(None)]));
        assert_eq!(
            effective(&model, 1),
            BTreeSet::from([bad(1), trunk(Some(BatchId(2)))])
        );
        assert_eq!(model.current(), Some(BatchId(2)));
        // A pass read late, started before run 1, opens the stretch run 1
        // started in and leaves the current one.
        model.passed(BatchId(0), &[], 0, true);
        assert_eq!(model.stretch(BatchId(1)), Some(BatchId(0)));
        assert_eq!(
            effective(&model, 0),
            BTreeSet::from([bad(1), trunk(Some(BatchId(0)))])
        );
        assert_eq!(model.current(), Some(BatchId(2)));
        // Every stretch is at the floor.
        for stretch in [None, Some(BatchId(0)), Some(BatchId(2))] {
            close(model.belief(trunk(stretch)), 0.001);
        }
    }

    #[test]
    fn a_pass_off_trunk_is_an_alibi_and_opens_no_stretch() {
        let mut model = model(&[1, 2]);
        model.failed(BatchId(1), &members(&[1, 2]), 0);
        model.passed(BatchId(2), &members(&[1]), 0, false);
        assert_eq!(model.alibis(PrNumber(1)), Some(&[0][..]));
        assert_eq!(model.current(), None);
        model.failed(BatchId(3), &members(&[2]), 0);
        assert_eq!(effective(&model, 1), BTreeSet::from([bad(2), trunk(None)]));
    }

    #[test]
    fn broke_raises_a_named_stretch_and_a_foreign_change_the_current_one() {
        let mut model = model(&[1]);
        model.failed(BatchId(1), &members(&[1]), 0);
        model.passed(BatchId(2), &[], 0, true);
        model.broke(Some(BatchId(2)), 0.3);
        close(
            model.belief(trunk(Some(BatchId(2)))),
            1.0 - (1.0 - 0.001) * (1.0 - 0.3),
        );
        close(model.belief(trunk(None)), 0.001);
        // Two changes by another hand: the floor and one each.
        let mut foreign = super::super::test_support::model(&[1]);
        foreign.sync(&ready_after(&[1], 2), &none(), 2, 2);
        close(
            foreign.belief(trunk(foreign.current())),
            1.0 - (1.0 - 0.001f64).powi(3),
        );
        // A raised stretch keeps every raise.
        foreign.broke(None, 0.5);
        close(
            foreign.belief(trunk(None)),
            1.0 - (1.0 - 0.001f64).powi(3) * 0.5,
        );
    }

    #[test]
    fn a_run_with_only_a_closed_stretch_s_trunk_cause_leaves() {
        let mut model = model(&[1, 2]);
        model.failed(BatchId(1), &members(&[1]), 0);
        model.failed(BatchId(2), &members(&[2]), 0);
        // 1 lands: run 1 blames only its stretch's trunk, which is current.
        model.sync(&ready_after(&[2], 1), &landing(&[1]), 1, 0);
        assert_eq!(batches(&model), vec![1, 2]);
        assert_eq!(effective(&model, 0), BTreeSet::from([trunk(None)]));
        // A pass on trunk closes it, and the run leaves; run 2 still blames
        // 2, and keeps its stretch.
        model.passed(BatchId(3), &[], 1, true);
        assert_eq!(batches(&model), vec![2]);
        assert_eq!(effective(&model, 0), BTreeSet::from([bad(2), trunk(None)]));
    }

    #[test]
    fn failed_probes_past_the_read_limit_evict_no_run_over_a_pull_request() {
        // Run 1 blames 1 in the first stretch; a pass on trunk opens the
        // next, and in it far more probes fail than the evidence holds.
        let probes = |n: u64| {
            let mut model = model(&[1, 2]);
            model.failed(BatchId(1), &members(&[1]), 0);
            model.passed(BatchId(2), &members(&[2]), 0, true);
            for id in 3..3 + n {
                model.failed(BatchId(id), &[], 0);
            }
            model
        };
        let flooded = probes(2 * MAX_EVIDENCE as u64);
        let last = 2 + 2 * MAX_EVIDENCE as u64;
        let newest: Vec<u64> = (last + 1 - MAX_COMPONENT_RUNS as u64..=last).collect();
        assert_eq!(batches(&flooded), [vec![1], newest].concat());
        // The probes dropped were past the read limit: inference over the
        // newest alone is the same.
        let read = probes(MAX_COMPONENT_RUNS as u64);
        let (a, b) = (
            super::super::infer::Inference::of(&flooded).unwrap(),
            super::super::infer::Inference::of(&read).unwrap(),
        );
        for cause in [bad(1), trunk(None), trunk(Some(BatchId(2)))] {
            close(a.posterior(&flooded, cause), b.posterior(&read, cause));
        }
    }

    #[test]
    fn stretches_no_run_falls_in_are_dropped_but_the_current_one() {
        let mut model = model(&[1]);
        model.broke(None, 0.5);
        model.failed(BatchId(2), &members(&[1]), 0);
        model.passed(BatchId(1), &[], 0, true);
        model.broke(Some(BatchId(1)), 0.5);
        model.passed(BatchId(3), &[], 0, true);
        model.broke(Some(BatchId(3)), 0.5);
        model.passed(BatchId(4), &[], 0, true);
        // Run 2 falls in pass 1's stretch; pass 3's stretch holds no run
        // but lies above it; the stretch before pass 1 holds nothing.
        close(model.belief(trunk(None)), 0.001);
        close(model.belief(trunk(Some(BatchId(1)))), 1.0 - 0.999 * 0.5);
        close(model.belief(trunk(Some(BatchId(3)))), 1.0 - 0.999 * 0.5);
        assert_eq!(model.stretch(BatchId(2)), Some(BatchId(1)));
        // Run 2 leaves with its pull request: only the current stretch stays.
        model.sync(&[], &none(), 0, 0);
        assert_eq!(batches(&model), Vec::<u64>::new());
        close(model.belief(trunk(Some(BatchId(1)))), 0.001);
        close(model.belief(trunk(Some(BatchId(3)))), 0.001);
        assert_eq!(model.current(), Some(BatchId(4)));
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
        withdrawn.sync(&ready_all(&[2, 3]), &none(), 0, 0);
        assert_eq!(batches(&withdrawn), vec![3]);
        assert_eq!(withdrawn.belief(bad(1)), 0.0);

        let mut landed = failed();
        landed.sync(&ready_after(&[2, 3], 1), &landing(&[1]), 1, 0);
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
        model.sync(&prs, &none(), 0, 0);
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
            BTreeSet::from([bad(1), bad(2), bad(3), trunk(None)])
        );

        // 1 lands and 2 is re-targeted on trunk: 2 and 3 are still one
        // stack, and 1 is no longer anything's ancestor.
        let mut retargeted = stacked(2, 1);
        retargeted.base = PrBase::Trunk;
        model.sync(&[retargeted, stacked(3, 2), ready(4)], &landing(&[1]), 1, 0);
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
            0,
        );
        let belief = model.belief(bad(1));
        assert!((1.0 - FLOOR..1.0).contains(&belief), "{belief}");
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

    #[test]
    fn a_suspected_position_folds_into_the_window_that_holds_it() {
        let mut model = model(&[1]);
        model.sync(&ready_after(&[1], 3), &none(), 3, 0);
        model.suspected(PrNumber(1), 2, 0.5);
        model.suspected(PrNumber(1), 2, 0.5);
        // Below the origin, at -25: never read.
        model.suspected(PrNumber(1), -40, 0.9);
        // Not held: nothing to fold into.
        model.suspected(PrNumber(9), 2, 0.5);
        let folded = |k: i64| 1.0 - (1.0 - regrown(k)) * 0.25;
        assert_eq!(
            model.windows(PrNumber(1), [1]),
            Some(vec![
                (-25, -25, o()),
                (-25, 1, regrown(26)),
                (1, 3, folded(2))
            ])
        );
        assert_eq!(
            model.windows(PrNumber(1), []),
            Some(vec![(-25, -25, o()), (-25, 3, folded(28))])
        );
        assert_eq!(model.windows(PrNumber(9), []), None);
        // A new head starts with nothing suspected.
        let mut pushed = repushed(1);
        pushed.behind = 3;
        model.sync(&[pushed], &none(), 3, 0);
        assert_eq!(
            model.windows(PrNumber(1), []),
            Some(vec![(0, 0, o()), (0, 3, regrown(3))])
        );
    }

    #[test]
    fn the_model_lists_its_pairs_and_the_heads_it_holds() {
        let mut model = model(&[1, 2, 3]);
        assert_eq!(model.pairs().count(), 0);
        model.failed(BatchId(1), &members(&[1, 3]), 0);
        assert_eq!(model.pairs().collect::<Vec<_>>(), vec![pair(1, 3)]);
        assert_eq!(model.head(PrNumber(2)), Some(&sha(2)));
        assert_eq!(model.head(PrNumber(9)), None);
    }
}
