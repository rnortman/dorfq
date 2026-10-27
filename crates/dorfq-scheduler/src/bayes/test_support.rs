//! Builders for the Bayesian scheduler's model-level tests.

use std::collections::BTreeSet;

use dorfq_core::{BatchId, PrNumber};

use super::infer::Inference;
use super::model::{Cause, Model, Priors};
use crate::test_support::{BEHIND, ready_all, sha};
use crate::{BayesConfig, Member, Speculation};

pub(super) fn priors() -> Priors {
    Priors {
        p_interact: 0.002,
        p_flake: 0.02,
        p_miss: 0.02,
        p_trunk: 0.001,
    }
}

/// The belief a pull request from [`crate::test_support`]'s builders starts
/// at before any pass: [`BEHIND`] landings past its origin at [`priors`],
/// and its own content.
pub(super) fn prior() -> f64 {
    let model = Model::new(priors());
    let content = model.discounted(priors().p_interact, 1);
    1.0 - (1.0 - content) * (1.0 - model.regrown(i64::from(BEHIND)))
}

/// The scheduler's parameters at [`priors`], every flag at the status quo.
pub(super) fn params() -> BayesConfig {
    let priors = priors();
    BayesConfig {
        lanes: 2,
        max_batch: 24,
        p_interact: priors.p_interact,
        p_flake: priors.p_flake,
        p_miss: priors.p_miss,
        p_trunk: priors.p_trunk,
        run_cost: 0.5,
        w_reject: 1.0,
        w_age: 0.0,
        eject_above: 0.9,
        others_below: 0.1,
        speculate_above: 0.0,
        speculation: Speculation::Chain,
    }
}

/// A model at [`priors`] holding `prs` at their original heads, at trunk
/// position 0.
pub(super) fn model(prs: &[u64]) -> Model {
    let mut model = Model::new(priors());
    model.sync(&ready_all(prs), &BTreeSet::new(), 0, 0);
    model
}

/// Pull requests at their original heads.
pub(super) fn members(prs: &[u64]) -> Vec<Member> {
    prs.iter().map(|&n| (PrNumber(n), sha(n))).collect()
}

/// Run `2·id + 1` failed over `tree` at position 0 in a stretch of its
/// own, opened by a pass on trunk, `2·id`, just before it.
pub(super) fn failed_alone(model: &mut Model, id: u64, tree: &[Member]) {
    model.passed(BatchId(2 * id), &[], 0, true);
    model.failed(BatchId(2 * id + 1), tree, 0);
}

pub(super) fn bad(n: u64) -> Cause {
    Cause::Bad(PrNumber(n))
}

pub(super) fn pair(a: u64, b: u64) -> Cause {
    Cause::pair(PrNumber(a), PrNumber(b))
}

/// Asserts that a pass of every set in `sets` would discount a pull-request
/// cause of a failed run inference reads: what a run of the set would blame
/// meets the causes the evidence still blames, trunk aside on both sides.
pub(super) fn informative(model: &Model, inference: &Inference, sets: &[Vec<Member>]) {
    let prs = |causes: BTreeSet<Cause>| -> BTreeSet<Cause> {
        causes
            .into_iter()
            .filter(|cause| !matches!(cause, Cause::Trunk(_)))
            .collect()
    };
    let blamed = prs(inference
        .components()
        .iter()
        .flat_map(|runs| inference.component_causes(runs))
        .collect());
    for set in sets {
        assert!(
            !prs(model.would_blame(set)).is_disjoint(&blamed),
            "{set:?} teaches nothing: {blamed:?}"
        );
    }
}
