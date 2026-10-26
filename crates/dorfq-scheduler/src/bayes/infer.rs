//! Exact posteriors over the evidence, by inclusion–exclusion.
//!
//! A failed run blames a pull request only for the landings between its
//! cleared position and the trunk the run started on, so the inference's
//! variables are, per pull request, the stretches of trunk between the
//! positions of the runs that blame it, each broken independently; and the
//! pairs. A run exercises every stretch of a pull request it blames up to
//! its own position.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use dorfq_core::{BatchId, PrNumber};

use super::model::{Cause, Model};

/// The most runs one component's inference reads; older ones are left out.
pub(crate) const MAX_COMPONENT_RUNS: usize = 12;

/// The largest estimated rounding error a posterior may carry. A component
/// whose failure is too improbable for the alternating sum to resolve
/// exceeds it, and inference fails rather than return noise.
const MAX_ERROR: f64 = 1e-2;

/// A component whose posteriors the alternating sum cannot resolve to
/// [`MAX_ERROR`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Unresolved {
    runs: usize,
    error: f64,
    fail: f64,
}

impl fmt::Display for Unresolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "posteriors over {} runs cannot be resolved: error estimate {:e}, P(fail) {:e}",
            self.runs, self.error, self.fail
        )
    }
}

impl Error for Unresolved {}

/// A variable of the inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Var {
    /// Something reaching trunk in positions `(from, to]` broke the pull
    /// request.
    Seg(PrNumber, i64, i64),
    /// The pair cause.
    Pair(Cause),
}

/// A cause's posterior in one failed run: the chance it held on the trunk
/// that run started on.
pub(crate) type RunPosteriors = Vec<(Cause, f64)>;

/// Posteriors of every cause in the evidence, and the components the
/// evidence falls into.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Inference {
    /// Each run's causes that could have failed it, in the model's evidence
    /// order.
    effective: Vec<BTreeSet<Cause>>,
    /// Each run's position, in the same order.
    at: Vec<i64>,
    /// Indices into the model's evidence, one list per component, oldest
    /// first; components ordered by their oldest run.
    components: Vec<Vec<usize>>,
    /// The component each effective cause belongs to.
    component_of: BTreeMap<Cause, usize>,
    /// Each component's variables and sums.
    solved: Vec<Solved>,
}

impl Inference {
    /// Infers over `model`'s evidence.
    pub(crate) fn of(model: &Model) -> Result<Inference, Unresolved> {
        let effective: Vec<BTreeSet<Cause>> = model
            .evidence()
            .iter()
            .map(|run| model.effective(run).collect())
            .collect();
        let at: Vec<i64> = model.evidence().iter().map(|run| run.at).collect();
        let components = components(&effective);
        let mut component_of = BTreeMap::new();
        let mut solved = Vec::with_capacity(components.len());
        for (c, runs) in components.iter().enumerate() {
            for &r in runs {
                component_of.extend(effective[r].iter().map(|&cause| (cause, c)));
            }
            let all: Vec<(i64, &BTreeSet<Cause>)> =
                runs.iter().map(|&r| (at[r], &effective[r])).collect();
            let component = Solved::new(model, &all, kept(runs).len());
            component.sum.check()?;
            solved.push(component);
        }
        Ok(Inference {
            effective,
            at,
            components,
            component_of,
            solved,
        })
    }

    /// The probability that no cause in `causes` holds now, given that every
    /// run inference read failed. `Bad(x)` is every stretch of `x` since its
    /// cleared position.
    ///
    /// Components are independent given the evidence, and a cause in none
    /// is independent of everything, so the result is a product: `1 − p_c`
    /// for each cause outside every component, and for each component `K`
    /// that `causes` touches, with `C` the variables in it,
    /// `Π_{c ∈ C} (1 − p_c) · P_K(F | C false) / P_K(F)`, the numerator's sum
    /// taken over `K`'s runs with `C` stripped from each. That numerator's
    /// terms are each at most the component's own, so the ratio carries
    /// about the absolute error [`Inference::of`] already bounded for `K`;
    /// it is clamped to `[0, 1]`, which a stripped sum below the rounding
    /// noise reads as 0.
    pub(crate) fn p_none(&self, model: &Model, causes: &BTreeSet<Cause>) -> f64 {
        let mut p = 1.0;
        let mut touched: BTreeMap<usize, BTreeSet<Var>> = BTreeMap::new();
        for &cause in causes {
            match self.component_of.get(&cause) {
                Some(&k) => touched
                    .entry(k)
                    .or_default()
                    .extend(self.solved[k].vars(cause)),
                None => p *= 1.0 - model.belief(cause),
            }
        }
        for (k, strip) in touched {
            p *= self.solved[k].none(&strip, model.priors.p_flake);
        }
        p
    }

    /// The posterior of `cause` now: its belief when no run inference read
    /// exercises it.
    pub(crate) fn posterior(&self, model: &Model, cause: Cause) -> f64 {
        match self.component_of.get(&cause) {
            Some(&k) => self.solved[k].posterior(cause, model.position()),
            None => model.belief(cause),
        }
    }

    /// The posteriors of run `r`'s effective causes, each as of the trunk
    /// that run started on.
    pub(crate) fn run_posteriors(&self, r: usize) -> RunPosteriors {
        self.effective[r]
            .iter()
            .map(|&cause| {
                let k = self.component_of[&cause];
                (cause, self.solved[k].posterior(cause, self.at[r]))
            })
            .collect()
    }

