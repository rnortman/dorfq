//! Rules 5 and 6 as a forge enforces them, and rule 8 as it checks itself.
//!
//! A [`Ledger`] holds the latest observation and the effects a forge has
//! performed since it, and answers whether an effect the scheduler asks for
//! is well-formed and names only known things. A forge hands it every
//! observation before returning it, calls the `check_*` method before an
//! effect and the past-tense method after it. A malformed observation is a
//! [`Malformed`] and a failed check a [`Violation`]; both convert to
//! [`ForgeError::Permanent`].

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use crate::{
    BatchId, BatchStart, BatchState, BatchStatus, CiStatus, ConflictWith, Departed, Departure,
    ForgeError, Observation, PrBase, PrNumber, PrSnapshot, PrStatus, Sha,
};

/// The latest observation and the effects performed since it.
#[derive(Debug, Default)]
pub struct Ledger {
    latest: Option<Observation>,
    /// Pull requests `eject` took out of knownness.
    ejected: BTreeSet<PrNumber>,
    /// Pull requests a successful `land` took out of knownness.
    landed: BTreeSet<PrNumber>,
    started: BTreeSet<BatchId>,
    /// Batches `land` was called on, refused or not.
    landing: BTreeSet<BatchId>,
    retired: BTreeSet<BatchId>,
}

/// What [`Ledger::check_land`] proved about a batch it accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landable<'a> {
    /// The tip the latest observation reported passed.
    pub tip: Sha,
    /// The batch as the latest observation reported it.
    pub status: &'a BatchStatus,
}

impl Ledger {
    /// A ledger that has seen no observation: nothing is known.
    pub fn new() -> Ledger {
        Ledger::default()
    }

    /// The latest observation, if there has been one.
    pub fn latest(&self) -> Option<&Observation> {
        self.latest.as_ref()
    }

    /// Checks `observation` against rule 8; if it is well-formed, makes it
    /// the latest and forgets the effects since the previous one. A
    /// malformed observation leaves the ledger as it was.
    pub fn observed(&mut self, observation: &Observation) -> Result<(), Malformed> {
        check_observation(observation)?;
        check_departed(self.latest.as_ref(), observation)?;
        *self = Ledger {
            latest: Some(observation.clone()),
            ..Ledger::default()
        };
        Ok(())
    }

    /// Rule 5 for `start_batch`, the stack clause included: every member
    /// based on a pull request has that pull request as an earlier member or
    /// as a member of a batch on the walk from the new batch's base to trunk.
    /// A batch with no members is a probe of its base and passes.
    pub fn check_start(&self, batch: &BatchStart) -> Result<(), Violation> {
        let id = batch.id;
        if self.observed_batch(id).is_some() || self.started.contains(&id) {
            return Err(Violation::BatchIdInUse { batch: id });
        }
        let mut seen = BTreeSet::new();
        for &pr in &batch.prs {
            if !seen.insert(pr) {
                return Err(Violation::DuplicateMember { batch: id, pr });
            }
            if self.in_landing(pr) {
                return Err(Violation::LandedInPlan {
                    effect: "start_batch",
                    pr,
                });
            }
            if !self.pr_known(pr) {
                return Err(Violation::MemberNotKnown { batch: id, pr });
            }
            if self.observed_pr(pr).map(|p| &p.status) != Some(&PrStatus::Ready) {
                return Err(Violation::MemberNotReady { batch: id, pr });
            }
        }
        let base_known = self.latest.as_ref().is_some_and(|o| {
            o.trunk == batch.base || o.batches.iter().any(|b| b.state.tip() == Some(&batch.base))
        });
        if !base_known {
            return Err(Violation::UnknownBase {
                batch: id,
                base: batch.base.clone(),
            });
        }
        let history = self.history(&batch.base);
        let mut earlier = BTreeSet::new();
        for &pr in &batch.prs {
            if let Some(PrBase::Pr(base)) = self.observed_pr(pr).map(|p| p.base)
                && !earlier.contains(&base)
                && !history.contains(&base)
            {
                return Err(Violation::BaseNotInBatch {
                    batch: id,
                    pr,
                    base,
                });
            }
            earlier.insert(pr);
        }
        Ok(())
    }

    /// The members of every batch on the walk from `base` down to trunk,
    /// following tips to the batches that have them, as the latest
    /// observation reports them.
    fn history(&self, base: &Sha) -> BTreeSet<PrNumber> {
        let mut members = BTreeSet::new();
        let Some(latest) = &self.latest else {
            return members;
        };
        let mut at = base;
        // Rule 8 makes the walk acyclic; the observation was checked.
        while let Some(batch) = latest.batches.iter().find(|b| b.state.tip() == Some(at)) {
            members.extend(batch.members.iter().map(|m| m.pr));
            at = &batch.base;
        }
        members
    }

    /// Records that `start_batch` created `batch`.
    pub fn started(&mut self, batch: BatchId) {
        self.started.insert(batch);
    }

