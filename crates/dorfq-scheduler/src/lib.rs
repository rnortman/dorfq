//! DorfQ's schedulers: implementations of [`dorfq_core::Scheduler`].
//!
//! - [`AllAtOnce`] is a deliberately stupid stub: it batches every ready
//!   pull request at once, blames every member when the batch fails, and
//!   runs one batch at a time. It is the floor every other policy must clear.
//! - [`Bisect`] is the non-learning baseline: a pipelined chain of fresh
//!   batches in queue order, a failed batch's halves as the chain's next
//!   entries, and a memory of member sets known to fail.
//! - `Bayes` (`kind = "bayes"`) is the first real scheduler: a belief per
//!   queued pull request, beliefs for pairs that failed together and a flake
//!   rate, updated exactly from every run's verdict; batches chosen by
//!   expected value against a run's cost, and a pull request ejected only
//!   when a failed run singles it out. Its `speculation` flag chooses
//!   between one chain of batches with culprit finders on trunk beside it
//!   (`chain`) and a tree of futures that scores batches on every standing
//!   tip and beside every pending batch by the chance they land (`tree`).
//!
//! All three batch stacked pull requests, every member after its base, and
//! keep an `Atomic` pull request with its ancestors:
//!
//! - `AllAtOnce` batches every batchable stack whole.
//! - `Bisect` grows batches from landing units (an `Atomic` pull request
//!   with its ancestors, or one other pull request), splits a failed batch
//!   only between units, and, when a single `Atomic` unit fails on its own,
//!   ejects its `Atomic` members and retests the ancestors.
//! - `Bayes` grows batches from the same units, never treats a child and its
//!   base as a pair cause (the child's own CI tested them together), and runs
//!   a stacked suspect's batch with every enqueued ancestor not beneath it.
//!
//! [`SchedulerConfig`] is the configuration both the application and the
//! simulator parse to choose and build one of them.

use std::collections::{BTreeMap, BTreeSet};

use dorfq_core::{
    BatchId, BatchState, BatchStatus, CiStatus, EjectReason, Ejection, Observation, PrBase,
    PrNumber, PrSnapshot, PrStatus, Sha, StackPolicy,
};

mod all_at_once;
mod bayes;
mod bisect;
mod config;
mod stacks;
#[cfg(test)]
mod test_support;

pub use all_at_once::AllAtOnce;
pub use bisect::Bisect;
pub use config::{BayesConfig, ConfigError, SchedulerConfig, Speculation};

/// A pull request at a head: re-pushing it makes a different member.
type Member = (PrNumber, Sha);

/// Collects ejections, emitting each pull request at most once, only if the
/// observation still lists it, and never for a member of a batch being
/// landed.
pub(crate) struct Ejections<'a> {
    present: BTreeSet<PrNumber>,
    settled: BTreeSet<PrNumber>,
    out: &'a mut Vec<Ejection>,
}

impl<'a> Ejections<'a> {
    /// A collector over the pull requests `observation` lists, writing to
    /// `out`.
    pub(crate) fn new(observation: &Observation, out: &'a mut Vec<Ejection>) -> Ejections<'a> {
        Ejections {
            present: observation.prs.iter().map(|p| p.number).collect(),
            settled: BTreeSet::new(),
            out,
        }
    }

    /// Ejects `pr` for `reason`; true when the ejection was emitted.
    pub(crate) fn eject(&mut self, pr: PrNumber, reason: EjectReason) -> bool {
        let emitted = self.present.contains(&pr) && self.settled.insert(pr);
        if emitted {
            self.out.push(Ejection::new(pr, reason));
        }
        emitted
    }

    /// Marks every member of `batch` as leaving through its landing, so
    /// nothing ejects them.
    pub(crate) fn landing(&mut self, batch: &BatchStatus) {
        self.settled.extend(batch.members.iter().map(|m| m.pr));
    }

    /// Ejects every pull request `observation` reports `Conflicting`, with
    /// what it conflicts with as the reason.
    pub(crate) fn eject_conflicting(&mut self, observation: &Observation) {
        for pr in &observation.prs {
            match &pr.status {
                PrStatus::Conflicting(with) => {
                    self.eject(pr.number, EjectReason::Conflicts { with: *with });
                }
                PrStatus::Ready | PrStatus::Waiting(_) => {}
            }
        }
    }
}

/// Whether `pr` may join a batch: the forge reports it `Ready`.
fn is_ready(pr: &PrSnapshot) -> bool {
    matches!(pr.status, PrStatus::Ready)
}

/// The pull requests a batch may take: of the `Ready` ones, those whose
/// every ancestor is taken too, less every ancestor of an enqueued `Atomic`
/// pull request that is not taken, repeated until nothing changes. Ordered
/// so that every base precedes its children, ties by number.
pub(crate) fn batchable(observation: &Observation) -> Vec<PrNumber> {
    let base = bases(&observation.prs);
    let mut set: BTreeSet<PrNumber> = observation
        .prs
        .iter()
        .filter(|p| is_ready(p))
        .map(|p| p.number)
        .collect();
    loop {
        let held_back: BTreeSet<PrNumber> = observation
            .prs
            .iter()
            .filter(|p| p.stack == StackPolicy::Atomic && !set.contains(&p.number))
            .flat_map(|p| ancestors(&base, p.base))
            .collect();
        let kept: BTreeSet<PrNumber> = set
            .iter()
            .copied()
            .filter(|n| {
                !held_back.contains(n)
                    && match base[n] {
                        PrBase::Trunk => true,
                        PrBase::Pr(base) => set.contains(&base),
                    }
            })
            .collect();
        if kept == set {
            break;
        }
        set = kept;
    }
    let mut order = Vec::with_capacity(set.len());
    let mut placed: BTreeSet<PrNumber> = BTreeSet::new();
    let mut open: BTreeSet<PrNumber> = set
        .iter()
        .copied()
        .filter(|n| base[n] == PrBase::Trunk)
        .collect();
    while let Some(next) = open.pop_first() {
        order.push(next);
        placed.insert(next);
        open.extend(
            set.iter()
                .copied()
                .filter(|n| base[n] == PrBase::Pr(next) && !placed.contains(n)),
        );
    }
    order
}

/// The base of each of `prs`, keyed by number.
pub(crate) fn bases(prs: &[PrSnapshot]) -> BTreeMap<PrNumber, PrBase> {
    prs.iter().map(|p| (p.number, p.base)).collect()
}

/// The ancestors of a pull request whose base is `from`, nearest first, by
/// `base`. The observation's base relation is closed and acyclic (rule 8).
pub(crate) fn ancestors(
    base: &BTreeMap<PrNumber, PrBase>,
    from: PrBase,
) -> impl Iterator<Item = PrNumber> + '_ {
    let mut at = from;
    std::iter::from_fn(move || match at {
        PrBase::Trunk => None,
        PrBase::Pr(n) => {
            at = base[&n];
            Some(n)
        }
    })
}

