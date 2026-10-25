//! The model behind the Bayesian scheduler.
//!
//! Two kinds of cause can make a run fail. `Bad(i)`: pull request `i` fails
//! against trunk on its own; one belief per queued pull request at its head,
//! starting at the prior `p_bad`, reset when the head changes. `Pair(i, j)`:
//! the two fail only together; materialised at the prior `p_interact` once a
//! failed run exercised both, dropped when either leaves. Beyond those, any
//! run fails for no reason with probability `p_flake`. A pull request and
//! one of its enqueued ancestors are never a pair: the forge reports a
//! stacked pull request ready only when its own CI covered its ancestors.
//!
//! A run exercises `Bad` of every member of its tree (its own members and
//! every ancestor batch's) and every pair of them. A passing run is an
//! alibi: every cause it exercised drops to a floor that inference treats
//! as false. A failing run is evidence. When a batch lands, every other
//! queued pull request's `Bad` belief regrows, so an old alibi fades: under
//! `landing = "decay"` by one `p_interact` per pull request the queue
//! landed; under `"transfer"` by its pair belief with each pull request
//! that reached trunk, which is the prior for a pair no run touched, the
//! floor for an alibied pair and nothing for a stack relative.
//! A run leaves the evidence when nothing it could blame is left above the
//! floor, or when a member of its tree leaves other than by landing. A
//! member that lands takes only its own causes out of the runs it was in.
//!
//! Posteriors are exact under this model: the evidence splits into
//! components of runs linked by a shared cause, and each component is
//! solved by inclusion–exclusion over its subsets of runs. That sum cannot
//! be resolved when a component is explained only by many low-belief causes
//! and flakes, and inference then stops the process rather than return
//! noise.

mod chain;
mod infer;
mod model;
mod policy;
#[cfg(test)]
mod test_support;
mod tree;

use std::collections::{BTreeMap, BTreeSet};

use dorfq_core::BatchId;

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
    /// Batches whose verdict has been taken in.
    read: BTreeSet<BatchId>,
    /// Landed batches whose decay has been applied.
    landings: BTreeSet<BatchId>,
}

impl Bayes {
    pub(crate) fn new(params: BayesConfig) -> Bayes {
        Bayes {
            params,
            ids: Ids::new(),
            model: Model::new(params.priors(), params.landing),
            finders: BTreeSet::new(),
            trees: BTreeMap::new(),
            read: BTreeSet::new(),
            landings: BTreeSet::new(),
        }
    }
}