    /// The components, as indices into the model's evidence.
    pub(crate) fn components(&self) -> &[Vec<usize>] {
        &self.components
    }

    /// Every effective cause in a component's `runs`.
    pub(crate) fn component_causes(&self, runs: &[usize]) -> BTreeSet<Cause> {
        runs.iter()
            .flat_map(|&r| self.effective[r].iter().copied())
            .collect()
    }

    /// Run `r`'s causes that could have failed it.
    pub(crate) fn run_causes(&self, r: usize) -> &BTreeSet<Cause> {
        &self.effective[r]
    }

    /// The posteriors were a further run, started now, to fail blaming
    /// `extra`: for each run inference would read in the component that run
    /// would join, oldest first and the hypothetical run last, its batch and
    /// its causes' posteriors as of its own position.
    pub(crate) fn with_failure(
        &self,
        model: &Model,
        extra: &BTreeSet<Cause>,
    ) -> Result<Vec<(Option<BatchId>, RunPosteriors)>, Unresolved> {
        let joined: BTreeSet<usize> = extra
            .iter()
            .filter_map(|cause| self.component_of.get(cause).copied())
            .collect();
        let mut runs: Vec<usize> = joined
            .into_iter()
            .flat_map(|c| self.components[c].iter().copied())
            .collect();
        runs.sort_unstable();
        let kept = &runs[runs.len().saturating_sub(MAX_COMPONENT_RUNS - 1)..];
        let mut all: Vec<(i64, &BTreeSet<Cause>)> = runs
            .iter()
            .map(|&r| (self.at[r], &self.effective[r]))
            .collect();
        all.push((model.position(), extra));
        let solved = Solved::new(model, &all, kept.len() + 1);
        solved.sum.check()?;
        let evidence = model.evidence();
        let of_run = |at: i64, causes: &BTreeSet<Cause>| -> RunPosteriors {
            causes
                .iter()
                .map(|&cause| (cause, solved.posterior(cause, at)))
                .collect()
        };
        Ok(kept
            .iter()
            .map(|&r| {
                (
                    Some(evidence[r].batch),
                    of_run(self.at[r], &self.effective[r]),
                )
            })
            .chain([(None, of_run(model.position(), extra))])
            .collect())
    }
}

/// One component's variables, the sets of them its read runs exercise, and
/// the sums over those.
#[derive(Debug, Clone, PartialEq)]
struct Solved {
    /// Every variable of the component's runs, read or not, with its belief.
    belief: BTreeMap<Var, f64>,
    /// The variables each read run exercises, oldest first.
    sets: Vec<BTreeSet<Var>>,
    sum: Alternating<Var>,
}

impl Solved {
    /// The variables of a component whose runs are `all`, each run's
    /// position and effective causes, oldest first, of which inference reads
    /// the newest `read`. A pull request's stretches are cut at its cleared
    /// position, at the position of every run of `all` that blames it and at
    /// the model's position.
    fn new(model: &Model, all: &[(i64, &BTreeSet<Cause>)], read: usize) -> Solved {
        let mut bounds: BTreeMap<PrNumber, BTreeSet<i64>> = BTreeMap::new();
        let mut belief: BTreeMap<Var, f64> = BTreeMap::new();
        for &(at, causes) in all {
            for &cause in causes {
                match cause {
                    Cause::Bad(x) => {
                        bounds.entry(x).or_default().insert(at);
                    }
                    Cause::Pair(..) => {
                        belief.insert(Var::Pair(cause), model.belief(cause));
                    }
                }
            }
        }
        for (&x, cuts) in &mut bounds {
            let cleared = model.cleared(x).expect("a blamed pull request is held");
            cuts.insert(cleared);
            cuts.insert(model.position());
            let cuts: Vec<i64> = cuts.iter().copied().collect();
            for w in cuts.windows(2) {
                let (a, b) = (w[0], w[1]);
                belief.insert(Var::Seg(x, a, b), model.broken_between(x, a, b));
            }
        }
        let sets: Vec<BTreeSet<Var>> = all[all.len() - read..]
            .iter()
            .map(|&(at, causes)| {
                causes
                    .iter()
                    .flat_map(|&cause| match cause {
                        Cause::Bad(x) => segments(&belief, x)
                            .filter(|(var, _)| matches!(var, Var::Seg(_, _, to) if *to <= at))
                            .map(|(var, _)| var)
                            .collect(),
                        Cause::Pair(..) => vec![Var::Pair(cause)],
                    })
                    .collect()
            })
            .collect();
        let refs: Vec<&BTreeSet<Var>> = sets.iter().collect();
        let sum = Alternating::over(&refs, |var| belief[&var], model.priors.p_flake);
        Solved { belief, sets, sum }
    }

    /// The variables of `cause`: every stretch of a pull request, or the
    /// pair.
    fn vars(&self, cause: Cause) -> Vec<Var> {
        match cause {
            Cause::Bad(x) => segments(&self.belief, x).map(|(var, _)| var).collect(),
            Cause::Pair(..) => vec![Var::Pair(cause)],
        }
    }

