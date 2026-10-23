//! Builders for hand-made observations in scheduler tests.

use std::collections::BTreeSet;

use dorfq_core::{
    BatchId, BatchMember, BatchStart, BatchState, BatchStatus, CiStatus, EjectReason, Ejection,
    Mergeable, Observation, Plan, PrNumber, PrSnapshot, Scheduler, Sha,
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

pub(crate) fn pr(number: u64, mergeable: Mergeable, head_ci: CiStatus) -> PrSnapshot {
    PrSnapshot {
        number: PrNumber(number),
        head: sha(number),
        mergeable,
        head_ci,
    }
}

pub(crate) fn ready(number: u64) -> PrSnapshot {
    pr(number, Mergeable::Clean, CiStatus::Passed)
}

pub(crate) fn ready_all(numbers: &[u64]) -> Vec<PrSnapshot> {
    numbers.iter().map(|&n| ready(n)).collect()
}

/// A pull request re-pushed to a head no batch was built with.
pub(crate) fn repushed(number: u64) -> PrSnapshot {
    PrSnapshot {
        head: sha(number + 500),
        ..ready(number)
    }
}

/// A batch with members at their original heads.
pub(crate) fn batch(id: u64, base: Sha, members: &[u64], state: BatchState) -> BatchStatus {
    BatchStatus {
        id: BatchId(id),
        base,
        members: members
            .iter()
            .map(|&n| BatchMember {
                pr: PrNumber(n),
                head: sha(n),
            })
            .collect(),
        state,
    }
}

/// A `Testing` batch whose tip is [`tip`] of its id.
pub(crate) fn testing(id: u64, base: Sha, members: &[u64], ci: CiStatus) -> BatchStatus {
    batch(id, base, members, BatchState::Testing { tip: tip(id), ci })
}

pub(crate) fn observation(prs: Vec<PrSnapshot>, batches: Vec<BatchStatus>) -> Observation {
    Observation {
        trunk: trunk(),
        prs,
        batches,
    }
}

pub(crate) fn ejection(pr: u64, reason: EjectReason) -> Ejection {
    Ejection {
        pr: PrNumber(pr),
        reason,
    }
}

pub(crate) fn numbers(ns: &[u64]) -> Vec<PrNumber> {
    ns.iter().copied().map(PrNumber).collect()
}

/// A start of batch `id` on `base` holding `members`.
pub(crate) fn start(id: u64, base: Sha, members: &[u64]) -> BatchStart {
    BatchStart {
        id: BatchId(id),
        base,
        prs: numbers(members),
    }
}

pub(crate) fn failed_by(id: u64) -> EjectReason {
    EjectReason::BatchFailed { batch: BatchId(id) }
}

/// Steps `scheduler` and checks the plan against rule 5 and the
/// observation: a new batch has an id the observation does not track,
/// non-empty distinct queued members and a base that is trunk or a tracked
/// tip; `start`, `land` and `retire` name a batch at most once and `eject` a
/// pull request at most once; no member of a landing batch is started or
/// ejected; and no pull request is in two new batches, or in a new batch and
/// a tracked one this plan does not retire.
pub(crate) fn step(scheduler: &mut impl Scheduler, observation: &Observation) -> Plan {
    let plan = scheduler.step(observation);
    let tracked: BTreeSet<BatchId> = observation.batches.iter().map(|b| b.id).collect();
    let queued: BTreeSet<PrNumber> = observation.prs.iter().map(|p| p.number).collect();
    let tips: BTreeSet<&Sha> = observation
        .batches
        .iter()
        .filter_map(|b| match &b.state {
            BatchState::Testing { tip, .. }
            | BatchState::Landed { tip }
            | BatchState::LandRefused { tip, .. } => Some(tip),
            BatchState::Conflict { .. } => None,
        })
        .collect();
    let distinct = |ids: Vec<BatchId>| ids.len() == ids.iter().collect::<BTreeSet<_>>().len();
    assert!(
        distinct(plan.start.iter().map(|s| s.id).collect()),
        "{plan:?}"
    );
    assert!(distinct(plan.land.clone()), "{plan:?}");
    assert!(distinct(plan.retire.clone()), "{plan:?}");
    let ejected: BTreeSet<PrNumber> = plan.eject.iter().map(|e| e.pr).collect();
    assert_eq!(ejected.len(), plan.eject.len(), "{plan:?}");
    let landing: BTreeSet<PrNumber> = observation
        .batches
        .iter()
        .filter(|b| plan.land.contains(&b.id))
        .flat_map(|b| b.members.iter().map(|m| m.pr))
        .collect();
    assert!(landing.is_disjoint(&ejected), "{plan:?}");

    let mut seen: BTreeSet<PrNumber> = BTreeSet::new();
    for batch in &observation.batches {
        if !plan.retire.contains(&batch.id) {
            seen.extend(batch.members.iter().map(|m| m.pr));
        }
    }
    for start in &plan.start {
        assert!(
            !tracked.contains(&start.id),
            "{:?} in use: {plan:?}",
            start.id
        );
        assert!(!start.prs.is_empty(), "{plan:?}");
        assert!(
            start.base == observation.trunk || tips.contains(&start.base),
            "{:?} on an unknown base: {plan:?}",
            start.id
        );
        for pr in &start.prs {
            assert!(queued.contains(pr), "{pr:?} not queued: {plan:?}");
            assert!(!landing.contains(pr), "{pr:?} is landing: {plan:?}");
            assert!(seen.insert(*pr), "{pr:?} started twice: {plan:?}");
        }
    }
    plan
}