/// Batch ids for a scheduler to start batches under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ids {
    next: u64,
}

impl Ids {
    pub(crate) fn new() -> Ids {
        Ids { next: 1 }
    }

    /// Raises the next id above every batch id `observation` reports, so a
    /// scheduler never reuses an id a forge still tracks.
    pub(crate) fn raise(&mut self, observation: &Observation) {
        if let Some(max) = observation.batches.iter().map(|b| b.id.0).max() {
            self.next = self.next.max(max + 1);
        }
    }

    /// A fresh id.
    pub(crate) fn mint(&mut self) -> BatchId {
        let id = BatchId(self.next);
        self.next += 1;
        id
    }
}

/// Whether a batch is still running CI.
// TODO(chain-collapse-under-runner-queue): a `Pending` batch may be running or
// still waiting for a runner; both schedulers bound chain depth by `lanes` over this.
pub(crate) fn is_pending(state: &BatchState) -> bool {
    matches!(
        state,
        BatchState::Testing {
            ci: CiStatus::Pending,
            ..
        }
    )
}

/// The chain a new batch builds on once a plan's landings run.
///
/// `after` is trunk once the landings run. Of `batches`, in id order, a
/// `Testing` batch built on `after` or on the tip of one already standing
/// stands; the end is the newest standing tip, else `after`. Returns the end
/// and the standing batches from the end down to `after`, newest first.
pub(crate) fn chain<'a>(
    after: &'a Sha,
    batches: impl IntoIterator<Item = &'a BatchStatus>,
) -> (&'a Sha, Vec<&'a BatchStatus>) {
    let mut standing: BTreeMap<&Sha, &BatchStatus> = BTreeMap::new();
    let mut end = after;
    for batch in batches {
        if let BatchState::Testing { tip, .. } = &batch.state
            && (batch.base == *after || standing.contains_key(&batch.base))
        {
            standing.insert(tip, batch);
            end = tip;
        }
    }
    let mut path = Vec::new();
    let mut node = end;
    while let Some(&batch) = standing.get(node) {
        path.push(batch);
        node = &batch.base;
    }
    (end, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use dorfq_core::{CiStatus, Hold};

    fn all(observation: &Observation) -> Vec<u64> {
        batchable(observation).into_iter().map(|n| n.0).collect()
    }

    #[test]
    fn batchable_puts_every_base_before_its_children() {
        // 5 <- 2 <- 4, 5 <- 3, and 1 on trunk.
        let prs = vec![
            ready(1),
            stacked(2, 5),
            stacked(3, 5),
            stacked(4, 2),
            ready(5),
        ];
        assert_eq!(all(&observation(prs, vec![])), [1, 5, 2, 3, 4]);
    }

    #[test]
    fn batchable_drops_the_children_of_a_waiting_parent() {
        let prs = vec![
            held(1, Hold::OwnCiPending),
            stacked(2, 1),
            stacked(3, 2),
            ready(4),
        ];
        assert_eq!(all(&observation(prs, vec![])), [4]);
    }

    #[test]
    fn batchable_holds_back_the_ancestors_of_a_waiting_atomic_tip() {
        let mut tip = atomic(held(3, Hold::Protection));
        tip.base = PrBase::Pr(PrNumber(2));
        // 4 on 1 is not an ancestor of 3 but loses its base with 1.
        let prs = vec![ready(1), stacked(2, 1), tip, stacked(4, 1), ready(5)];
        assert_eq!(all(&observation(prs, vec![])), [5]);
    }

    #[test]
    fn batchable_releases_the_ancestors_once_the_atomic_tip_is_ready_or_gone() {
        let prs = vec![ready(1), stacked(2, 1), atomic(stacked(3, 2))];
        assert_eq!(all(&observation(prs, vec![])), [1, 2, 3]);
        let prs = vec![ready(1), stacked(2, 1)];
        assert_eq!(all(&observation(prs, vec![])), [1, 2]);
    }

    #[test]
    fn chain_walks_through_probes_on_trunk_and_on_a_tracked_tip() {
        let batches = [
            testing(1, trunk(), &[], CiStatus::Pending),
            testing(2, tip(1), &[3], CiStatus::Pending),
            testing(3, tip(2), &[], CiStatus::Passed),
        ];
        let trunk = trunk();
        let (end, path) = chain(&trunk, &batches);
        assert_eq!(end, &tip(3));
        let ids: Vec<u64> = path.iter().map(|b| b.id.0).collect();
        assert_eq!(ids, [3, 2, 1]);
    }
}
