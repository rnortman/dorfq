//! Builders for hand-made observations in scheduler tests.

use std::collections::BTreeSet;

use dorfq_core::ledger::{Ledger, Violation};
use dorfq_core::{
    BatchId, BatchMember, BatchStart, BatchState, BatchStatus, CiStatus, ConflictWith, Departed,
    Departure, EjectReason, Ejection, Failure, Hold, Observation, Plan, PrBase, PrNumber,
    PrSnapshot, PrStatus, Scheduler, Sha, StackPolicy, Timestamp,
};

pub(crate) fn sha(n: u64) -> Sha {
    Sha::parse(&format!("{n:040x}")).unwrap()
}

pub(crate) fn trunk() -> Sha {
    sha(1000)
}

/// The tip a test gives batch `id`.
pub(crate) fn tip(id: u64) -> Sha {
    sha(2000 + id)
}

/// The staleness every builder below reports, unless it says otherwise:
/// the landings since the pull request's own CI started.
pub(crate) const BEHIND: u32 = 25;

/// A pull request on trunk at its original head, with `status`, at
/// [`BEHIND`].
pub(crate) fn pr(number: u64, status: PrStatus) -> PrSnapshot {
    PrSnapshot::new(PrNumber(number), sha(number), PrBase::Trunk, status, BEHIND)
}

pub(crate) fn ready(number: u64) -> PrSnapshot {
    pr(number, PrStatus::Ready)
}

/// `pr`, reporting `behind` landings since its own CI started.
pub(crate) fn stale(mut pr: PrSnapshot, behind: u32) -> PrSnapshot {
    pr.behind = behind;
    pr
}

/// A ready pull request whose own CI ran against the current trunk.
pub(crate) fn fresh(number: u64) -> PrSnapshot {
    stale(ready(number), 0)
}

/// A pull request the forge holds for `hold`.
pub(crate) fn held(number: u64, hold: Hold) -> PrSnapshot {
    pr(number, PrStatus::Waiting(BTreeSet::from([hold])))
}

/// A pull request that conflicts with trunk.
pub(crate) fn conflicting(number: u64) -> PrSnapshot {
    pr(number, PrStatus::Conflicting(ConflictWith::Trunk))
}

/// A ready pull request based on pull request `base`.
pub(crate) fn stacked(number: u64, base: u64) -> PrSnapshot {
    let mut pr = ready(number);
    pr.base = PrBase::Pr(PrNumber(base));
    pr
}

/// `pr`, flagged `Atomic`.
pub(crate) fn atomic(pr: PrSnapshot) -> PrSnapshot {
    pr.with_stack(StackPolicy::Atomic)
}

/// A ready `Atomic` pull request based on pull request `base`.
pub(crate) fn atomic_stacked(number: u64, base: u64) -> PrSnapshot {
    atomic(stacked(number, base))
}

pub(crate) fn ready_all(numbers: &[u64]) -> Vec<PrSnapshot> {
    numbers.iter().map(|&n| ready(n)).collect()
}

/// [`ready_all`], after `landings` more landings than [`BEHIND`]: what a
/// forge reports once trunk has moved under them.
pub(crate) fn ready_after(numbers: &[u64], landings: u32) -> Vec<PrSnapshot> {
    numbers
        .iter()
        .map(|&n| stale(ready(n), BEHIND + landings))
        .collect()
}

/// A pull request re-pushed to a head no batch was built with.
pub(crate) fn repushed(number: u64) -> PrSnapshot {
    let mut pr = ready(number);
    pr.head = sha(number + 500);
    pr
}

/// A batch with members at their original heads.
pub(crate) fn batch(id: u64, base: Sha, members: &[u64], state: BatchState) -> BatchStatus {
    BatchStatus::new(
        BatchId(id),
        base,
        members
            .iter()
            .map(|&n| BatchMember::new(PrNumber(n), sha(n)))
            .collect(),
        state,
    )
}

/// A `Testing` batch whose tip is [`tip`] of its id.
pub(crate) fn testing(id: u64, base: Sha, members: &[u64], ci: CiStatus) -> BatchStatus {
    batch(id, base, members, BatchState::Testing { tip: tip(id), ci })
}