    /// Rules 5 and 6 for `land`: the batch is known, was not already landed
    /// since the latest observation, was reported `Testing { ci: Passed }`,
    /// is not a probe, and holds no ancestor of an enqueued `Atomic` pull request without
    /// holding that pull request too. Returns the passed tip with the batch
    /// as the latest observation reported it.
    pub fn check_land(&self, batch: BatchId) -> Result<Landable<'_>, Violation> {
        if !self.batch_known(batch) {
            return Err(Violation::UnknownBatch {
                effect: "land",
                batch,
            });
        }
        if self.landing.contains(&batch) {
            return Err(Violation::LandedTwice { batch });
        }
        let status = self
            .observed_batch(batch)
            .expect("a known batch is in the latest observation");
        match &status.state {
            BatchState::Testing {
                tip,
                ci: CiStatus::Passed,
            } => {
                if status.members.is_empty() {
                    return Err(Violation::ProbeLanded { batch });
                }
                if let Some(pr) = self.latest.as_ref().and_then(|o| o.split_atomic(status)) {
                    return Err(Violation::SplitsAtomicStack { batch, pr });
                }
                Ok(Landable {
                    tip: tip.clone(),
                    status,
                })
            }
            BatchState::Testing {
                ci: CiStatus::Pending | CiStatus::Failed(_),
                ..
            }
            | BatchState::Conflict { .. }
            | BatchState::Landed { .. }
            | BatchState::LandRefused { .. } => Err(Violation::NotPassed { batch }),
        }
    }

    /// Records that `land` was called on `batch`, whether or not it is
    /// refused: its members may not be started or ejected until the next
    /// observation.
    pub fn landing(&mut self, batch: BatchId) {
        self.landing.insert(batch);
    }

    /// Records that `batch` landed: its members, as the latest observation
    /// reports them, are no longer known.
    ///
    /// # Panics
    ///
    /// If `batch` is not in the latest observation: `landed` follows a
    /// `check_land` that accepted it, so a missing batch is a caller bug.
    pub fn landed(&mut self, batch: BatchId) {
        let members: Vec<PrNumber> = self
            .observed_batch(batch)
            .expect("landed follows a check_land that accepted the batch")
            .members
            .iter()
            .map(|m| m.pr)
            .collect();
        self.landed.extend(members);
    }

    /// Rules 5 and 6 for `retire`: the batch is known, and `land` was not
    /// called on it since the latest observation.
    pub fn check_retire(&self, batch: BatchId) -> Result<(), Violation> {
        if !self.batch_known(batch) {
            return Err(Violation::UnknownBatch {
                effect: "retire",
                batch,
            });
        }
        if self.landing.contains(&batch) {
            return Err(Violation::RetiredInPlan { batch });
        }
        Ok(())
    }

    /// Records that `batch` was retired.
    pub fn retired(&mut self, batch: BatchId) {
        self.retired.insert(batch);
    }

    /// Rules 5 and 6 for `eject`: the pull request is not a member of a
    /// batch landed since the latest observation, and is known.
    pub fn check_eject(&self, pr: PrNumber) -> Result<(), Violation> {
        if self.in_landing(pr) {
            return Err(Violation::LandedInPlan {
                effect: "eject",
                pr,
            });
        }
        if !self.pr_known(pr) {
            return Err(Violation::UnknownPr { pr });
        }
        Ok(())
    }

    /// Records that `pr` was ejected.
    pub fn ejected(&mut self, pr: PrNumber) {
        self.ejected.insert(pr);
    }

    /// The pull requests ejected since the latest observation, by number.
    pub fn ejected_since_latest(&self) -> impl Iterator<Item = PrNumber> + '_ {
        self.ejected.iter().copied()
    }

    /// Whether `pr` is known: the latest observation reported it and no
    /// effect since removed it.
    pub fn pr_known(&self, pr: PrNumber) -> bool {
        self.observed_pr(pr).is_some() && !self.ejected.contains(&pr) && !self.landed.contains(&pr)
    }

    /// The departures a new observation with `prs` and `batches` reports:
    /// every pull request in the latest observation that `prs` lacks, by
    /// number, at its latest head, classified in the contract's order. A
    /// member of a batch `batches` reports `Landed` is `Landed`; one ejected
    /// since the latest observation is `Ejected`; `otherwise` decides the
    /// rest from what the forge knows. Empty before the first observation.
    pub fn departures<E>(
        &self,
        prs: &[PrSnapshot],
        batches: &[BatchStatus],
        mut otherwise: impl FnMut(&PrSnapshot) -> Result<Departed, E>,
    ) -> Result<Vec<Departure>, E> {
        let Some(latest) = &self.latest else {
            return Ok(Vec::new());
        };
        let staying: BTreeSet<PrNumber> = prs.iter().map(|p| p.number).collect();
        let mut departures = Vec::new();
        for gone in latest.prs.iter().filter(|p| !staying.contains(&p.number)) {
            let landed_with = batches.iter().find(|b| {
                matches!(b.state, BatchState::Landed { .. })
                    && b.members.iter().any(|m| m.pr == gone.number)
            });
            let how = match landed_with {
                Some(batch) => Departed::Landed { batch: batch.id },
                None if self.ejected.contains(&gone.number) => Departed::Ejected,
                None => otherwise(gone)?,
            };
            departures.push(Departure::new(gone.number, gone.head.clone(), how));
        }
        Ok(departures)
    }

    fn observed_batch(&self, id: BatchId) -> Option<&BatchStatus> {
        self.latest
            .as_ref()
            .and_then(|o| o.batches.iter().find(|b| b.id == id))
    }

    fn batch_known(&self, id: BatchId) -> bool {
        self.observed_batch(id).is_some() && !self.retired.contains(&id)
    }

    fn observed_pr(&self, pr: PrNumber) -> Option<&PrSnapshot> {
        self.latest
            .as_ref()
            .and_then(|o| o.prs.iter().find(|p| p.number == pr))
    }

    /// True when `pr` is a member, as the latest observation reports it, of
    /// a batch `land` was called on since that observation.
    fn in_landing(&self, pr: PrNumber) -> bool {
        self.landing.iter().any(|&b| {
            self.observed_batch(b)
                .is_some_and(|s| s.members.iter().any(|m| m.pr == pr))
        })
    }
}

