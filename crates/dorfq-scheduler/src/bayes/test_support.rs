//! Builders for the Bayesian scheduler's model-level tests.

use std::collections::BTreeSet;

use dorfq_core::PrNumber;

use super::model::{Cause, Model, Priors};
use crate::Member;
use crate::test_support::{ready_all, sha};

pub(super) fn priors() -> Priors {
    Priors {
        p_bad: 0.05,
        p_interact: 0.002,
        p_flake: 0.02,
    }
}

/// A model at [`priors`] holding `prs` at their original heads.
pub(super) fn model(prs: &[u64]) -> Model {
    let mut model = Model::new(priors());
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