pub(crate) fn observation(prs: Vec<PrSnapshot>, batches: Vec<BatchStatus>) -> Observation {
    Observation::new(Timestamp(0), trunk(), 0, prs, batches, Vec::new())
}

/// A failed CI verdict with no per-check detail.
pub(crate) fn ci_failed() -> CiStatus {
    CiStatus::Failed(Failure::new(BTreeSet::new()))
}

/// Pull request `pr`, at its original head, departed with batch `id`.
pub(crate) fn landed(pr: u64, id: u64) -> Departure {
    Departure::new(
        PrNumber(pr),
        sha(pr),
        Departed::Landed { batch: BatchId(id) },
    )
}

pub(crate) fn ejection(pr: u64, reason: EjectReason) -> Ejection {
    Ejection::new(PrNumber(pr), reason)
}

pub(crate) fn numbers(ns: &[u64]) -> Vec<PrNumber> {
    ns.iter().copied().map(PrNumber).collect()
}

/// A start of batch `id` on `base` holding `members`.
pub(crate) fn start(id: u64, base: Sha, members: &[u64]) -> BatchStart {
    BatchStart::new(BatchId(id), base, numbers(members))
}

pub(crate) fn failed_by(id: u64) -> EjectReason {
    EjectReason::BatchFailed { batch: BatchId(id) }
}

pub(crate) fn conflicts_with_trunk() -> EjectReason {
    EjectReason::Conflicts {
        with: ConflictWith::Trunk,
    }
}

pub(crate) fn conflicts_with(pr: u64) -> EjectReason {
    EjectReason::Conflicts {
        with: ConflictWith::Pr(PrNumber(pr)),
    }
}

/// Steps `scheduler` and checks the plan: a ledger handed `observation`
/// accepts every item in rule 7's order, as a forge's would (rules 5 and
/// 6); at most one new batch has no members, a probe, and it is based on
/// trunk or an observed tip; and no pull request is in two new batches, or
/// in a new batch and a tracked one this plan does not retire.
pub(crate) fn step(scheduler: &mut impl Scheduler, observation: &Observation) -> Plan {
    let plan = scheduler
        .step(observation)
        .expect("the scheduler produces a plan");
    let mut ledger = Ledger::new();
    // A fresh ledger has no previous observation for `departed` to match.
    let mut first = observation.clone();
    first.departed.clear();
    ledger
        .observed(&first)
        .unwrap_or_else(|e| panic!("malformed test observation: {e}"));
    let violation = |e: Violation| -> ! { panic!("{e}: {plan:?}") };
    for &b in &plan.land {
        ledger.check_land(b).unwrap_or_else(|e| violation(e));
        ledger.landing(b);
    }
    for start in &plan.start {
        ledger.check_start(start).unwrap_or_else(|e| violation(e));
        ledger.started(start.id);
    }
    for e in &plan.eject {
        ledger.check_eject(e.pr).unwrap_or_else(|e| violation(e));
        ledger.ejected(e.pr);
    }
    for &b in &plan.retire {
        ledger.check_retire(b).unwrap_or_else(|e| violation(e));
        ledger.retired(b);
    }

    let mut seen: BTreeSet<PrNumber> = BTreeSet::new();
    for batch in &observation.batches {
        if !plan.retire.contains(&batch.id) {
            seen.extend(batch.members.iter().map(|m| m.pr));
        }
    }
    let probes: Vec<&BatchStart> = plan.start.iter().filter(|s| s.prs.is_empty()).collect();
    assert!(probes.len() <= 1, "more than one probe: {plan:?}");
    for probe in probes {
        assert!(
            probe.base == observation.trunk
                || observation
                    .batches
                    .iter()
                    .any(|b| b.state.tip() == Some(&probe.base)),
            "a probe on neither trunk nor a tip: {plan:?}"
        );
    }
    for start in &plan.start {
        for pr in &start.prs {
            assert!(seen.insert(*pr), "{pr:?} started twice: {plan:?}");
        }
    }
    plan
}
