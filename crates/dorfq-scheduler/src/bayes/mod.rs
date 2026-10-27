//! The model behind the Bayesian scheduler.
//!
//! Three kinds of cause can make a run fail. `Bad(i)`: pull request `i` fails
//! against trunk on its own. The model holds, per queued pull request at its
//! head, an origin — the trunk its own CI ran on at first sight, as the
//! forge's `behind` reports it, reset when the head changes — and the
//! alibis read since. Its belief is over windows of trunk: the origin window,
//! its own content, which its own CI missed at `p_miss` if broken, at the
//! prior `p_interact`; and the landings since the origin, each of which may
//! have broken it with probability `p_interact`. `Pair(i, j)`: the two fail
//! only together; materialised at the prior `p_interact` once a failed run
//! exercised both, dropped when either leaves. `Trunk(o)`: trunk, or CI,
//! broke after passing run `o` started, failing every run started after it
//! and before the next pass; below. Beyond those, any run fails for no
//! reason with probability `p_flake`. A pull request and one of its
//! enqueued ancestors are never a pair: the forge reports a stacked pull
//! request ready only when its own CI covered its ancestors.
//!
//! Trunk positions are the forge's count of the pull requests that reached
//! trunk, landed by the queue or by any other hand. A run exercises `Bad` of
//! every member of its tree (its own members and every ancestor batch's) and
//! every pair of them. CI misses a real break at `p_miss`, so a passing run
//! is evidence, not proof: an alibi at the trunk it started on, credited
//! with its own tree's landings since, that lowers the odds of every window
//! it covers by `p_miss`; and each materialised pair it exercised is lowered
//! the same way, no cause once below a floor. A pull request's own CI passing
//! on its head on a later trunk is one more alibi, for its `Bad` alone.
//!
//! A failing run is evidence. A failed run is read at the trunk it started
//! on: it blames a pull request whose origin is at or behind that trunk, for
//! its own content and the landings between the two, so a landing after the
//! run's start never explains its failure. A run leaves the evidence when
//! nothing it could blame is left: every member has landed and its pairs
//! are below the floor; or when a member of its tree leaves other than by
//! landing. A member that lands takes only its own causes out of the runs it
//! was in.
//!
//! A break of trunk is transient: someone repairs it, and the queue sees
//! only runs passing again. So trunk is a cause per stretch: every passing
//! run whose tree stood on trunk opens one, and a failed run blames the
//! stretch it started in, the latest such pass that started before it.
//! A stretch starts at the floor `p_trunk`, the chance trunk or CI breaks
//! for a reason the queue did not test, and is raised by each landing read
//! while it is current, at the chance the landed batch was bad given every
//! verdict read when it was landed, and by each change to trunk by another
//! hand, at `p_trunk` once more. Failures between two passes share their
//! stretch's cause, so they fall to trunk rather than to whoever was in
//! them; a later pass leaves the explanation of the earlier failures alone.
//! A run that blames nothing but a stretch a later pass closed leaves the
//! evidence, and of those that blame nothing but the current stretch only
//! as many are kept as inference reads.
//!
//! Once trunk is believed broken, a probe — a batch with no members, on
//! trunk — is the candidate that tests it: its chance of passing is the
//! current stretch's alone, and a failure that would single that stretch out
//! is worth every queued pull request's value, all of which a broken trunk
//! holds up. One runs at a time, waited for and retired on its verdict,
//! or cancelled once trunk moves past its base, never landed and never
//! built on; a pass on trunk opens a fresh stretch
//! and the queue resumes. A pull request whose failures are capped by its
//! stretch's trunk, one too fresh to outweigh `p_trunk`, is found across a
//! probe, where its failures on both sides are explained by its own cause
//! once and by trunk only as two stretches together.
//!
//! Posteriors are exact under this model, over the runs inference reads: a
//! pull request's chance of being bad is split into independent windows of
//! trunk at its origin, its alibis and the positions of the failed runs that
//! blame it, the evidence splits into components of runs linked by a shared
//! cause, and each component is solved by inclusion–exclusion over its
//! subsets of runs. A batch's chance of passing is the posterior probability
//! that none of the causes its run exercises holds, given every failed run,
//! from the same sums, the current stretch's trunk among them. A sum
//! explained only by many low-belief causes and flakes cannot be resolved;
//! it is read over fewer of the component's newest runs until it can.

mod chain;
mod infer;
mod model;
mod policy;
#[cfg(test)]
mod test_support;
mod tree;

use std::collections::{BTreeMap, BTreeSet};

use dorfq_core::{BatchId, PrNumber, Sha, Timestamp};

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
/// ejected only when a failed run singles it out. A pull request whose cause
/// a failed run singles out at `notify_above` is told so, once per head, and
/// stays; at `eject_above` it is ejected.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Bayes {
    params: BayesConfig,
    ids: Ids,
    model: Model,
    /// Culprit finders and probes started and still tracked; every other
    /// tracked batch with members, started here or not, is a chain batch.
    /// Empty under the tree.
    finders: BTreeSet<BatchId>,
    /// Every batch in the latest observation: its tree and the trunk its
    /// run tested.
    tracked: BTreeMap<BatchId, Tracked>,
    /// The latest observation's trunk and position: every batch the plan
    /// made from it starts on that trunk or on a tracked tip.
    last_trunk: Option<(Sha, i64)>,
    /// Batches whose verdict has been taken in.
    read: BTreeSet<BatchId>,
    /// When each queued pull request's number first appeared, kept across
    /// re-pushes.
    since: BTreeMap<PrNumber, Timestamp>,
    /// Each queued pull request's value this step: `1 + w_age·h` for the
    /// `h` hours since it appeared.
    values: BTreeMap<PrNumber, f64>,
    /// Each batch a plan landed, with the chance it was bad, given every
    /// verdict read by then: credited to the current stretch once an
    /// observation reports it landed.
    landing_breaks: BTreeMap<BatchId, f64>,
    /// Each materialised pair's posterior as of the latest step, so that
    /// when one half reaches trunk the belief it earned is carried onto the
    /// other at that position.
    pair_posteriors: BTreeMap<model::Cause, f64>,
    /// The head each queued pull request was last told about.
    notified: BTreeMap<PrNumber, Sha>,
}

/// A tracked batch: what its run exercises and the trunk it tested.
#[derive(Debug, Clone, PartialEq)]
struct Tracked {
    /// Its tree members still queued, taken when it was first observed.
    tree: Vec<Member>,
    /// The trunk position its run tests: the position its tree stands on,
    /// credited with every member of its tree that has reached trunk since.
    covered: i64,
}

impl Bayes {
    pub(crate) fn new(params: BayesConfig) -> Bayes {
        Bayes {
            params,
            ids: Ids::new(),
            model: Model::new(params.priors()),
            finders: BTreeSet::new(),
            tracked: BTreeMap::new(),
            last_trunk: None,
            read: BTreeSet::new(),
            since: BTreeMap::new(),
            values: BTreeMap::new(),
            landing_breaks: BTreeMap::new(),
            pair_posteriors: BTreeMap::new(),
            notified: BTreeMap::new(),
        }
    }
}
