//! Exact posteriors over the evidence, by inclusion–exclusion.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

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

/// Posteriors of every cause in the evidence, and the components the
/// evidence falls into.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Inference {
    posterior: BTreeMap<Cause, f64>,
    /// Each run's causes above the floor, in the model's evidence order.
    effective: Vec<BTreeSet<Cause>>,
    /// Indices into the model's evidence, one list per component, oldest
    /// first; components ordered by their oldest run.
    components: Vec<Vec<usize>>,
    /// The component each effective cause belongs to.
    component_of: BTreeMap<Cause, usize>,
}

impl Inference {
    /// Infers over `model`'s evidence.
    pub(crate) fn of(model: &Model) -> Result<Inference, Unresolved> {
        let effective: Vec<BTreeSet<Cause>> = model
            .evidence()
            .iter()
            .map(|run| model.effective(run).collect())
            .collect();
        let components = components(&effective);
        let mut component_of = BTreeMap::new();
        for (c, runs) in components.iter().enumerate() {
            for &r in runs {
                component_of.extend(effective[r].iter().map(|&cause| (cause, c)));
            }
        }
        let mut posterior = BTreeMap::new();
        for runs in &components {
            let kept = &runs[runs.len().saturating_sub(MAX_COMPONENT_RUNS)..];
            let sets: Vec<&BTreeSet<Cause>> = kept.iter().map(|&r| &effective[r]).collect();
            posterior.extend(posteriors(
                &sets,
                |cause| model.belief(cause),
                model.priors.p_flake,
            )?);
        }
        Ok(Inference {
            posterior,
            effective,
            components,
            component_of,
        })
    }

    /// The posterior of `cause`: its belief when no run inference read
    /// exercises it.
    pub(crate) fn posterior(&self, model: &Model, cause: Cause) -> f64 {
        self.posterior
            .get(&cause)
            .copied()
            .unwrap_or_else(|| model.belief(cause))
    }

    /// The components, as indices into the model's evidence.
    pub(crate) fn components(&self) -> &[Vec<usize>] {
        &self.components
    }

    /// Each run's causes above the floor, in the model's evidence order.
    pub(crate) fn effective(&self) -> &[BTreeSet<Cause>] {
        &self.effective
    }

    /// The posteriors were a further run with causes `extra` to fail: over
    /// the component that run would join, with the existing runs inference
    /// would read in it, oldest first.
    pub(crate) fn with_failure(
        &self,
        model: &Model,
        extra: &BTreeSet<Cause>,
    ) -> Result<(Vec<usize>, BTreeMap<Cause, f64>), Unresolved> {
        let joined: BTreeSet<usize> = extra
            .iter()
            .filter_map(|cause| self.component_of.get(cause).copied())
            .collect();
        let mut runs: Vec<usize> = joined
            .into_iter()
            .flat_map(|c| self.components[c].iter().copied())
            .collect();
        runs.sort_unstable();
        let kept = runs.split_off(runs.len().saturating_sub(MAX_COMPONENT_RUNS - 1));
        let mut sets: Vec<&BTreeSet<Cause>> = kept.iter().map(|&r| &self.effective[r]).collect();
        sets.push(extra);
        let posterior = posteriors(&sets, |cause| model.belief(cause), model.priors.p_flake)?;
        Ok((kept, posterior))
    }
}

/// Partitions runs into components linked by a shared cause.
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