/// A call that breaks rule 5 or rule 6: a scheduler bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// `start_batch` with an id the latest observation reports or that was
    /// started since it.
    BatchIdInUse { batch: BatchId },
    /// `start_batch` naming a member twice.
    DuplicateMember { batch: BatchId, pr: PrNumber },
    /// `start_batch` naming a pull request that is not known.
    MemberNotKnown { batch: BatchId, pr: PrNumber },
    /// `start_batch` naming a pull request the latest observation did not
    /// report `Ready`.
    MemberNotReady { batch: BatchId, pr: PrNumber },
    /// `start_batch` or `eject` naming a member of a batch `land` was called
    /// on since the latest observation, whether or not the landing was
    /// refused.
    LandedInPlan { effect: &'static str, pr: PrNumber },
    /// `start_batch` naming `pr`, based on `base`, without `base` as an
    /// earlier member or a member of a batch below the new batch's base.
    BaseNotInBatch {
        batch: BatchId,
        pr: PrNumber,
        base: PrNumber,
    },
    /// `start_batch` on a base that is neither the latest observation's
    /// trunk nor the tip of one of its batches.
    UnknownBase { batch: BatchId, base: Sha },
    /// `land` or `retire` on a batch that is not known.
    UnknownBatch {
        effect: &'static str,
        batch: BatchId,
    },
    /// `land` on a batch the latest observation did not report as passed.
    NotPassed { batch: BatchId },
    /// `land` on a probe: a batch with no members.
    ProbeLanded { batch: BatchId },
    /// `land` on a batch holding an ancestor of the enqueued `Atomic` pull
    /// request `pr` but not `pr` itself.
    SplitsAtomicStack { batch: BatchId, pr: PrNumber },
    /// `land` on the same batch twice after one observation.
    LandedTwice { batch: BatchId },
    /// `eject` on a pull request that is not known.
    UnknownPr { pr: PrNumber },
    /// `retire` on a batch `land` was called on since the latest
    /// observation: a landed batch is reported `Landed` before it is retired.
    RetiredInPlan { batch: BatchId },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::BatchIdInUse { batch } => {
                write!(f, "start_batch: batch id {} is already in use", batch.0)
            }
            Violation::DuplicateMember { batch, pr } => {
                write!(f, "start_batch: batch {} names pr {} twice", batch.0, pr.0)
            }
            Violation::MemberNotKnown { batch, pr } => write!(
                f,
                "start_batch: batch {} names pr {}, which is not known",
                batch.0, pr.0
            ),
            Violation::MemberNotReady { batch, pr } => write!(
                f,
                "start_batch: batch {} names pr {}, which is not ready",
                batch.0, pr.0
            ),
            Violation::LandedInPlan { effect, pr } => write!(
                f,
                "{effect}: pr {} is a member of a batch this plan lands",
                pr.0
            ),
            Violation::BaseNotInBatch { batch, pr, base } => write!(
                f,
                "start_batch: batch {} names pr {} without its base pr {} before it",
                batch.0, pr.0, base.0
            ),
            Violation::UnknownBase { batch, base } => write!(
                f,
                "start_batch: batch {} is based on {base}, which is neither trunk nor a batch tip",
                batch.0
            ),
            Violation::UnknownBatch { effect, batch } => {
                write!(f, "{effect}: batch {} is not known", batch.0)
            }
            Violation::NotPassed { batch } => {
                write!(f, "land: batch {} was not reported as passed", batch.0)
            }
            Violation::ProbeLanded { batch } => {
                write!(f, "land: batch {} is a probe, which never lands", batch.0)
            }
            Violation::SplitsAtomicStack { batch, pr } => write!(
                f,
                "land: batch {} holds an ancestor of atomic pr {} but not pr {}",
                batch.0, pr.0, pr.0
            ),
            Violation::LandedTwice { batch } => {
                write!(f, "land: batch {} was already landed in this plan", batch.0)
            }
            Violation::UnknownPr { pr } => write!(f, "eject: pr {} is not known", pr.0),
            Violation::RetiredInPlan { batch } => {
                write!(f, "retire: batch {} is landed in this plan", batch.0)
            }
        }
    }
}

impl Error for Violation {}

impl From<Violation> for ForgeError {
    fn from(violation: Violation) -> ForgeError {
        ForgeError::Permanent(Box::new(violation))
    }
}

/// An observation that breaks rule 8: a forge bug.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Malformed {
    /// `field` (`prs` or `batches`) is out of order.
    Unsorted { field: &'static str },
    /// `prs` reports a pull request twice.
    DuplicatePr { pr: PrNumber },
    /// `batches` reports a batch twice.
    DuplicateBatch { batch: BatchId },
    /// `pr`'s base is a pull request `prs` does not report.
    DanglingBase { pr: PrNumber, base: PrNumber },
    /// Following bases from `pr` leads back to it.
    BaseCycle { pr: PrNumber },
    /// `pr` is `Waiting` on nothing.
    EmptyWaiting { pr: PrNumber },
    /// `pr` is `Conflicting` with something other than its base.
    ConflictBaseMismatch { pr: PrNumber },
    /// `departed` names `pr` though the previous observation did not report
    /// it, this one does, or names it twice or at another head; or it omits
    /// `pr`, which the previous observation reported and this one does not.
    DepartedMismatch { pr: PrNumber },
    /// A batch's tip is its own base.
    TipIsBase { batch: BatchId },
    /// Two batches report one tip.
    DuplicateTip { batch: BatchId, other: BatchId },
    /// Following bases from `batch` through the batches whose tips they are
    /// never reaches a sha that no batch has as tip.
    BatchBaseCycle { batch: BatchId },
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Malformed::Unsorted { field } => write!(f, "observation: {field} is not sorted"),
            Malformed::DuplicatePr { pr } => {
                write!(f, "observation: pr {} is reported twice", pr.0)
            }
            Malformed::DuplicateBatch { batch } => {
                write!(f, "observation: batch {} is reported twice", batch.0)
            }
            Malformed::DanglingBase { pr, base } => write!(
                f,
                "observation: pr {}'s base pr {} is not reported",
                pr.0, base.0
            ),
            Malformed::BaseCycle { pr } => {
                write!(f, "observation: the bases from pr {} lead back to it", pr.0)
            }
            Malformed::EmptyWaiting { pr } => {
                write!(f, "observation: pr {} is waiting on nothing", pr.0)
            }
            Malformed::ConflictBaseMismatch { pr } => write!(
                f,
                "observation: pr {} conflicts with something other than its base",
                pr.0
            ),
            Malformed::DepartedMismatch { pr } => write!(
                f,
                "observation: departed disagrees with the previous observation on pr {}",
                pr.0
            ),
            Malformed::TipIsBase { batch } => {
                write!(f, "observation: batch {}'s tip is its base", batch.0)
            }
            Malformed::DuplicateTip { batch, other } => write!(
                f,
                "observation: batches {} and {} have one tip",
                other.0, batch.0
            ),
            Malformed::BatchBaseCycle { batch } => write!(
                f,
                "observation: the bases from batch {} lead back to it",
                batch.0
            ),
        }
    }
}

impl Error for Malformed {}

impl From<Malformed> for ForgeError {
    fn from(malformed: Malformed) -> ForgeError {
        ForgeError::Permanent(Box::new(malformed))
    }
}

/// Rule 8's `departed` clause against the observation before it: sorted
/// by number, and exactly `previous.prs` minus `observation.prs`, each at its
/// previous head.
fn check_departed(
    previous: Option<&Observation>,
    observation: &Observation,
) -> Result<(), Malformed> {
    for pair in observation.departed.windows(2) {
        let (a, b) = (pair[0].pr, pair[1].pr);
        if a == b {
            return Err(Malformed::DepartedMismatch { pr: a });
        }
        if a > b {
            return Err(Malformed::Unsorted { field: "departed" });
        }
    }
    let staying: BTreeSet<PrNumber> = observation.prs.iter().map(|p| p.number).collect();
    let mut expected = previous
        .map(|o| &o.prs[..])
        .unwrap_or_default()
        .iter()
        .filter(|p| !staying.contains(&p.number));
    let mut reported = observation.departed.iter();
    loop {
        match (expected.next(), reported.next()) {
            (None, None) => return Ok(()),
            (Some(e), Some(r)) if e.number == r.pr && e.head == r.head => {}
            (Some(e), Some(r)) => {
                return Err(Malformed::DepartedMismatch {
                    pr: e.number.min(r.pr),
                });
            }
            (Some(e), None) => return Err(Malformed::DepartedMismatch { pr: e.number }),
            (None, Some(r)) => return Err(Malformed::DepartedMismatch { pr: r.pr }),
        }
    }
}