    /// The posterior of `cause` at position `at`: for a pull request, that
    /// a stretch of it ending at or before `at` holds.
    ///
    /// A pull request's stretches `s_1 … s_k`, oldest first, are exercised
    /// by nested sets of runs `M_1 ⊇ … ⊇ M_k`, so a run set `S` meets every
    /// `M_i` up to some `j` and none beyond; summing the alternating terms
    /// by that `j`, with `G_j = g[F ∖ M_j]`,
    /// `P(F ∧ none) = (P(F) − G_k) + Σ_j (G_j − G_{j−1}) Π_{i ≥ j} (1 − p_i)`,
    /// `G_0 = 0`.
    fn posterior(&self, cause: Cause, at: i64) -> f64 {
        let fail = self.sum.fail();
        let given = |var: Var| {
            self.sum
                .given(self.sum.masks.get(&var).copied().unwrap_or(0))
        };
        let segments: Vec<(f64, f64)> = match cause {
            Cause::Pair(..) => {
                let var = Var::Pair(cause);
                let p = self.belief.get(&var).copied().unwrap_or(0.0);
                return (p * given(var) / fail).clamp(0.0, 1.0);
            }
            Cause::Bad(x) => segments(&self.belief, x)
                .filter(|(var, _)| matches!(var, Var::Seg(_, _, to) if *to <= at))
                .map(|(var, p)| (given(var), 1.0 - p))
                .collect(),
        };
        let Some(&(last, _)) = segments.last() else {
            return 0.0;
        };
        let mut none = fail - last;
        let mut survive = 1.0;
        for (j, &(g, q)) in segments.iter().enumerate().rev() {
            survive *= q;
            let below = if j == 0 { 0.0 } else { segments[j - 1].0 };
            none += (g - below) * survive;
        }
        (1.0 - none / fail).clamp(0.0, 1.0)
    }

    /// [`Inference::p_none`]'s factor for this component, with `strip` the
    /// variables that must not hold.
    fn none(&self, strip: &BTreeSet<Var>, p_flake: f64) -> f64 {
        let stripped: Vec<BTreeSet<Var>> = self
            .sets
            .iter()
            .map(|set| set.difference(strip).copied().collect())
            .collect();
        let refs: Vec<&BTreeSet<Var>> = stripped.iter().collect();
        let given_none = Alternating::over(&refs, |var| self.belief[&var], p_flake).fail();
        let none: f64 = strip.iter().map(|var| 1.0 - self.belief[var]).product();
        (none * given_none / self.sum.fail()).clamp(0.0, 1.0)
    }
}

/// Pull request `x`'s stretches in `belief`, oldest first.
fn segments(belief: &BTreeMap<Var, f64>, x: PrNumber) -> impl Iterator<Item = (Var, f64)> + '_ {
    belief
        .range(Var::Seg(x, i64::MIN, i64::MIN)..=Var::Seg(x, i64::MAX, i64::MAX))
        .map(|(&var, &p)| (var, p))
}

/// The runs inference reads of a component's `runs`: the newest
/// [`MAX_COMPONENT_RUNS`].
fn kept(runs: &[usize]) -> &[usize] {
    &runs[runs.len().saturating_sub(MAX_COMPONENT_RUNS)..]
}

/// Partitions runs into components linked by a shared cause. Two runs that
/// blame one pull request share its oldest stretch, so these are the
/// components of the variables too.
fn components(effective: &[BTreeSet<Cause>]) -> Vec<Vec<usize>> {
    let mut parent: Vec<usize> = (0..effective.len()).collect();
    fn root(parent: &mut [usize], mut r: usize) -> usize {
        while parent[r] != r {
            parent[r] = parent[parent[r]];
            r = parent[r];
        }
        r
    }
    let mut first: BTreeMap<Cause, usize> = BTreeMap::new();
    for (r, set) in effective.iter().enumerate() {
        for &cause in set {
            let other = *first.entry(cause).or_insert(r);
            let (a, b) = (root(&mut parent, other), root(&mut parent, r));
            if a != b {
                parent[a.max(b)] = a.min(b);
            }
        }
    }
    let mut by_root: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for r in 0..effective.len() {
        let top = root(&mut parent, r);
        by_root.entry(top).or_default().push(r);
    }
    by_root.into_values().collect()
}

/// The alternating sum over a set of runs, given that every one failed.
///
/// `Q(S)` is the probability that every run in `S` passes: no flake and no
/// variable any of them exercises true. `g[T] = Σ_{S ⊆ T} (−1)^|S| Q(S)`, so
/// `P(F fail) = g[F]`, and a variable exercised by runs `R(c)` makes those
/// fail surely, so `P(c | F) = p_c · g[F∖R(c)] / P(F fail)`. Both are
/// subset sums over a mask of at most [`MAX_COMPONENT_RUNS`] runs;
/// variables with one mask share a weight.
#[derive(Debug, Clone, PartialEq)]
struct Alternating<K> {
    /// Each variable's runs, as a mask.
    masks: BTreeMap<K, usize>,
    /// `g[T]` for every mask `T`.
    g: Vec<f64>,
    /// The sum of the signed terms' magnitudes, which the sums cancel.
    magnitude: f64,
    /// How many runs.
    runs: usize,
}

