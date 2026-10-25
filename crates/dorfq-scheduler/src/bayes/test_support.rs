//! Builders for the Bayesian scheduler's model-level tests.

use std::collections::BTreeSet;

use dorfq_core::PrNumber;

use super::model::{Cause, Model, Priors};
use crate::test_support::{ready_all, sha};
use crate::{BayesConfig, Charge, Landing, Member, Speculation};

pub(super) fn priors() -> Priors {
    Priors {
        p_bad: 0.05,
        p_interact: 0.002,
        p_flake: 0.02,
    }
}

/// The scheduler's parameters at [`priors`], every flag at the status quo.
pub(super) fn params() -> BayesConfig {
    let priors = priors();
    BayesConfig {
        lanes: 2,
        max_batch: 24,
        p_bad: priors.p_bad,
        p_interact: priors.p_interact,
        p_flake: priors.p_flake,
        run_cost: 0.5,
        w_reject: 1.0,
        eject_above: 0.9,
        others_below: 0.1,
        speculate_above: 0.0,
        speculation: Speculation::Chain,
        charge: Charge::Discounted,
        landing: Landing::Decay,
    }
}

/// A model at [`priors`] holding `prs` at their original heads.
pub(super) fn model(prs: &[u64]) -> Model {
    let mut model = Model::new(priors(), Landing::Decay);
    model.sync(&ready_all(prs), &BTreeSet::new());
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
