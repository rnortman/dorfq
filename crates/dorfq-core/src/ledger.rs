//! Rules 5 and 6 as a forge enforces them.
//!
//! A [`Ledger`] holds the latest observation and the effects a forge has
//! performed since it, and answers whether an effect the scheduler asks for
//! is well-formed and names only known things. A forge calls the `check_*`
//! method before an effect and the past-tense method after it; a failed
//! check is a [`Violation`], which converts to [`ForgeError::Permanent`].

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use crate::{
    BatchId, BatchStart, BatchState, BatchStatus, CiStatus, ForgeError, Observation, PrNumber, Sha,
};

/// The latest observation and the effects performed since it.
#[derive(Debug, Default)]
pub struct Ledger {
    latest: Option<Observation>,
    /// Pull requests an effect took out of knownness: ejected, or landed.
    removed: BTreeSet<PrNumber>,
    started: BTreeSet<BatchId>,
    /// Batches `land` was called on, refused or not.
    landing: BTreeSet<BatchId>,
    retired: BTreeSet<BatchId>,
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

    /// Makes `observation` the latest and forgets the effects since the
    /// previous one.
    pub fn observed(&mut self, observation: &Observation) {
        *self = Ledger {
            latest: Some(observation.clone()),
            ..Ledger::default()
        };
    }

    /// Rule 5 for `start_batch`.
    pub fn check_start(&self, batch: &BatchStart) -> Result<(), Violation> {
        let id = batch.id;
        if self.observed_batch(id).is_some() || self.started.contains(&id) {
            return Err(Violation::BatchIdInUse { batch: id });
        }
        if batch.prs.is_empty() {
            return Err(Violation::EmptyBatch { batch: id });
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
        }
        let base_known = self.latest.as_ref().is_some_and(|o| {
            o.trunk == batch.base
                || o.batches.iter().any(|b| match &b.state {
                    BatchState::Testing { tip, .. }
                    | BatchState::Landed { tip }
                    | BatchState::LandRefused { tip, .. } => *tip == batch.base,
                    BatchState::Conflict { .. } => false,
                })
        });
        if !base_known {
            return Err(Violation::UnknownBase {
                batch: id,
                base: batch.base.clone(),
            });
        }
        Ok(())
    }

    /// Records that `start_batch` created `batch`.
    pub fn started(&mut self, batch: BatchId) {
        self.started.insert(batch);
    }

    /// Rules 5 and 6 for `land`: the batch is known, was not already landed
    /// since the latest observation, and was reported `Testing { ci: Passed }`.
    /// Returns the batch as the latest observation reported it.
    // TODO(quality-check-land-returns-status-callers-re-match): return the
    // tip the `Testing { ci: Passed }` match proved, so callers need no
    // second match and no `unreachable!`.
    pub fn check_land(&self, batch: BatchId) -> Result<&BatchStatus, Violation> {
        if !self.batch_known(batch) {
            return Err(Violation::UnknownBatch {
                effect: "land",
                batch,
            });
        }
        if self.landing.contains(&batch) {
            return Err(Violation::LandedTwice { batch });
        }
        let observed = self
            .observed_batch(batch)
            .expect("a known batch is in the latest observation");
        match observed.state {
            BatchState::Testing {
                ci: CiStatus::Passed,
                ..
            } => Ok(observed),
            _ => Err(Violation::NotPassed { batch }),
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
        self.removed.extend(members);
    }

    /// Rule 6 for `retire`: the batch is known.
    pub fn check_retire(&self, batch: BatchId) -> Result<(), Violation> {
        if self.batch_known(batch) {
            Ok(())
        } else {
            Err(Violation::UnknownBatch {
                effect: "retire",
                batch,
            })
        }
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
        self.removed.insert(pr);
    }

    fn observed_batch(&self, id: BatchId) -> Option<&BatchStatus> {
        self.latest
            .as_ref()
            .and_then(|o| o.batches.iter().find(|b| b.id == id))
    }

    fn batch_known(&self, id: BatchId) -> bool {
        self.observed_batch(id).is_some() && !self.retired.contains(&id)
    }

    fn pr_known(&self, pr: PrNumber) -> bool {
        let observed = self
            .latest
            .as_ref()
            .is_some_and(|o| o.prs.iter().any(|p| p.number == pr));
        observed && !self.removed.contains(&pr)
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
    /// `start_batch` with no members.
    EmptyBatch { batch: BatchId },
    /// `start_batch` naming a member twice.
    DuplicateMember { batch: BatchId, pr: PrNumber },
    /// `start_batch` naming a pull request that is not known.
    MemberNotKnown { batch: BatchId, pr: PrNumber },
    /// `start_batch` or `eject` naming a member of a batch `land` was called
    /// on since the latest observation, whether or not the landing was
    /// refused.
    LandedInPlan { effect: &'static str, pr: PrNumber },
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
    /// `land` on the same batch twice after one observation.
    LandedTwice { batch: BatchId },
    /// `eject` on a pull request that is not known.
    UnknownPr { pr: PrNumber },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::BatchIdInUse { batch } => {
                write!(f, "start_batch: batch id {} is already in use", batch.0)
            }
            Violation::EmptyBatch { batch } => {
                write!(f, "start_batch: batch {} has no members", batch.0)
            }
            Violation::DuplicateMember { batch, pr } => {
                write!(f, "start_batch: batch {} names pr {} twice", batch.0, pr.0)
            }
            Violation::MemberNotKnown { batch, pr } => write!(
                f,
                "start_batch: batch {} names pr {}, which is not known",
                batch.0, pr.0
            ),
            Violation::LandedInPlan { effect, pr } => write!(
                f,
                "{effect}: pr {} is a member of a batch this plan lands",
                pr.0
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
            Violation::LandedTwice { batch } => {
                write!(f, "land: batch {} was already landed in this plan", batch.0)
            }
            Violation::UnknownPr { pr } => write!(f, "eject: pr {} is not known", pr.0),
        }
    }
}

impl Error for Violation {}

impl From<Violation> for ForgeError {
    fn from(violation: Violation) -> ForgeError {
        ForgeError::Permanent(Box::new(violation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BatchMember, LandRefusal, Mergeable, PrSnapshot};

    fn sha(n: u64) -> Sha {
        Sha::parse(&format!("{n:040x}")).unwrap()
    }

    fn pr(n: u64) -> PrSnapshot {
        PrSnapshot {
            number: PrNumber(n),
            head: sha(100 + n),
            mergeable: Mergeable::Clean,
            head_ci: CiStatus::Passed,
        }
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
        ledger.observed(&Observation {
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
        });
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
    fn start_rejects_empty_and_duplicate_members() {
        let ledger = world();
        assert_eq!(
            ledger.check_start(&start(9, 1, &[])),
            Err(Violation::EmptyBatch { batch: BatchId(9) })
        );
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
    fn check_land_returns_the_observed_status() {
        let ledger = world();
        let status = ledger.check_land(BatchId(1)).unwrap();
        assert_eq!(status, &batch(1, &[1, 2], passed(10)));
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
        ledger.observed(&latest);
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
}
