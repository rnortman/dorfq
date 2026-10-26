//! Builders for the Bayesian scheduler's model-level tests.

use std::collections::BTreeSet;

use dorfq_core::PrNumber;

use super::infer::Inference;
use super::model::{Cause, Model, Priors};
use crate::test_support::{BEHIND, ready_all, sha};
use crate::{BayesConfig, Charge, Landing, Member, Speculation};

pub(super) fn priors() -> Priors {
    Priors {
        p_interact: 0.002,
        p_flake: 0.02,
    }
}

/// The belief a pull request from [`crate::test_support`]'s builders starts
/// at: [`BEHIND`] landings past its cleared position at [`priors`].
pub(super) fn prior() -> f64 {
    Model::new(priors(), Landing::Decay).regrown(i64::from(BEHIND))
}

/// The scheduler's parameters at [`priors`], every flag at the status quo.
pub(super) fn params() -> BayesConfig {
    let priors = priors();
    BayesConfig {
        lanes: 2,
        max_batch: 24,
        p_interact: priors.p_interact,
        p_flake: priors.p_flake,
        run_cost: 0.5,
        w_reject: 1.0,
        w_age: 0.0,
        eject_above: 0.9,
        others_below: 0.1,
        speculate_above: 0.0,
        speculation: Speculation::Chain,
        charge: Charge::Discounted,
        landing: Landing::Decay,
    }
}

/// A model at [`priors`] holding `prs` at their original heads, at trunk
/// position 0.
pub(super) fn model(prs: &[u64]) -> Model {
    let mut model = Model::new(priors(), Landing::Decay);
    model.sync(&ready_all(prs), &BTreeSet::new(), 0);
    model
}

/// Pull requests at their original heads.
pub(super) fn members(prs: &[u64]) -> Vec<Member> {
    prs.iter().map(|&n| (PrNumber(n), sha(n))).collect()
}

pub(super) fn bad(n: u64) -> Cause {
    Cause::Bad(PrNumber(n))
}

pub(super) fn pair(a: u64, b: u64) -> Cause {
    Cause::pair(PrNumber(a), PrNumber(b))
}

/// Asserts that a pass of every set in `sets` would strip a cause from a
/// failed run inference reads: what a run of the set would blame meets the
/// causes the evidence still blames.
pub(super) fn informative(model: &Model, inference: &Inference, sets: &[Vec<Member>]) {
    let blamed: BTreeSet<Cause> = inference
        .components()
        .iter()
        .flat_map(|runs| inference.component_causes(runs))
        .collect();
    for set in sets {
        assert!(
            !model.would_blame(set).is_disjoint(&blamed),
            "{set:?} teaches nothing: {blamed:?}"
        );
    }
}
