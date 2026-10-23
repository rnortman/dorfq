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
//!   when a failed run singles it out.
//!
//! [`SchedulerConfig`] is the configuration both the application and the
//! simulator parse to choose and build one of them.

use std::collections::{BTreeMap, BTreeSet};

use dorfq_core::{
    BatchId, BatchState, BatchStatus, CiStatus, ConflictWith, EjectReason, Ejection, Mergeable,
    Observation, PrNumber, PrSnapshot, Sha,
};

mod all_at_once;
mod bayes;
mod bisect;
mod config;
#[cfg(test)]
mod test_support;

pub use all_at_once::AllAtOnce;
pub use bisect::Bisect;
pub use config::{BayesConfig, ConfigError, SchedulerConfig};

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
            self.out.push(Ejection { pr, reason });
        }
        emitted
    }

    /// Marks every member of `batch` as leaving through its landing, so
    /// nothing ejects them.
    pub(crate) fn landing(&mut self, batch: &BatchStatus) {
        self.settled.extend(batch.members.iter().map(|m| m.pr));
    }

    /// Ejects every pull request `observation` reports `Conflicting` with
    /// trunk.
    pub(crate) fn eject_conflicting(&mut self, observation: &Observation) {
        for pr in &observation.prs {
            if pr.mergeable == Mergeable::Conflicting {
                self.eject(pr.number, EjectReason::ConflictsWithTrunk);
            }
        }
    }
}

/// Whether `pr` may join a batch: clean against trunk and its own CI passed.
pub(crate) fn is_ready(pr: &PrSnapshot) -> bool {
    pr.mergeable == Mergeable::Clean && pr.head_ci == CiStatus::Passed
}

/// The ejection reason for the member a `Conflict` batch names.
pub(crate) fn conflict_reason(with: ConflictWith) -> EjectReason {
    match with {
        ConflictWith::Trunk => EjectReason::ConflictsWithTrunk,
        ConflictWith::Pr(pr) => EjectReason::ConflictsWith { pr },
    }
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
