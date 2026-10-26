//! The model behind the Bayesian scheduler.
//!
//! Two kinds of cause can make a run fail. `Bad(i)`: pull request `i` fails
//! against trunk on its own. The model holds one cleared position per queued
//! pull request at its head — the trunk it is known good against: at first
//! sight the trunk its own CI ran on, as the forge's `behind` reports it,
//! moved forward by every alibi and never back, and reset when the head
//! changes. The belief that it is bad at any position is what the landings
//! since its cleared position regrow. `Pair(i, j)`: the two fail only
//! together; materialised at the prior `p_interact` once a failed run
//! exercised both, dropped when either leaves. Beyond those, any run fails
//! for no reason with probability `p_flake`. A pull request and one of its
//! enqueued ancestors are never a pair: the forge reports a stacked pull
//! request ready only when its own CI covered its ancestors.
//!
//! Trunk positions count the pull requests that reached trunk, landed by the
//! queue or merged by another hand. A run exercises `Bad` of every member of
//! its tree (its own members and every ancestor batch's) and every pair of
//! them. A passing run is an alibi: it clears every `Bad` it exercised at the
//! trunk it started on, credited with its own tree's landings since, and
//! sets every pair it exercised to a floor that inference treats as false.
//! A pull request's own CI passing on its head is an alibi for its `Bad`
//! alone, against the trunk that run started on. Each landing since a
//! cleared position may have broken the pull request, with probability
//! `p_interact` per pull request that reached trunk.
//!
//! A failing run is evidence. A failed run is read at the trunk it started
//! on: it blames a pull request only if its cleared position is behind that
//! trunk, and only for the landings between the two, so a landing after the
//! run's start never explains its failure and an alibi later than the run
//! clears the pull request of it. A run leaves the evidence when nothing it
//! could blame is left: every member's cleared position is past the run's,
//! and its pairs are at the floor; or when a member of its tree leaves other
//! than by landing. A member that lands takes only its own causes out of the
//! runs it was in.
//!
//! Posteriors are exact under this model: a pull request's chance of being
//! bad is split into independent stretches of trunk at the positions of the
//! failed runs that blame it, the evidence splits into components of runs
//! linked by a shared cause, and each component is solved by
//! inclusion–exclusion over its subsets of runs. A batch's chance of passing
//! is the posterior probability that none of the causes its run exercises
//! holds, given every failed run, from the same sums. That sum cannot be
//! resolved when a component is explained only by many low-belief causes and
//! flakes, and inference then stops the process rather than return noise.

mod chain;
mod infer;
mod model;
mod policy;
#[cfg(test)]
mod test_support;
mod tree;

use std::collections::{BTreeMap, BTreeSet};

use dorfq_core::{BatchId, PrNumber, Timestamp};

use crate::{BayesConfig, Ids, Member};
use model::Model;
pub(crate) use model::{FLOOR, Priors};

/// The Bayesian scheduler: beliefs about causes, updated exactly from every
/// verdict, and batches chosen by expected value.
///
/// Under `speculation = "chain"` it keeps one chain of batches built to
/// land, each on the previous one's tip, and runs culprit finders on trunk
/// beside it: small batches started for their verdict. Under `"tree"` every
/// standing batch's tip is a node beside trunk, and each free lane takes the
/// best batch on any node, weighted by the chance it lands: its node becomes
/// trunk and every older batch on the node fails; a batch whose node dies,
/// or whose chance of landing falls to zero, is cancelled. A pull request is
/// ejected only when a failed run singles it out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Bayes {
    params: BayesConfig,
    ids: Ids,
    model: Model,
    /// Culprit finders started and still tracked; every other tracked batch,
    /// started here or not, is a chain batch. Empty under the tree.
    finders: BTreeSet<BatchId>,
    /// Each tracked batch's tree members, taken when it was first observed.
    trees: BTreeMap<BatchId, Vec<Member>>,
    /// For each tracked batch, the pull requests that reached trunk outside
    /// its tree since it started: landings its run did not test.
    untested: BTreeMap<BatchId, u32>,
    /// How many pull requests the scheduler has seen reach trunk.
    position: i64,
    /// Batches whose verdict has been taken in.
    read: BTreeSet<BatchId>,
    /// When each queued pull request's number first appeared, kept across
    /// re-pushes.
    since: BTreeMap<PrNumber, Timestamp>,
    /// Each queued pull request's value this step: `1 + w_age·h` for the
    /// `h` hours since it appeared.
    values: BTreeMap<PrNumber, f64>,
}

impl Bayes {
    pub(crate) fn new(params: BayesConfig) -> Bayes {
        Bayes {
            params,
            ids: Ids::new(),
            model: Model::new(params.priors()),
            finders: BTreeSet::new(),
            trees: BTreeMap::new(),
            untested: BTreeMap::new(),
            position: 0,
            read: BTreeSet::new(),
            since: BTreeMap::new(),
            values: BTreeMap::new(),
        }
    }
}