/// The posterior of every cause of `runs`, given that every run failed.
///
/// `Q(S)` is the probability that every run in `S` passes: no flake and no
/// cause any of them exercises true. `P(F fail) = Σ_S (−1)^|S| Q(S)`, and a
/// cause exercised by runs `R(c)` makes those fail surely, so
/// `P(c | F) = p_c · Σ_{S ⊆ F∖R(c)} (−1)^|S| Q(S) / P(F fail)`. Both sums are
/// subset sums over a mask of at most [`MAX_COMPONENT_RUNS`] runs; causes
/// with one mask share a weight.
///
/// Fails when the sums cannot be resolved to [`MAX_ERROR`]: `P(F fail)` is
/// not positive, not finite, or small against the terms that cancel to give
/// it.
fn posteriors(
    runs: &[&BTreeSet<Cause>],
    belief: impl Fn(Cause) -> f64,
    p_flake: f64,
) -> Result<BTreeMap<Cause, f64>, Unresolved> {
    let n = runs.len();
    assert!(n <= MAX_COMPONENT_RUNS, "component of {n} runs");
    let size = 1usize << n;
    let full = size - 1;
    let mut masks: BTreeMap<Cause, usize> = BTreeMap::new();
    for (r, set) in runs.iter().enumerate() {
        for &cause in *set {
            *masks.entry(cause).or_default() |= 1 << r;
        }
    }
    // z[T]: Σ ln(1 − p_c) over causes whose runs all lie in T.
    let mut z = vec![0.0f64; size];
    for (&cause, &mask) in &masks {
        z[mask] += (1.0 - belief(cause)).ln();
    }
    subset_sums(&mut z);
    let ln_pass = (1.0 - p_flake).ln();
    // g[T]: Σ_{S ⊆ T} (−1)^|S| Q(S), from the signed terms.
    let mut g: Vec<f64> = (0..size)
        .map(|s| {
            let k = s.count_ones();
            let q = (f64::from(k) * ln_pass + z[full] - z[full & !s]).exp();
            if k % 2 == 0 { q } else { -q }
        })
        .collect();
    let magnitude: f64 = g.iter().map(|q| q.abs()).sum();
    subset_sums(&mut g);
    let fail = g[full];
    // The sums round by about ε of the magnitude of the terms they cancel;
    // against brute force this estimate runs well above the error, by a
    // factor that varies with the component.
    let error = f64::EPSILON * magnitude / fail;
    // TODO(bayes-posterior-precision): a component explained only by
    // low-belief causes cannot be resolved, so inference fails here rather
    // than eject on noise; recovering instead is unbuilt.
    if !(fail > 0.0 && error < MAX_ERROR) {
        return Err(Unresolved {
            runs: n,
            error,
            fail,
        });
    }
    Ok(masks
        .into_iter()
        .map(|(cause, mask)| {
            let p = belief(cause) * g[full & !mask] / fail;
            (cause, p.clamp(0.0, 1.0))
        })
        .collect())
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
    use super::super::model::FLOOR;
    use super::super::test_support::{bad, members, model, pair, priors};
    use super::*;
    use dorfq_core::BatchId;

    fn bads(ns: &[u64]) -> BTreeSet<Cause> {
        ns.iter().map(|&n| bad(n)).collect()
    }

    /// Hand-built runs of `Bad` causes at the prior 0.05.
    fn infer(runs: &[BTreeSet<Cause>]) -> BTreeMap<Cause, f64> {
        let refs: Vec<&BTreeSet<Cause>> = runs.iter().collect();
        posteriors(&refs, |_| 0.05, priors().p_flake).unwrap()
    }

    /// `P(c | every run failed)` by summing over every assignment of the
    /// causes, each run failing surely when one of its causes holds and
    /// with probability `p_flake` otherwise.
    fn brute_force(
        runs: &[BTreeSet<Cause>],
        belief: impl Fn(Cause) -> f64,
    ) -> BTreeMap<Cause, f64> {
        let causes: Vec<Cause> = runs
            .iter()
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        assert!(causes.len() <= 16);
        let mut fail = 0.0;
        let mut with = vec![0.0; causes.len()];
        for assignment in 0u32..1 << causes.len() {
            let holds = |c: &Cause| {
                let i = causes.iter().position(|x| x == c).unwrap();
                assignment & (1 << i) != 0
            };
            let mut weight: f64 = causes
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
            fail += weight;
            for (i, w) in with.iter_mut().enumerate() {
                if assignment & (1 << i) != 0 {
                    *w += weight;
                }
            }
        }
        causes
            .into_iter()
            .zip(with)
            .map(|(c, w)| (c, w / fail))
            .collect()
    }

    fn assert_close(got: &BTreeMap<Cause, f64>, want: &BTreeMap<Cause, f64>) {
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>()
        );
        for (cause, p) in want {
            assert!(
                (got[cause] - p).abs() < 1e-9,
                "{cause:?}: {} against {p}",
                got[cause]
            );
        }
    }

    /// The model's evidence as effective sets, inferred both ways.
    fn assert_model_matches_brute_force(model: &Model) {
        let runs: Vec<BTreeSet<Cause>> = model
            .evidence()
            .iter()
            .map(|run| model.effective(run).collect())
            .collect();
        let inference = Inference::of(model).unwrap();
        let got: BTreeMap<Cause, f64> = runs
            .iter()
            .flatten()
            .map(|&c| (c, inference.posterior(model, c)))
            .collect();
        assert_close(&got, &brute_force(&runs, |c| model.belief(c)));
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
        model.failed(BatchId(1), &members(&[1, 2, 3]));
        model.failed(BatchId(2), &members(&[3, 4]));
        assert_model_matches_brute_force(&model);
    }

    #[test]
    fn a_chain_whose_child_passed_is_read_through_its_alibi() {
        // Parent {1, 2} failed; child {3} on it failed; child {4} on the
        // parent passed, clearing 1, 2 and their pair.
        let mut model = model(&[1, 2, 3, 4]);
        model.failed(BatchId(1), &members(&[1, 2]));
        model.failed(BatchId(2), &members(&[1, 2, 3]));
        model.passed(&members(&[1, 2, 4]));
        assert_eq!(model.evidence().len(), 1);
        let run = &model.evidence()[0];
        assert_eq!(
            model.effective(run).collect::<Vec<_>>(),
            vec![bad(3), pair(1, 3), pair(2, 3)]
        );
        assert_model_matches_brute_force(&model);
        // A grandchild that passes clears the whole chain: a flake.
        model.passed(&members(&[1, 2, 3]));
        assert!(model.evidence().is_empty());
        assert_eq!(
            Inference::of(&model).unwrap().posterior(&model, bad(3)),
            FLOOR
        );
    }

    #[test]
    fn a_pair_only_explanation_matches_brute_force() {
        let mut model = model(&[1, 2]);
        model.failed(BatchId(1), &members(&[1, 2]));
        model.passed(&members(&[1]));
        model.passed(&members(&[2]));
        assert_model_matches_brute_force(&model);
        let p = Inference::of(&model).unwrap().posterior(&model, pair(1, 2));
        assert!(
            (p - 0.002 / (0.002 + 0.998 * priors().p_flake)).abs() < 1e-12,
            "{p}"
        );
    }

    #[test]
    fn a_component_with_a_cause_at_the_floor_matches_brute_force() {
        let mut model = model(&[1, 2, 3, 4]);
        model.failed(BatchId(1), &members(&[1, 2, 3]));
        model.failed(BatchId(2), &members(&[3, 4]));
        model.passed(&members(&[2]));
        assert_model_matches_brute_force(&model);
        assert_eq!(
            Inference::of(&model).unwrap().posterior(&model, bad(2)),
            FLOOR
        );
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
        model.failed(BatchId(1), &members(&[1, 2]));
        model.failed(BatchId(2), &members(&[4]));
        model.failed(BatchId(3), &members(&[2, 3]));
        model.failed(BatchId(4), &members(&[5]));
        model.failed(BatchId(5), &members(&[4, 5]));
        let inference = Inference::of(&model).unwrap();
        assert_eq!(inference.components(), &[vec![0, 2], vec![1, 3, 4]]);
    }

    #[test]
    fn the_component_cap_drops_the_oldest_runs() {
        let mut model = model(&[1, 2]);
        model.failed(BatchId(1), &members(&[1, 2]));
        model.failed(BatchId(2), &members(&[1, 2]));
        for id in 3..=14 {
            model.failed(BatchId(id), &members(&[2]));
        }
        let inference = Inference::of(&model).unwrap();
        assert_eq!(inference.components(), &[(0..14).collect::<Vec<_>>()]);
        // 1 appears only in the two runs left out, so it keeps its belief.
        assert_eq!(inference.posterior(&model, bad(1)), 0.05);
        assert_eq!(inference.posterior(&model, pair(1, 2)), 0.002);
        let lone = vec![bads(&[2]); MAX_COMPONENT_RUNS];
        assert!((inference.posterior(&model, bad(2)) - infer(&lone)[&bad(2)]).abs() < 1e-12);
    }

    /// Runs `{r, r + 1}` for `r` in `1..=n`, every cause at `belief`.
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
}