impl<K: Ord + Copy> Alternating<K> {
    /// The sums over `runs`, each variable at `belief`.
    fn over(runs: &[&BTreeSet<K>], belief: impl Fn(K) -> f64, p_flake: f64) -> Self {
        let n = runs.len();
        assert!(n <= MAX_COMPONENT_RUNS, "component of {n} runs");
        let size = 1usize << n;
        let full = size - 1;
        let mut masks: BTreeMap<K, usize> = BTreeMap::new();
        for (r, set) in runs.iter().enumerate() {
            for &var in *set {
                *masks.entry(var).or_default() |= 1 << r;
            }
        }
        // z[T]: Σ ln(1 − p_c) over variables whose runs all lie in T.
        let mut z = vec![0.0f64; size];
        for (&var, &mask) in &masks {
            z[mask] += (1.0 - belief(var)).ln();
        }
        subset_sums(&mut z);
        let ln_pass = (1.0 - p_flake).ln();
        let mut g: Vec<f64> = (0..size)
            .map(|s| {
                let k = s.count_ones();
                let q = (f64::from(k) * ln_pass + z[full] - z[full & !s]).exp();
                if k % 2 == 0 { q } else { -q }
            })
            .collect();
        let magnitude: f64 = g.iter().map(|q| q.abs()).sum();
        subset_sums(&mut g);
        Alternating {
            masks,
            g,
            magnitude,
            runs: n,
        }
    }

    /// `P(F fail)`: every run fails.
    fn fail(&self) -> f64 {
        self.g[self.g.len() - 1]
    }

    /// `g[F ∖ mask]`: every run outside `mask` fails.
    fn given(&self, mask: usize) -> f64 {
        self.g[(self.g.len() - 1) & !mask]
    }

    /// Fails when the sums cannot be resolved to [`MAX_ERROR`]: `P(F fail)`
    /// is not positive, not finite, or small against the terms that cancel
    /// to give it.
    fn check(&self) -> Result<(), Unresolved> {
        let fail = self.fail();
        // The sums round by about ε of the magnitude of the terms they cancel;
        // against brute force this estimate runs well above the error, by a
        // factor that varies with the component.
        let error = f64::EPSILON * self.magnitude / fail;
        // TODO(bayes-posterior-precision): a component explained only by
        // low-belief causes cannot be resolved, so inference fails here rather
        // than eject on noise; recovering instead is unbuilt.
        if fail > 0.0 && error < MAX_ERROR {
            Ok(())
        } else {
            Err(Unresolved {
                runs: self.runs,
                error,
                fail,
            })
        }
    }
}

