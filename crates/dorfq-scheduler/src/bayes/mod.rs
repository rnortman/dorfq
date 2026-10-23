//! The model behind the Bayesian scheduler.
//!
//! Two kinds of cause can make a run fail. `Bad(i)`: pull request `i` fails
//! against trunk on its own; one belief per queued pull request at its head,
//! starting at the prior `p_bad`, reset when the head changes. `Pair(i, j)`:
//! the two fail only together; materialised at the prior `p_interact` once a
//! failed run exercised both, dropped when either leaves. Beyond those, any
//! run fails for no reason with probability `p_flake`.
//!
//! A run exercises `Bad` of every member of its tree (its own members and
//! every ancestor batch's) and every pair of them. A passing run is an
//! alibi: every cause it exercised drops to a floor that inference treats
//! as false. A failing run is evidence. When a batch lands, every other
//! queued pull request's `Bad` belief regrows by one chance per landed pull
//! request of being broken by what trunk now holds, so an old alibi fades.
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

mod infer;
mod model;
mod policy;
#[cfg(test)]
mod test_support;

use std::collections::{BTreeMap, BTreeSet};

use dorfq_core::BatchId;

use crate::{BayesConfig, Ids, Member};
use model::Model;
pub(crate) use model::{FLOOR, Priors};

/// The Bayesian scheduler: beliefs about causes, updated exactly from every
/// verdict, and batches chosen by expected value.
///
/// It keeps one chain of batches built to land, each on the previous one's
/// tip, and runs culprit finders on trunk beside it: small batches started
/// for their verdict. A pull request is ejected only when a failed run
/// singles it out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Bayes {
    params: BayesConfig,
    ids: Ids,
    model: Model,
    /// Culprit finders started and still tracked; every other tracked batch,
    /// started here or not, is a chain batch.
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
            model: Model::new(params.priors()),
            finders: BTreeSet::new(),
            trees: BTreeMap::new(),
            read: BTreeSet::new(),
            landings: BTreeSet::new(),
        }
    }
}