/// Rule 8's clauses, first violation first: `prs` by number and `batches`
/// by id, strictly increasing; each pull request's status consistent with
/// its base, and every base reported; the base relation over `prs`
/// acyclic; every tip distinct from its batch's base and from every other
/// tip; the base relation over batches acyclic.
fn check_observation(observation: &Observation) -> Result<(), Malformed> {
    for pair in observation.prs.windows(2) {
        let (a, b) = (pair[0].number, pair[1].number);
        if a == b {
            return Err(Malformed::DuplicatePr { pr: a });
        }
        if a > b {
            return Err(Malformed::Unsorted { field: "prs" });
        }
    }
    let by_number: BTreeMap<PrNumber, &PrSnapshot> =
        observation.prs.iter().map(|p| (p.number, p)).collect();
    for pr in &observation.prs {
        match &pr.status {
            PrStatus::Waiting(holds) if holds.is_empty() => {
                return Err(Malformed::EmptyWaiting { pr: pr.number });
            }
            PrStatus::Conflicting(with) => {
                let consistent = match pr.base {
                    PrBase::Trunk => *with == ConflictWith::Trunk,
                    PrBase::Pr(base) => *with == ConflictWith::Pr(base),
                };
                if !consistent {
                    return Err(Malformed::ConflictBaseMismatch { pr: pr.number });
                }
            }
            PrStatus::Ready | PrStatus::Waiting(_) => {}
        }
        if let PrBase::Pr(base) = pr.base
            && !by_number.contains_key(&base)
        {
            return Err(Malformed::DanglingBase {
                pr: pr.number,
                base,
            });
        }
    }
    for pr in &observation.prs {
        let mut steps = 0;
        let mut at = pr.base;
        while let PrBase::Pr(n) = at {
            if n == pr.number || steps > observation.prs.len() {
                return Err(Malformed::BaseCycle { pr: pr.number });
            }
            steps += 1;
            at = by_number[&n].base;
        }
    }
    for pair in observation.batches.windows(2) {
        let (a, b) = (pair[0].id, pair[1].id);
        if a == b {
            return Err(Malformed::DuplicateBatch { batch: a });
        }
        if a > b {
            return Err(Malformed::Unsorted { field: "batches" });
        }
    }
    let mut by_tip: BTreeMap<&Sha, &BatchStatus> = BTreeMap::new();
    for batch in &observation.batches {
        let Some(tip) = batch.state.tip() else {
            continue;
        };
        if *tip == batch.base {
            return Err(Malformed::TipIsBase { batch: batch.id });
        }
        if let Some(other) = by_tip.insert(tip, batch) {
            return Err(Malformed::DuplicateTip {
                batch: batch.id,
                other: other.id,
            });
        }
    }
    for batch in &observation.batches {
        let mut steps = 0;
        let mut at = by_tip.get(&batch.base);
        while let Some(parent) = at {
            if parent.id == batch.id || steps > observation.batches.len() {
                return Err(Malformed::BatchBaseCycle { batch: batch.id });
            }
            steps += 1;
            at = by_tip.get(&parent.base);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BatchMember, Hold, LandRefusal, StackPolicy, Withdrawal};

    fn sha(n: u64) -> Sha {
        Sha::parse(&format!("{n:040x}")).unwrap()
    }

    fn pr(n: u64) -> PrSnapshot {
        PrSnapshot {
            number: PrNumber(n),
            head: sha(100 + n),
            base: PrBase::Trunk,
            status: PrStatus::Ready,
            stack: StackPolicy::Splittable,
            behind: 0,
        }
    }

    /// Pull request `n` based on pull request `base`, ready.
    fn stacked(n: u64, base: u64) -> PrSnapshot {
        PrSnapshot {
            base: PrBase::Pr(PrNumber(base)),
            ..pr(n)
        }
    }

    fn with_status(snapshot: PrSnapshot, status: PrStatus) -> PrSnapshot {
        PrSnapshot { status, ..snapshot }
    }

    fn waiting(hold: Hold) -> PrStatus {
        PrStatus::Waiting(BTreeSet::from([hold]))
    }

    fn batch(id: u64, members: &[u64], state: BatchState) -> BatchStatus {
        BatchStatus {
            id: BatchId(id),
            base: sha(1),
            members: members
                .iter()
                .map(|&n| BatchMember {
                    pr: PrNumber(n),
                    head: sha(100 + n),
                })
                .collect(),
            state,
        }
    }

    fn passed(tip: u64) -> BatchState {
        BatchState::Testing {
            tip: sha(tip),
            ci: CiStatus::Passed,
        }
    }

    /// Trunk 1; pull requests 1-4 enqueued; batch 1 (members 1, 2) passed
    /// at tip 10; batch 2 (member 3) conflicting; batch 3 (member 4) pending
    /// at tip 30; batch 4 refused at tip 40; batch 5 landed at tip 50.
    fn world() -> Ledger {
        let mut ledger = Ledger::new();
        ledger
            .observed(&Observation {
                now: crate::Timestamp(0),
                trunk: sha(1),
                prs: (1..=4).map(pr).collect(),
                batches: vec![
                    batch(1, &[1, 2], passed(10)),
                    batch(
                        2,
                        &[3],
                        BatchState::Conflict {
                            pr: PrNumber(3),
                            with: crate::ConflictWith::Trunk,
                        },
                    ),
                    batch(
                        3,
                        &[4],
                        BatchState::Testing {
                            tip: sha(30),
                            ci: CiStatus::Pending,
                        },
                    ),
                    batch(
                        4,
                        &[4],
                        BatchState::LandRefused {
                            tip: sha(40),
                            reason: LandRefusal::TrunkMoved { trunk: sha(1) },
                        },
                    ),
                    batch(5, &[], BatchState::Landed { tip: sha(50) }),
                ],
                departed: vec![],
            })
            .unwrap();
        ledger
    }

    fn start(id: u64, base: u64, prs: &[u64]) -> BatchStart {
        BatchStart {
            id: BatchId(id),
            base: sha(base),
            prs: prs.iter().map(|&n| PrNumber(n)).collect(),
        }
    }

    #[test]
    fn nothing_is_known_before_an_observation() {
        let ledger = Ledger::new();
        assert!(ledger.latest().is_none());
        assert_eq!(
            ledger.check_start(&start(1, 1, &[1])),
            Err(Violation::MemberNotKnown {
                batch: BatchId(1),
                pr: PrNumber(1)
            })
        );
        assert_eq!(
            ledger.check_land(BatchId(1)),
            Err(Violation::UnknownBatch {
                effect: "land",
                batch: BatchId(1)
            })
        );
        assert_eq!(
            ledger.check_eject(PrNumber(1)),
            Err(Violation::UnknownPr { pr: PrNumber(1) })
        );
    }

    #[test]
    fn a_well_formed_start_on_trunk_or_a_tip_passes() {
        let ledger = world();
        assert_eq!(ledger.check_start(&start(9, 1, &[1, 2])), Ok(()));
        for tip in [10, 40, 50] {
            assert_eq!(ledger.check_start(&start(9, tip, &[3])), Ok(()), "{tip}");
        }
    }

    #[test]
    fn start_rejects_an_id_observed_or_started_since() {
        let mut ledger = world();
        assert_eq!(
            ledger.check_start(&start(1, 1, &[3])),
            Err(Violation::BatchIdInUse { batch: BatchId(1) })
        );
        ledger.started(BatchId(9));
        assert_eq!(
            ledger.check_start(&start(9, 1, &[3])),
            Err(Violation::BatchIdInUse { batch: BatchId(9) })
        );
    }

    #[test]
    fn start_accepts_a_probe_on_trunk_or_a_tip() {
        let ledger = world();
        for base in [1, 10, 30, 50] {
            assert_eq!(ledger.check_start(&start(9, base, &[])), Ok(()), "{base}");
        }
    }

    #[test]
    fn start_rejects_duplicate_members() {
        let ledger = world();
        assert_eq!(
            ledger.check_start(&start(9, 1, &[1, 2, 1])),
            Err(Violation::DuplicateMember {
                batch: BatchId(9),
                pr: PrNumber(1)
            })
        );
    }

    #[test]
    fn start_rejects_a_member_never_observed_or_ejected_since() {
        let mut ledger = world();
        assert_eq!(
            ledger.check_start(&start(9, 1, &[7])),
            Err(Violation::MemberNotKnown {
                batch: BatchId(9),
                pr: PrNumber(7)
            })
        );
        ledger.ejected(PrNumber(3));
        assert_eq!(
            ledger.check_start(&start(9, 1, &[3])),
            Err(Violation::MemberNotKnown {
                batch: BatchId(9),
                pr: PrNumber(3)
            })
        );
    }

    #[test]
    fn start_and_eject_reject_a_member_of_a_batch_landed_in_this_plan() {
        // Refused: `landing` only.
        let mut refused = world();
        refused.landing(BatchId(1));
        // Succeeded: `landing` then `landed`.
        let mut succeeded = world();
        succeeded.landing(BatchId(1));
        succeeded.landed(BatchId(1));
        for ledger in [&refused, &succeeded] {
            assert_eq!(
                ledger.check_start(&start(9, 1, &[3, 2])),
                Err(Violation::LandedInPlan {
                    effect: "start_batch",
                    pr: PrNumber(2)
                })
            );
            assert_eq!(
                ledger.check_eject(PrNumber(1)),
                Err(Violation::LandedInPlan {
                    effect: "eject",
                    pr: PrNumber(1)
                })
            );
            assert_eq!(ledger.check_eject(PrNumber(3)), Ok(()));
        }
    }

    #[test]
    fn start_rejects_a_member_not_reported_ready() {
        let mut ledger = Ledger::new();
        let prs = vec![
            pr(1),
            with_status(pr(2), waiting(Hold::OwnCiPending)),
            with_status(pr(3), PrStatus::Conflicting(ConflictWith::Trunk)),
        ];
        ledger
            .observed(&Observation {
                prs,
                ..observation(&[], vec![])
            })
            .unwrap();
        assert_eq!(ledger.check_start(&start(9, 1, &[1])), Ok(()));
        for n in [2, 3] {
            assert_eq!(
                ledger.check_start(&start(9, 1, &[1, n])),
                Err(Violation::MemberNotReady {
                    batch: BatchId(9),
                    pr: PrNumber(n)
                }),
                "{n}"
            );
        }
    }

    /// Trunk 1; 1 on trunk, 2 on 1, 3 on 2, 4 on 1, all ready; batch 1
    /// (member 1) passed at tip 10 on trunk; batch 2 (member 2) pending at
    /// tip 20 on tip 10.
    fn stack_world() -> Ledger {
        let mut ledger = Ledger::new();
        let on_ten = BatchStatus {
            base: sha(10),
            ..batch(
                2,
                &[2],
                BatchState::Testing {
                    tip: sha(20),
                    ci: CiStatus::Pending,
                },
            )
        };
        ledger
            .observed(&Observation {
                prs: vec![pr(1), stacked(2, 1), stacked(3, 2), stacked(4, 1)],
                ..observation(&[], vec![batch(1, &[1], passed(10)), on_ten])
            })
            .unwrap();
        ledger
    }

    #[test]
    fn start_accepts_a_child_after_its_parent_or_on_a_batch_holding_it() {
        let ledger = stack_world();
        assert_eq!(ledger.check_start(&start(9, 1, &[1, 2, 3])), Ok(()));
        assert_eq!(ledger.check_start(&start(9, 1, &[1, 4, 2])), Ok(()));
        // The parent is a member of the base batch.
        assert_eq!(ledger.check_start(&start(9, 10, &[2, 4])), Ok(()));
        assert_eq!(ledger.check_start(&start(9, 20, &[3])), Ok(()));
        // The parent is a member of the base batch's base.
        assert_eq!(ledger.check_start(&start(9, 20, &[4])), Ok(()));
    }

    #[test]
    fn start_rejects_a_child_before_or_without_its_parent() {
        let ledger = stack_world();
        let missing = |pr: u64, base: u64| {
            Err(Violation::BaseNotInBatch {
                batch: BatchId(9),
                pr: PrNumber(pr),
                base: PrNumber(base),
            })
        };
        assert_eq!(ledger.check_start(&start(9, 1, &[2, 1])), missing(2, 1));
        assert_eq!(ledger.check_start(&start(9, 1, &[2])), missing(2, 1));
        assert_eq!(ledger.check_start(&start(9, 1, &[1, 3])), missing(3, 2));
        assert_eq!(ledger.check_start(&start(9, 10, &[3])), missing(3, 2));
    }

    /// Trunk 1; 1 on trunk, 2 on 1, 3 on 2 with 3 `Atomic` when `atomic`,
    /// and only 1 and 2 enqueued unless `three`; batch 1 (member 1) passed at
    /// tip 10, batch 2 (members 1, 2) at tip 20, batch 3 (members 1-3) at
    /// tip 30 when `three`.
    fn atomic_world(atomic: bool, three: bool) -> Ledger {
        let mut ledger = Ledger::new();
        let tip = if atomic {
            stacked(3, 2).with_stack(StackPolicy::Atomic)
        } else {
            stacked(3, 2)
        };
        let mut prs = vec![pr(1), stacked(2, 1)];
        let mut batches = vec![batch(1, &[1], passed(10)), batch(2, &[1, 2], passed(20))];
        if three {
            prs.push(tip);
            batches.push(batch(3, &[1, 2, 3], passed(30)));
        }
        ledger
            .observed(&Observation {
                prs,
                ..observation(&[], batches)
            })
            .unwrap();
        ledger
    }

    #[test]
    fn land_rejects_a_batch_that_splits_an_atomic_stack() {
        let ledger = atomic_world(true, true);
        for id in [1, 2] {
            assert_eq!(
                ledger.check_land(BatchId(id)).map(|l| l.tip),
                Err(Violation::SplitsAtomicStack {
                    batch: BatchId(id),
                    pr: PrNumber(3)
                }),
                "{id}"
            );
        }
        assert_eq!(ledger.check_land(BatchId(3)).map(|l| l.tip), Ok(sha(30)));
    }

    #[test]
    fn land_accepts_a_part_of_a_splittable_or_unenqueued_stack() {
        for ledger in [atomic_world(false, true), atomic_world(true, false)] {
            for id in [1, 2] {
                assert!(ledger.check_land(BatchId(id)).is_ok(), "{id}");
            }
        }
    }

    #[test]
    fn only_a_successful_landing_takes_the_members_out_of_knownness() {
        let mut ledger = world();
        ledger.landing(BatchId(1));
        assert!(ledger.pr_known(PrNumber(1)) && ledger.pr_known(PrNumber(2)));
        ledger.landed(BatchId(1));
        assert!(!ledger.pr_known(PrNumber(1)) && !ledger.pr_known(PrNumber(2)));
        assert!(ledger.pr_known(PrNumber(3)));
    }

    #[test]
    fn start_rejects_an_unknown_base() {
        let ledger = world();
        // The tip of a pending batch is a tip; a conflicting batch has none,
        // and a sha nobody reported is no base.
        assert_eq!(ledger.check_start(&start(9, 30, &[1])), Ok(()));
        assert_eq!(
            ledger.check_start(&start(9, 77, &[1])),
            Err(Violation::UnknownBase {
                batch: BatchId(9),
                base: sha(77)
            })
        );
    }

    #[test]
    fn check_land_returns_the_passed_tip_and_the_observed_status() {
        let ledger = world();
        let landable = ledger.check_land(BatchId(1)).unwrap();
        assert_eq!(landable.tip, sha(10));
        assert_eq!(landable.status, &batch(1, &[1, 2], passed(10)));
    }

    #[test]
    fn land_rejects_an_unknown_batch() {
        let mut ledger = world();
        assert_eq!(
            ledger.check_land(BatchId(9)),
            Err(Violation::UnknownBatch {
                effect: "land",
                batch: BatchId(9)
            })
        );
        ledger.retired(BatchId(1));
        assert_eq!(
            ledger.check_land(BatchId(1)),
            Err(Violation::UnknownBatch {
                effect: "land",
                batch: BatchId(1)
            })
        );
    }

    #[test]
    fn land_rejects_a_batch_not_reported_passed() {
        let ledger = world();
        for id in [2, 3, 4, 5] {
            assert_eq!(
                ledger.check_land(BatchId(id)),
                Err(Violation::NotPassed { batch: BatchId(id) }),
                "{id}"
            );
        }
    }

    #[test]
    fn land_rejects_a_passed_probe() {
        let mut ledger = Ledger::new();
        ledger
            .observed(&observation(&[1], vec![on(1, 1, 10)]))
            .unwrap();
        assert_eq!(
            ledger.check_land(BatchId(1)),
            Err(Violation::ProbeLanded { batch: BatchId(1) })
        );
        assert_eq!(ledger.check_retire(BatchId(1)), Ok(()));
    }

    #[test]
    fn land_rejects_a_second_landing() {
        let mut ledger = world();
        ledger.landing(BatchId(1));
        assert_eq!(
            ledger.check_land(BatchId(1)),
            Err(Violation::LandedTwice { batch: BatchId(1) })
        );
    }

    #[test]
    #[should_panic(expected = "landed follows a check_land that accepted the batch")]
    fn landed_on_a_batch_never_observed_is_a_caller_bug() {
        world().landed(BatchId(9));
    }

    #[test]
    fn retire_rejects_an_unknown_batch() {
        let mut ledger = world();
        assert_eq!(ledger.check_retire(BatchId(2)), Ok(()));
        assert_eq!(
            ledger.check_retire(BatchId(9)),
            Err(Violation::UnknownBatch {
                effect: "retire",
                batch: BatchId(9)
            })
        );
        ledger.retired(BatchId(2));
        assert_eq!(
            ledger.check_retire(BatchId(2)),
            Err(Violation::UnknownBatch {
                effect: "retire",
                batch: BatchId(2)
            })
        );
    }

    #[test]
    fn retire_rejects_a_batch_landed_in_this_plan() {
        // Refused or not: the landing is reported before the batch goes.
        let mut refused = world();
        refused.landing(BatchId(1));
        let mut succeeded = world();
        succeeded.landing(BatchId(1));
        succeeded.landed(BatchId(1));
        for ledger in [&refused, &succeeded] {
            assert_eq!(
                ledger.check_retire(BatchId(1)),
                Err(Violation::RetiredInPlan { batch: BatchId(1) })
            );
            assert_eq!(ledger.check_retire(BatchId(2)), Ok(()));
        }
    }

    #[test]
    fn eject_rejects_an_unknown_pr() {
        let mut ledger = world();
        assert_eq!(
            ledger.check_eject(PrNumber(7)),
            Err(Violation::UnknownPr { pr: PrNumber(7) })
        );
        ledger.ejected(PrNumber(4));
        assert_eq!(
            ledger.check_eject(PrNumber(4)),
            Err(Violation::UnknownPr { pr: PrNumber(4) })
        );
    }

    #[test]
    fn observed_clears_the_effects() {
        let mut ledger = world();
        ledger.started(BatchId(9));
        ledger.landing(BatchId(1));
        ledger.landed(BatchId(1));
        ledger.retired(BatchId(2));
        ledger.ejected(PrNumber(3));
        let latest = ledger.latest().unwrap().clone();
        ledger.observed(&latest).unwrap();
        assert_eq!(ledger.latest(), Some(&latest));
        assert_eq!(ledger.check_start(&start(9, 1, &[1, 3])), Ok(()));
        assert!(ledger.check_land(BatchId(1)).is_ok());
        assert_eq!(ledger.check_retire(BatchId(2)), Ok(()));
        assert_eq!(ledger.check_eject(PrNumber(3)), Ok(()));
    }

    #[test]
    fn a_violation_is_permanent_and_keeps_its_text() {
        let error = ForgeError::from(Violation::UnknownPr { pr: PrNumber(9) });
        let ForgeError::Permanent(cause) = &error else {
            panic!("expected a permanent error, got {error:?}");
        };
        assert_eq!(
            cause.downcast_ref::<Violation>(),
            Some(&Violation::UnknownPr { pr: PrNumber(9) })
        );
        assert_eq!(
            error.to_string(),
            "permanent forge error: eject: pr 9 is not known"
        );
    }

    fn observation(prs: &[u64], batches: Vec<BatchStatus>) -> Observation {
        Observation {
            now: crate::Timestamp(0),
            trunk: sha(1),
            prs: prs.iter().map(|&n| pr(n)).collect(),
            batches,
            departed: vec![],
        }
    }

    /// Batch `id` testing at `tip` on `base`.
    fn on(id: u64, base: u64, tip: u64) -> BatchStatus {
        BatchStatus {
            base: sha(base),
            ..batch(id, &[], passed(tip))
        }
    }

    #[test]
    fn observed_accepts_a_well_formed_observation() {
        let mut ledger = Ledger::new();
        // A chain on trunk, a sibling on the chain's first tip, a batch with
        // no tip, and a landed batch whose tip is trunk.
        let o = observation(
            &[1, 2, 5],
            vec![
                on(1, 1, 10),
                on(2, 10, 20),
                on(3, 10, 30),
                batch(
                    4,
                    &[1],
                    BatchState::Conflict {
                        pr: PrNumber(1),
                        with: crate::ConflictWith::Trunk,
                    },
                ),
                BatchStatus {
                    base: sha(2),
                    ..batch(5, &[], BatchState::Landed { tip: sha(1) })
                },
            ],
        );
        assert_eq!(ledger.observed(&o), Ok(()));
        assert_eq!(ledger.latest(), Some(&o));
    }

    #[test]
    fn observed_accepts_a_well_formed_stack() {
        // 1 <- 2 <- 3 and 2 <- 4: a chain and a tree, with a held child and
        // children conflicting with their bases.
        let prs = vec![
            pr(1),
            stacked(2, 1),
            with_status(
                stacked(3, 2),
                waiting(Hold::AncestorMoved { pr: PrNumber(1) }),
            ),
            with_status(
                stacked(4, 2),
                PrStatus::Conflicting(ConflictWith::Pr(PrNumber(2))),
            ),
            with_status(pr(5), PrStatus::Conflicting(ConflictWith::Trunk)),
        ];
        let o = Observation {
            prs,
            ..observation(&[], vec![])
        };
        assert_eq!(Ledger::new().observed(&o), Ok(()));
    }

    #[test]
    fn observed_rejects_each_pull_request_defect_by_name() {
        let with_prs = |prs: Vec<PrSnapshot>| Observation {
            prs,
            ..observation(&[], vec![])
        };
        let cases = [
            (
                with_prs(vec![pr(1), stacked(2, 7)]),
                Malformed::DanglingBase {
                    pr: PrNumber(2),
                    base: PrNumber(7),
                },
            ),
            (
                with_prs(vec![stacked(1, 1)]),
                Malformed::BaseCycle { pr: PrNumber(1) },
            ),
            (
                with_prs(vec![pr(1), stacked(2, 3), stacked(3, 2)]),
                Malformed::BaseCycle { pr: PrNumber(2) },
            ),
            (
                with_prs(vec![stacked(1, 2), stacked(2, 3), stacked(3, 2)]),
                Malformed::BaseCycle { pr: PrNumber(1) },
            ),
            (
                with_prs(vec![with_status(pr(1), PrStatus::Waiting(BTreeSet::new()))]),
                Malformed::EmptyWaiting { pr: PrNumber(1) },
            ),
            (
                with_prs(vec![
                    pr(1),
                    with_status(pr(2), PrStatus::Conflicting(ConflictWith::Pr(PrNumber(1)))),
                ]),
                Malformed::ConflictBaseMismatch { pr: PrNumber(2) },
            ),
            (
                with_prs(vec![
                    pr(1),
                    with_status(stacked(2, 1), PrStatus::Conflicting(ConflictWith::Trunk)),
                ]),
                Malformed::ConflictBaseMismatch { pr: PrNumber(2) },
            ),
            (
                with_prs(vec![
                    pr(1),
                    pr(2),
                    with_status(
                        stacked(3, 1),
                        PrStatus::Conflicting(ConflictWith::Pr(PrNumber(2))),
                    ),
                ]),
                Malformed::ConflictBaseMismatch { pr: PrNumber(3) },
            ),
        ];
        for (o, expected) in cases {
            let mut ledger = world();
            let before = ledger.latest().cloned();
            assert_eq!(ledger.observed(&o), Err(expected.clone()));
            assert_eq!(ledger.latest().cloned(), before, "{expected}");
        }
    }

    #[test]
    fn observed_rejects_each_defect_by_name() {
        let cases = [
            (
                observation(&[2, 1], vec![]),
                Malformed::Unsorted { field: "prs" },
            ),
            (
                observation(&[1, 3, 3], vec![]),
                Malformed::DuplicatePr { pr: PrNumber(3) },
            ),
            (
                observation(&[], vec![on(2, 1, 20), on(1, 1, 10)]),
                Malformed::Unsorted { field: "batches" },
            ),
            (
                observation(&[], vec![on(1, 1, 10), on(1, 1, 20)]),
                Malformed::DuplicateBatch { batch: BatchId(1) },
            ),
            (
                observation(&[], vec![on(1, 1, 10), on(2, 30, 30)]),
                Malformed::TipIsBase { batch: BatchId(2) },
            ),
            (
                observation(&[], vec![on(1, 1, 10), on(2, 1, 10)]),
                Malformed::DuplicateTip {
                    batch: BatchId(2),
                    other: BatchId(1),
                },
            ),
            (
                observation(&[], vec![on(1, 1, 10), on(2, 30, 20), on(3, 20, 30)]),
                Malformed::BatchBaseCycle { batch: BatchId(2) },
            ),
        ];
        for (o, expected) in cases {
            let mut ledger = world();
            let before = ledger.latest().cloned();
            assert_eq!(ledger.observed(&o), Err(expected.clone()));
            assert_eq!(ledger.latest().cloned(), before, "{expected}");
        }
    }

    #[test]
    fn a_batch_leading_into_a_cycle_is_rejected() {
        // Batch 1 is based on batch 2's tip; 2 and 3 are based on each
        // other's tips.
        let o = observation(&[], vec![on(1, 20, 10), on(2, 30, 20), on(3, 20, 30)]);
        assert_eq!(
            Ledger::new().observed(&o),
            Err(Malformed::BatchBaseCycle { batch: BatchId(1) })
        );
    }

    fn departure(n: u64, how: Departed) -> Departure {
        Departure::new(PrNumber(n), sha(100 + n), how)
    }

    fn withdrawn(_: &PrSnapshot) -> Result<Departed, ()> {
        Ok(Departed::Withdrawn(Withdrawal::IntentCleared))
    }

    #[test]
    fn departures_classify_landed_then_ejected_then_the_forge() {
        // Pull requests 1-4 at the latest observation; 1 and 2 landed with
        // batch 1, 3 ejected, 4 gone by itself, 5 new.
        let mut ledger = world();
        ledger.landing(BatchId(1));
        ledger.landed(BatchId(1));
        ledger.ejected(PrNumber(3));
        let batches = vec![batch(1, &[1, 2], BatchState::Landed { tip: sha(10) })];
        let mut asked = Vec::new();
        let departures = ledger
            .departures(&[pr(5)], &batches, |p| {
                asked.push(p.number);
                withdrawn(p)
            })
            .unwrap();
        assert_eq!(
            departures,
            [
                departure(1, Departed::Landed { batch: BatchId(1) }),
                departure(2, Departed::Landed { batch: BatchId(1) }),
                departure(3, Departed::Ejected),
                departure(4, Departed::Withdrawn(Withdrawal::IntentCleared)),
            ]
        );
        assert_eq!(asked, [PrNumber(4)]);
        let staying: Vec<PrSnapshot> = (1..=4).map(pr).collect();
        assert_eq!(ledger.departures(&staying, &batches, withdrawn), Ok(vec![]));
        assert_eq!(
            ledger.departures(&[], &[], |_| Err("no")),
            Err("no"),
            "an error from the forge's classification is returned"
        );
    }

    #[test]
    fn departures_are_empty_before_the_first_observation() {
        assert_eq!(Ledger::new().departures(&[], &[], withdrawn), Ok(vec![]));
    }

    #[test]
    fn ejected_since_latest_lists_ejections_and_not_landings() {
        let mut ledger = world();
        ledger.landing(BatchId(1));
        ledger.landed(BatchId(1));
        ledger.ejected(PrNumber(4));
        ledger.ejected(PrNumber(3));
        let ejected: Vec<PrNumber> = ledger.ejected_since_latest().collect();
        assert_eq!(ejected, [PrNumber(3), PrNumber(4)]);
        assert!(!ledger.pr_known(PrNumber(1)) && !ledger.pr_known(PrNumber(3)));
        let latest = ledger.latest().unwrap().clone();
        ledger.observed(&latest).unwrap();
        assert_eq!(ledger.ejected_since_latest().count(), 0);
    }

    #[test]
    fn observed_accepts_departed_matching_the_previous_observation() {
        let mut ledger = world();
        let next = Observation {
            departed: vec![
                departure(2, Departed::Merged),
                departure(4, Departed::Ejected),
            ],
            ..observation(&[1, 3, 5], vec![])
        };
        assert_eq!(ledger.observed(&next), Ok(()));
    }

    #[test]
    fn observed_rejects_each_departed_defect_by_name() {
        let gone = |n| departure(n, Departed::Merged);
        let with_departed = |prs: &[u64], departed| Observation {
            departed,
            ..observation(prs, vec![])
        };
        let moved_head = Departure::new(PrNumber(4), sha(999), Departed::Merged);
        let cases = [
            // 4 left and is not reported.
            (with_departed(&[1, 2, 3], vec![]), PrNumber(4)),
            // 7 was never reported.
            (with_departed(&[1, 2, 3, 4], vec![gone(7)]), PrNumber(7)),
            // 3 is still enqueued.
            (with_departed(&[1, 2, 3, 4], vec![gone(3)]), PrNumber(3)),
            // 4 left at another head than it had.
            (with_departed(&[1, 2, 3], vec![moved_head]), PrNumber(4)),
            // 4 reported twice.
            (
                with_departed(&[1, 2, 3], vec![gone(4), gone(4)]),
                PrNumber(4),
            ),
        ];
        for (o, pr) in cases {
            let mut ledger = world();
            let before = ledger.latest().cloned();
            assert_eq!(
                ledger.observed(&o),
                Err(Malformed::DepartedMismatch { pr }),
                "{pr:?}"
            );
            assert_eq!(ledger.latest().cloned(), before);
        }
        assert_eq!(
            world().observed(&with_departed(&[1], vec![gone(3), gone(2), gone(4)])),
            Err(Malformed::Unsorted { field: "departed" })
        );
        assert_eq!(
            Ledger::new().observed(&with_departed(&[], vec![gone(1)])),
            Err(Malformed::DepartedMismatch { pr: PrNumber(1) }),
            "a first observation has no departures"
        );
    }

    #[test]
    fn a_malformed_observation_is_permanent_and_keeps_its_text() {
        let error = ForgeError::from(Malformed::DuplicatePr { pr: PrNumber(4) });
        let ForgeError::Permanent(cause) = &error else {
            panic!("expected a permanent error, got {error:?}");
        };
        assert_eq!(
            cause.downcast_ref::<Malformed>(),
            Some(&Malformed::DuplicatePr { pr: PrNumber(4) })
        );
        assert_eq!(
            error.to_string(),
            "permanent forge error: observation: pr 4 is reported twice"
        );
    }
}