/// Replaces each `v[T]` with `Σ_{S ⊆ T} v[S]`.
fn subset_sums(v: &mut [f64]) {
    let mut bit = 1;
    while bit < v.len() {
        for t in 0..v.len() {
            if t & bit != 0 {
                v[t] += v[t ^ bit];
            }
        }
        bit <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::Priors;
    use super::super::test_support::{bad, members, model, pair, prior, priors};
    use super::*;
    use crate::Landing;
    use crate::test_support::{ready, stacked, stale};
    use dorfq_core::{BatchId, PrSnapshot};

    fn bads(ns: &[u64]) -> BTreeSet<Cause> {
        ns.iter().map(|&n| bad(n)).collect()
    }

    /// The posterior of every variable of `runs`, given that every run
    /// failed.
    fn posteriors<K: Ord + Copy>(
        runs: &[&BTreeSet<K>],
        belief: impl Fn(K) -> f64,
        p_flake: f64,
    ) -> Result<BTreeMap<K, f64>, Unresolved> {
        let sum = Alternating::over(runs, &belief, p_flake);
        sum.check()?;
        Ok(sum
            .masks
            .iter()
            .map(|(&k, &mask)| {
                (
                    k,
                    (belief(k) * sum.given(mask) / sum.fail()).clamp(0.0, 1.0),
                )
            })
            .collect())
    }

    /// Hand-built runs of `Bad` causes at the belief 0.05.
    fn infer(runs: &[BTreeSet<Cause>]) -> BTreeMap<Cause, f64> {
        let refs: Vec<&BTreeSet<Cause>> = runs.iter().collect();
        posteriors(&refs, |_| 0.05, priors().p_flake).unwrap()
    }

    /// Every assignment of the variables of `runs` and `extra`, weighted by
    /// its probability and that of every run failing: each run fails surely
    /// when one of its variables holds and with probability `p_flake`
    /// otherwise. The variables, and the weights indexed by assignment.
    fn weights<K: Ord + Copy>(
        runs: &[BTreeSet<K>],
        extra: &BTreeSet<K>,
        belief: impl Fn(K) -> f64,
    ) -> (Vec<K>, Vec<f64>) {
        let vars: Vec<K> = runs
            .iter()
            .flatten()
            .chain(extra)
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        assert!(vars.len() <= 16);
        let weights = (0usize..1 << vars.len())
            .map(|assignment| {
                let holds = |c: &K| {
                    let i = vars.iter().position(|x| x == c).unwrap();
                    assignment & (1 << i) != 0
                };
                let mut weight: f64 = vars
                    .iter()
                    .map(|&c| {
                        if holds(&c) {
                            belief(c)
                        } else {
                            1.0 - belief(c)
                        }
                    })
                    .product();
                for run in runs {
                    if !run.iter().any(holds) {
                        weight *= priors().p_flake;
                    }
                }
                weight
            })
            .collect();
        (vars, weights)
    }

    /// The summed weight of the assignments in which no variable in `mask`
    /// holds, over the summed weight of all.
    fn share(weights: &[f64], mask: usize) -> f64 {
        let clear: f64 = weights
            .iter()
            .enumerate()
            .filter(|&(a, _)| a & mask == 0)
            .map(|(_, w)| w)
            .sum();
        clear / weights.iter().sum::<f64>()
    }

    /// `P(c | every run failed)` by summing over every assignment.
    fn brute_force<K: Ord + Copy>(
        runs: &[BTreeSet<K>],
        belief: impl Fn(K) -> f64,
    ) -> BTreeMap<K, f64> {
        let (vars, weights) = weights(runs, &BTreeSet::new(), belief);
        vars.into_iter()
            .enumerate()
            .map(|(i, c)| (c, 1.0 - share(&weights, 1 << i)))
            .collect()
    }

    /// `P(no variable of none holds | every run failed)` by summing over
    /// every assignment.
    fn brute_none<K: Ord + Copy>(
        runs: &[BTreeSet<K>],
        none: &BTreeSet<K>,
        belief: impl Fn(K) -> f64,
    ) -> f64 {
        let (vars, weights) = weights(runs, none, belief);
        let mask: usize = vars
            .iter()
            .enumerate()
            .filter(|(_, c)| none.contains(c))
            .map(|(i, _)| 1 << i)
            .sum();
        share(&weights, mask)
    }

    fn assert_close<K: Ord + Copy + fmt::Debug>(got: &BTreeMap<K, f64>, want: &BTreeMap<K, f64>) {
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>()
        );
        for (k, p) in want {
            assert!((got[k] - p).abs() < 1e-9, "{k:?}: {} against {p}", got[k]);
        }
    }

    /// The whole evidence of `model` as one set of variables, every run
    /// read: what brute force enumerates.
    fn variables(model: &Model) -> Solved {
        let effective: Vec<BTreeSet<Cause>> = model
            .evidence()
            .iter()
            .map(|run| model.effective(run).collect())
            .collect();
        let all: Vec<(i64, &BTreeSet<Cause>)> = model
            .evidence()
            .iter()
            .zip(&effective)
            .map(|(run, causes)| (run.at, causes))
            .collect();
        Solved::new(model, &all, all.len())
    }

    /// `P(cause holds at position at | every run failed)` by brute force
    /// over the model's variables.
    fn brute_at(model: &Model, cause: Cause, at: i64) -> f64 {
        let vars = variables(model);
        let none: BTreeSet<Var> = vars
            .vars(cause)
            .into_iter()
            .filter(|var| !matches!(var, Var::Seg(_, _, to) if *to > at))
            .collect();
        1.0 - brute_none(&vars.sets, &none, |var| vars.belief[&var])
    }

    /// Every posterior inference gives, now and per run, against brute
    /// force.
    fn assert_model_matches_brute_force(model: &Model) {
        let inference = Inference::of(model).unwrap();
        for (r, run) in model.evidence().iter().enumerate() {
            for (cause, p) in inference.run_posteriors(r) {
                let want = brute_at(model, cause, run.at);
                assert!((p - want).abs() < 1e-9, "{r} {cause:?}: {p} against {want}");
                let now = inference.posterior(model, cause);
                let want = brute_at(model, cause, model.position());
                assert!((now - want).abs() < 1e-9, "{cause:?}: {now} against {want}");
            }
        }
    }

    #[test]
    fn one_run_of_three_matches_brute_force() {
        let runs = [bads(&[1, 2, 3])];
        assert_close(&infer(&runs), &brute_force(&runs, |_| 0.05));
    }

    #[test]
    fn two_runs_sharing_a_member_match_brute_force() {
        let runs = [bads(&[1, 2, 3]), bads(&[3, 4])];
        assert_close(&infer(&runs), &brute_force(&runs, |_| 0.05));
        let mut model = model(&[1, 2, 3, 4]);
        model.failed(BatchId(1), &members(&[1, 2, 3]), 0);
        model.failed(BatchId(2), &members(&[3, 4]), 0);
        assert_model_matches_brute_force(&model);
    }

    #[test]
    fn a_chain_whose_child_passed_is_read_through_its_alibi() {
        // Parent {1, 2} failed; child {3} on it failed; child {4} on the
        // parent passed, clearing 1, 2 and their pair.
        let mut model = model(&[1, 2, 3, 4]);
        model.failed(BatchId(1), &members(&[1, 2]), 0);
        model.failed(BatchId(2), &members(&[1, 2, 3]), 0);
        model.passed(&members(&[1, 2, 4]), 0);
        assert_eq!(model.evidence().len(), 1);
        let run = &model.evidence()[0];
        assert_eq!(
            model.effective(run).collect::<Vec<_>>(),
            vec![bad(3), pair(1, 3), pair(2, 3)]
        );
        assert_model_matches_brute_force(&model);
        // A grandchild that passes clears the whole chain: a flake.
        model.passed(&members(&[1, 2, 3]), 0);
        assert!(model.evidence().is_empty());
        assert_eq!(
            Inference::of(&model).unwrap().posterior(&model, bad(3)),
            0.0
        );
    }

    #[test]
    fn a_pair_only_explanation_matches_brute_force() {
        let mut model = model(&[1, 2]);
        model.failed(BatchId(1), &members(&[1, 2]), 0);
        model.passed(&members(&[1]), 0);
        model.passed(&members(&[2]), 0);
        assert_model_matches_brute_force(&model);
        let p = Inference::of(&model).unwrap().posterior(&model, pair(1, 2));
        assert!(
            (p - 0.002 / (0.002 + 0.998 * priors().p_flake)).abs() < 1e-12,
            "{p}"
        );
    }

    #[test]
    fn a_cleared_cause_leaves_the_component() {
        let mut model = model(&[1, 2, 3, 4]);
        model.failed(BatchId(1), &members(&[1, 2, 3]), 0);
        model.failed(BatchId(2), &members(&[3, 4]), 0);
        model.passed(&members(&[2]), 0);
        assert_model_matches_brute_force(&model);
        let inference = Inference::of(&model).unwrap();
        assert!(!inference.effective[0].contains(&bad(2)));
        assert_eq!(inference.posterior(&model, bad(2)), 0.0);
    }

    #[test]
    fn a_lone_failure_gives_the_closed_form() {
        let p = infer(&[bads(&[1])])[&bad(1)];
        assert!((p - 0.05 / 0.069).abs() < 1e-12, "{p}");
    }

    #[test]
    fn one_failed_run_of_twenty_gives_each_member_its_exact_share() {
        let members: Vec<u64> = (1..=20).collect();
        let posterior = infer(&[bads(&members)]);
        let exact = 0.05 / (1.0 - 0.98 * 0.95f64.powi(20));
        for p in posterior.values() {
            assert!((p - exact).abs() < 1e-12, "{p} against {exact}");
        }
        assert!((exact - 0.077).abs() < 5e-4, "{exact}");
    }

    #[test]
    fn a_failed_batch_then_lone_failures_settle_the_suspect() {
        let members: Vec<u64> = (1..=20).collect();
        let once = infer(&[bads(&members), bads(&[1])])[&bad(1)];
        assert!((once - 0.806).abs() < 1e-3, "{once}");
        let twice = infer(&[bads(&members), bads(&[1]), bads(&[1])])[&bad(1)];
        assert!((twice - 0.995).abs() < 1e-3, "{twice}");
    }

    #[test]
    fn three_failed_pairs_sharing_a_member_single_it_out_by_coincidence() {
        let runs = [bads(&[1, 2]), bads(&[1, 3]), bads(&[1, 4])];
        let posterior = infer(&runs);
        assert_close(&posterior, &brute_force(&runs, |_| 0.05));
        assert!((posterior[&bad(1)] - 0.994).abs() < 1e-3, "{posterior:?}");
        for n in 2..=4 {
            assert!((posterior[&bad(n)] - 0.054).abs() < 1e-3, "{posterior:?}");
        }
    }

    #[test]
    fn a_hundred_cross_pairs_share_the_blame_with_the_flake() {
        let cross: BTreeSet<Cause> = (1..=10)
            .flat_map(|a| (11..=20).map(move |b| pair(a, b)))
            .collect();
        let refs = [&cross];
        let posterior = posteriors(&refs, |_| 0.002, priors().p_flake).unwrap();
        let exact = 0.002 / (1.0 - 0.98 * 0.998f64.powi(100));
        assert_eq!(posterior.len(), 100);
        for p in posterior.values() {
            assert!((p - exact).abs() < 1e-12, "{p} against {exact}");
        }
        assert!((exact - 0.010).abs() < 5e-4, "{exact}");
    }

    #[test]
    fn components_are_runs_linked_by_a_shared_cause() {
        let mut model = model(&[1, 2, 3, 4, 5]);
        model.failed(BatchId(1), &members(&[1, 2]), 0);
        model.failed(BatchId(2), &members(&[4]), 0);
        model.failed(BatchId(3), &members(&[2, 3]), 0);
        model.failed(BatchId(4), &members(&[5]), 0);
        model.failed(BatchId(5), &members(&[4, 5]), 0);
        let inference = Inference::of(&model).unwrap();
        assert_eq!(inference.components(), &[vec![0, 2], vec![1, 3, 4]]);
    }

    #[test]
    fn the_component_cap_drops_the_oldest_runs() {
        let mut model = model(&[1, 2]);
        model.failed(BatchId(1), &members(&[1, 2]), 0);
        model.failed(BatchId(2), &members(&[1, 2]), 0);
        for id in 3..=14 {
            model.failed(BatchId(id), &members(&[2]), 0);
        }
        let inference = Inference::of(&model).unwrap();
        assert_eq!(inference.components(), &[(0..14).collect::<Vec<_>>()]);
        // 1 appears only in the two runs left out, so it keeps its belief.
        assert!((inference.posterior(&model, bad(1)) - prior()).abs() < 1e-15);
        assert!((inference.posterior(&model, pair(1, 2)) - 0.002).abs() < 1e-15);
        let lone = vec![bads(&[2]); MAX_COMPONENT_RUNS];
        let refs: Vec<&BTreeSet<Cause>> = lone.iter().collect();
        let want = posteriors(&refs, |_| prior(), priors().p_flake).unwrap()[&bad(2)];
        assert!((inference.posterior(&model, bad(2)) - want).abs() < 1e-12);
    }

    /// Runs `{r, r + 1}` for `r` in `1..=n`.
    fn chain_of(n: u64) -> Vec<BTreeSet<Cause>> {
        (1..=n).map(|r| bads(&[r, r + 1])).collect()
    }

    #[test]
    fn twelve_chained_runs_at_the_prior_match_brute_force() {
        let runs = chain_of(12);
        let refs: Vec<&BTreeSet<Cause>> = runs.iter().collect();
        let got = posteriors(&refs, |_| 0.05, priors().p_flake).unwrap();
        for (cause, p) in brute_force(&runs, |_| 0.05) {
            assert!(
                (got[&cause] - p).abs() < 1e-6,
                "{cause:?}: {} against {p}",
                got[&cause]
            );
        }
    }

    #[test]
    fn chained_runs_too_improbable_to_resolve_are_an_error() {
        // Exact posteriors near 0.035; the alternating sum returns 0.
        let runs = chain_of(11);
        let refs: Vec<&BTreeSet<Cause>> = runs.iter().collect();
        let error = posteriors(&refs, |_| 0.002, priors().p_flake).unwrap_err();
        assert_eq!(error.runs, 11);
        assert!(
            error
                .to_string()
                .contains("over 11 runs cannot be resolved"),
            "{error}"
        );
    }

    #[test]
    fn a_certain_cause_is_an_error_rather_than_nan() {
        let runs = [bads(&[1, 2]), bads(&[2, 3])];
        let refs: Vec<&BTreeSet<Cause>> = runs.iter().collect();
        let result = posteriors(
            &refs,
            |c| if c == bad(1) { 1.0 } else { 0.05 },
            priors().p_flake,
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("cannot be resolved"),
            "a certain cause resolves"
        );
    }

    /// A model at `p_interact`, holding `prs` each `behind` landings back
    /// and stacked on the one before so that no two are a pair, in which
    /// each of `runs` failed at position 0.
    fn hand(p_interact: f64, behind: u32, prs: &[u64], runs: &[BTreeSet<Cause>]) -> Model {
        let priors = Priors {
            p_interact,
            ..priors()
        };
        let mut model = Model::new(priors, Landing::Decay);
        let snapshots: Vec<_> = prs
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let pr = if i == 0 {
                    ready(n)
                } else {
                    stacked(n, prs[i - 1])
                };
                stale(pr, behind)
            })
            .collect();
        model.sync(&snapshots, &BTreeSet::new(), 0);
        for (id, run) in runs.iter().enumerate() {
            let tree: Vec<u64> = run.iter().flat_map(Cause::prs).map(|pr| pr.0).collect();
            model.failed(BatchId(id as u64), &members(&tree), 0);
        }
        model
    }

    /// [`Inference::p_none`] of `none` over the model's evidence, checked
    /// against brute force to `1e-9` and to `[0, 1]`.
    fn none_matches_brute_force(model: &Model, none: &BTreeSet<Cause>) -> f64 {
        let got = Inference::of(model).unwrap().p_none(model, none);
        let vars = variables(model);
        let mut strip: BTreeSet<Var> = BTreeSet::new();
        let mut outside = 1.0;
        for &cause in none {
            let of = vars.vars(cause);
            if of.is_empty() {
                outside *= 1.0 - model.belief(cause);
            }
            strip.extend(of);
        }
        let want = outside * brute_none(&vars.sets, &strip, |var| vars.belief[&var]);
        assert!((got - want).abs() < 1e-9, "{got} against {want}");
        assert!((0.0..=1.0).contains(&got), "{got}");
        got
    }

    #[test]
    fn p_none_of_causes_outside_the_evidence_is_the_product() {
        let model = hand(0.002, 25, &[1, 2, 8, 9], &[bads(&[1, 2])]);
        let got = none_matches_brute_force(&model, &bads(&[8, 9]));
        assert!((got - (1.0 - prior()).powi(2)).abs() < 1e-15, "{got}");
    }

    #[test]
    fn p_none_of_a_whole_run_is_its_flake() {
        let model = hand(0.002, 25, &[1, 2, 3], &[bads(&[1, 2, 3])]);
        let got = none_matches_brute_force(&model, &bads(&[1, 2, 3]));
        let none = (1.0 - prior()).powi(3);
        let fail = 1.0 - 0.98 * none;
        assert!((got - 0.02 * none / fail).abs() < 1e-12, "{got}");
    }

    #[test]
    fn p_none_of_a_unit_failed_twice_is_far_below_the_product_of_marginals() {
        let prs: Vec<u64> = (25..=31).collect();
        let unit = bads(&prs);
        let runs = [unit.clone(), unit.clone()];
        let model = hand(0.001, 5, &prs, &runs);
        let joint = none_matches_brute_force(&model, &unit);
        let inference = Inference::of(&model).unwrap();
        let marginals: f64 = unit
            .iter()
            .map(|&c| 1.0 - inference.posterior(&model, c))
            .product();
        assert!(joint < 0.02 && marginals > 0.3, "{joint} {marginals}");
        let model = hand(0.001, 1, &prs, &runs);
        let joint = none_matches_brute_force(&model, &unit);
        assert!((joint - 0.054).abs() < 1e-3, "{joint}");
    }

    #[test]
    fn p_none_strips_a_shared_cause_and_leaves_runs_that_share_nothing() {
        let model = hand(0.002, 25, &[1, 2, 3], &[bads(&[1, 3]), bads(&[2, 3])]);
        none_matches_brute_force(&model, &bads(&[3]));
        none_matches_brute_force(&model, &bads(&[1, 3]));
    }

    #[test]
    fn p_none_multiplies_over_the_components_it_touches() {
        let runs = [bads(&[1, 2]), bads(&[3, 4])];
        let model = hand(0.002, 25, &[1, 2, 3, 4, 9], &runs);
        none_matches_brute_force(&model, &bads(&[1, 3, 9]));
        // The component of 3 and 4 is untouched and contributes nothing.
        let one = none_matches_brute_force(&model, &bads(&[1, 2]));
        let alone = hand(0.002, 25, &[1, 2], &runs[..1]);
        let alone = none_matches_brute_force(&alone, &bads(&[1, 2]));
        assert!((one - alone).abs() < 1e-15, "{one} {alone}");
    }

    #[test]
    fn p_none_resolves_a_stripped_sum_below_the_rounding_noise() {
        // Twelve failures of {1, 2}: with both stripped only flakes are
        // left, and the stripped sum is p_flake^12.
        let runs = vec![bads(&[1, 2]); MAX_COMPONENT_RUNS];
        let model = hand(0.001, 1, &[1, 2], &runs);
        let got = none_matches_brute_force(&model, &bads(&[1, 2]));
        assert!(got < 1e-9, "{got}");
        // Runs {x, r, r + 1}: stripped of x, a chain of ten whose sum the
        // relative check rejects, while the component's own sum resolves.
        let x = 100;
        let chain: Vec<BTreeSet<Cause>> = (1..=10).map(|r| bads(&[x, r, r + 1])).collect();
        let prs: Vec<u64> = (1..=11).chain([x]).collect();
        let model = hand(0.001, 1, &prs, &chain);
        let got = none_matches_brute_force(&model, &bads(&[x]));
        assert!(got < 1e-9, "{got}");
        let stripped = chain_of(10);
        let refs: Vec<&BTreeSet<Cause>> = stripped.iter().collect();
        let regrown = model.regrown(1);
        assert!(posteriors(&refs, |_| regrown, priors().p_flake).is_err());
    }

    #[test]
    fn a_pull_request_blamed_at_three_positions_has_three_nested_stretches() {
        // 1, 2 and 3 on trunk, each five landings behind at position 0.
        let prs = |landings: u32| -> Vec<PrSnapshot> {
            (1..=3).map(|n| stale(ready(n), 5 + landings)).collect()
        };
        let mut model = Model::new(priors(), Landing::Decay);
        model.sync(&prs(0), &BTreeSet::new(), 0);
        model.failed(BatchId(1), &members(&[1, 2]), 0);
        model.sync(&prs(4), &BTreeSet::new(), 4);
        model.failed(BatchId(2), &members(&[1, 3]), 4);
        model.sync(&prs(7), &BTreeSet::new(), 7);
        model.failed(BatchId(3), &members(&[1]), 7);
        let one = PrNumber(1);
        let inference = Inference::of(&model).unwrap();
        let solved = &inference.solved[0];
        let stretches: Vec<(Var, f64, usize)> = segments(&solved.belief, one)
            .map(|(var, p)| (var, p, solved.sum.masks[&var]))
            .collect();
        assert_eq!(
            stretches,
            vec![
                (Var::Seg(one, -5, 0), model.regrown(5), 0b111),
                (Var::Seg(one, 0, 4), model.regrown(4), 0b110),
                (Var::Seg(one, 4, 7), model.regrown(3), 0b100),
            ]
        );
        assert_model_matches_brute_force(&model);
        let at: Vec<f64> = (0..3)
            .map(|r| {
                inference
                    .run_posteriors(r)
                    .into_iter()
                    .find(|&(cause, _)| cause == bad(1))
                    .unwrap()
                    .1
            })
            .collect();
        assert!(at[0] < at[1] && at[1] < at[2], "{at:?}");
        assert_eq!(inference.posterior(&model, bad(1)), at[2]);

        // 3 passes covered at 4: the run started at 4 no longer blames it.
        model.passed(&members(&[3]), 4);
        let inference = Inference::of(&model).unwrap();
        assert!(!inference.effective[1].contains(&bad(3)));
        let vars = &inference.solved[0].belief;
        assert!(segments(vars, PrNumber(3)).next().is_none());
        assert_model_matches_brute_force(&model);
    }

    #[test]
    fn a_failure_cleared_of_one_member_is_not_reblamed_on_it_by_later_landings() {
        let prs = |landings: u32| -> Vec<PrSnapshot> {
            (41..=45).map(|n| stale(ready(n), 5 + landings)).collect()
        };
        let mut model = Model::new(priors(), Landing::Decay);
        model.sync(&prs(0), &BTreeSet::new(), 0);
        model.failed(BatchId(1), &members(&[41, 42, 43, 44, 45]), 0);
        model.passed(&members(&[41]), 0);
        model.sync(&prs(3), &BTreeSet::new(), 3);
        let inference = Inference::of(&model).unwrap();
        assert_eq!(inference.posterior(&model, bad(41)), model.regrown(3));
        // The run still blames 42–45 up to its own trunk, and the pairs
        // among all five; each is bad now if bad then or broken since.
        let vars = variables(&model);
        assert_eq!(vars.sets[0].len(), 14);
        for n in 42..=45 {
            let then = Var::Seg(PrNumber(n), -5, 0);
            let bad_then =
                1.0 - brute_none(&vars.sets, &BTreeSet::from([then]), |var| vars.belief[&var]);
            let now = 1.0 - (1.0 - bad_then) * (1.0 - model.regrown(3));
            let got = inference.posterior(&model, bad(n));
            assert!((got - now).abs() < 1e-12, "{n}: {got} against {now}");
        }
    }
}
