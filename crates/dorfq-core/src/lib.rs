//! The contract between DorfQ's scheduler and its forge.
//!
//! The boundary is data in both directions, one call in each: a [`Forge`]
//! produces an [`Observation`], a [`Scheduler`] turns it into a [`Plan`], and
//! the application's driver executes the plan through the forge. The
//! scheduler never calls the forge; the forge never sees the scheduler.
//!
//! Two terms the rules use. The **latest observation** is the one most
//! recently returned by [`Forge::observe`]. A pull request or batch is
//! **known** when the latest observation reported it and no effect performed
//! since has removed it: [`Forge::eject`] removes the pull request,
//! [`Forge::retire`] removes the batch, and a successful [`Forge::land`]
//! removes every member of the batch it landed. A refused landing removes
//! nothing.
//!
//! # Rules
//!
//! 1. **Facts flow one way.** [`Forge::observe`] is the only source of
//!    facts. Effects return `Ok(())`; their consequences appear in the next
//!    observation. A forge keeps a batch in [`Observation::batches`] from
//!    [`Forge::start_batch`] until [`Forge::retire`], whatever its state.
//! 2. **The forge enforces the two landing invariants, whatever the
//!    scheduler asks.** `land(b)` is accepted only for a batch the latest
//!    observation reported as `Testing { ci: Passed }`; anything else is a
//!    scheduler bug (rule 6). The forge then fast-forwards trunk to `b`'s tip
//!    only if `b`'s base equals the current trunk and every member is still
//!    enqueued at the head it was built with; otherwise the batch becomes
//!    [`BatchState::LandRefused`] with the reason, and the call returns
//!    `Ok(())`. Only the exact tree that passed CI on top of the current
//!    trunk ever lands, and nothing lands that its owner has withdrawn. The
//!    forge does not re-check CI at `land`: the tip is immutable and the tree
//!    passed.
//! 3. **Plan items are requests, and the scheduler assumes nothing about
//!    their execution.** The scheduler learns facts only from observations;
//!    it never takes an effect to have happened because it was planned. It
//!    may remember what it has emitted, never what it inferred the forge did
//!    with it. A batch id that never appears in an observation was never
//!    started. Rule 7 makes a partially executed plan harmless: every plan
//!    item is driven by a fact that is still in the next observation if the
//!    item did not run.
//! 4. **The scheduler is deterministic.** [`Scheduler::step`] reads no
//!    clock, no randomness and no I/O; its output is a function of its own
//!    state and the observations it has been given, in order.
//! 5. **Plans are well-formed, and batch ids are the scheduler's.** A
//!    [`BatchStart::id`] must not equal the id of any batch in the latest
//!    observation. [`BatchStart::prs`] is non-empty, without duplicates, and
//!    every member is known; [`BatchStart::base`] is the latest observation's
//!    `trunk` or the tip of one of its batches. Within one plan, `eject` names
//!    each pull request at most once, each of `start`, `land` and `retire`
//!    names a batch id at most once, and no member of a batch that `land`
//!    names is named by `start` or `eject`: a plan does not land a pull
//!    request and also build on it or eject it, whether or not the landing is
//!    refused. The forge answers a violation with [`ForgeError::Permanent`]:
//!    it is a scheduler bug, and the process stops (rule 7).
//! 6. **Effects on unknown things are bugs.** `land` or `retire` on a batch
//!    that is not known; `land` on a batch the latest observation did not
//!    report as `Testing { ci: Passed }`; `eject` on a pull request that is
//!    not known — all [`ForgeError::Permanent`]. "Known" is judged against
//!    the latest observation and the forge's own effects since it, not
//!    against whatever the world has done meanwhile: a pull request whose
//!    owner cleared the intent after the observation is still known, and
//!    `eject` on it succeeds.
//! 7. **The driver executes a plan in a fixed order and stops at the first
//!    error.** Order: `land`, `start`, `eject`, `retire`. Landings come first
//!    so that a refused landing is visible before anything is built on its
//!    tip; `retire` comes last, so a plan cut short leaves the batch, and the
//!    facts that drove the plan, in the next observation. On
//!    [`ForgeError::Transient`] the driver abandons the rest of the plan and
//!    ticks again; on [`ForgeError::Permanent`] it stops the process.
//! 8. **Ordering is canonical.** [`Observation::prs`] is sorted by number and
//!    [`Observation::batches`] by id, so two forges reporting the same facts
//!    produce equal observations.
//! 9. **The forge talks to authors.** The forge tells a pull request's author
//!    about every transition the queue causes or observes on it: entry into
//!    the queue, inclusion in a batch, landing, ejection with the scheduler's
//!    reason, and leaving the queue by neither. The scheduler decides the
//!    ejection reason and nothing else about messaging; nothing about
//!    messages crosses the boundary.
//!
//! Forges enforce rules 5 and 6 through [`ledger::Ledger`], which holds the
//! latest observation and the effects performed since it, so every forge
//! judges knownness by the same code.

pub mod ledger;

use std::error::Error;
use std::fmt;

/// A pull request, by the forge's number for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PrNumber(pub u64);

/// A batch, by an id the scheduler chose (rule 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BatchId(pub u64);

/// A commit id: exactly 40 lowercase hexadecimal digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha(String);

impl Sha {
    /// Parses a commit id, rejecting anything but 40 lowercase hex digits.
    pub fn parse(s: &str) -> Result<Sha, InvalidSha> {
        let valid = s.len() == 40
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Sha(s.to_owned()))
        } else {
            Err(InvalidSha(s.to_owned()))
        }
    }

    /// The commit id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Text that [`Sha::parse`] rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSha(pub String);

impl fmt::Display for InvalidSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid commit id {:?}: expected 40 lowercase hex digits",
            self.0
        )
    }
}

impl Error for InvalidSha {}

/// Everything the forge reports in one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// The current tip of the managed branch.
    pub trunk: Sha,
    /// Every open pull request targeting trunk that carries the intent
    /// marker, sorted by number.
    pub prs: Vec<PrSnapshot>,
    /// Every batch the forge tracks (started, not yet retired), sorted by id.
    pub batches: Vec<BatchStatus>,
}

/// One enqueued pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSnapshot {
    pub number: PrNumber,
    pub head: Sha,
    /// Whether it merges into trunk, as the forge knows it.
    pub mergeable: Mergeable,
    /// The pull request's own required checks on `head`.
    pub head_ci: CiStatus,
}

/// Whether a pull request merges cleanly into trunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeable {
    Clean,
    Conflicting,
    /// The forge has not computed it yet.
    Unknown,
}

/// The state of a set of CI checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiStatus {
    Pending,
    Passed,
    Failed,
}

/// One tracked batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchStatus {
    pub id: BatchId,
    /// The base it was started on, echoed from the [`BatchStart`].
    pub base: Sha,
    /// Its members, in batch order, echoed from the [`BatchStart`].
    pub members: Vec<BatchMember>,
    pub state: BatchState,
}

/// A batch member and the head the batch was built with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchMember {
    pub pr: PrNumber,
    pub head: Sha,
}

/// Where a batch is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchState {
    /// Git refused to merge `pr` while building; there is no tip and no CI.
    Conflict { pr: PrNumber, with: ConflictWith },
    /// Built and pushed; CI pending or decided.
    Testing { tip: Sha, ci: CiStatus },
    /// Trunk was fast-forwarded to `tip`.
    Landed { tip: Sha },
    /// The forge declined to land (rule 2).
    LandRefused { tip: Sha, reason: LandRefusal },
}

/// What a conflicting member conflicted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictWith {
    Trunk,
    /// An earlier member of the same batch.
    Pr(PrNumber),
}

/// Why the forge declined to land a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandRefusal {
    /// The batch's base is no longer trunk.
    TrunkMoved { trunk: Sha },
    /// A member is no longer enqueued at the head the batch was built with.
    MemberGone { pr: PrNumber },
}

/// What the scheduler asks the forge to do in one tick, executed in the
/// order `land`, `start`, `eject`, `retire` (rule 7).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub start: Vec<BatchStart>,
    pub land: Vec<BatchId>,
    pub retire: Vec<BatchId>,
    pub eject: Vec<Ejection>,
}

impl Plan {
    /// True when the plan asks for nothing.
    pub fn is_empty(&self) -> bool {
        self.start.is_empty()
            && self.land.is_empty()
            && self.retire.is_empty()
            && self.eject.is_empty()
    }
}

/// A request to build, push and track a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchStart {
    pub id: BatchId,
    /// Trunk, or the tip of a tracked batch (rule 5).
    pub base: Sha,
    /// The members in merge order; non-empty, no duplicates.
    pub prs: Vec<PrNumber>,
}

/// A request to clear a pull request's intent and tell its author why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ejection {
    pub pr: PrNumber,
    pub reason: EjectReason,
}

/// Why a pull request leaves the queue without landing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EjectReason {
    ConflictsWithTrunk,
    ConflictsWith { pr: PrNumber },
    BatchFailed { batch: BatchId },
}

/// The decision side of the boundary.
pub trait Scheduler {
    /// Turns the latest observation into a plan. Deterministic (rule 4).
    fn step(&mut self, observation: &Observation) -> Plan;
}

/// The fact-and-effect side of the boundary.
pub trait Forge {
    /// Reports the current state of the world (rules 1 and 8).
    fn observe(&mut self) -> Result<Observation, ForgeError>;
    /// Builds, pushes and tracks a batch, and tells its members (rules 5, 9).
    fn start_batch(&mut self, batch: &BatchStart) -> Result<(), ForgeError>;
    /// Lands a batch that passed, or refuses it (rule 2), and tells its
    /// members.
    fn land(&mut self, batch: BatchId) -> Result<(), ForgeError>;
    /// Forgets a batch and drops its branch.
    fn retire(&mut self, batch: BatchId) -> Result<(), ForgeError>;
    /// Clears a pull request's intent and tells its author why.
    fn eject(&mut self, pr: PrNumber, reason: &EjectReason) -> Result<(), ForgeError>;
}

/// A failed forge call.
#[derive(Debug)]
pub enum ForgeError {
    /// Try again next tick.
    Transient(Box<dyn Error + Send + Sync>),
    /// Stop: operator attention, or a scheduler bug.
    Permanent(Box<dyn Error + Send + Sync>),
}

impl fmt::Display for ForgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForgeError::Transient(e) => write!(f, "transient forge error: {e}"),
            ForgeError::Permanent(e) => write!(f, "permanent forge error: {e}"),
        }
    }
}

impl Error for ForgeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ForgeError::Transient(e) | ForgeError::Permanent(e) => Some(e.as_ref()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn sha_accepts_forty_lowercase_hex() {
        let sha = Sha::parse(GOOD).unwrap();
        assert_eq!(sha.as_str(), GOOD);
        assert_eq!(sha.to_string(), GOOD);
    }

    #[test]
    fn sha_rejects_uppercase() {
        let upper = GOOD.to_uppercase();
        assert_eq!(Sha::parse(&upper), Err(InvalidSha(upper.clone())));
    }

    #[test]
    fn sha_rejects_short_and_long() {
        assert!(Sha::parse(&GOOD[..39]).is_err());
        assert!(Sha::parse(&format!("{GOOD}0")).is_err());
        assert!(Sha::parse("").is_err());
    }

    #[test]
    fn sha_rejects_non_hex() {
        let bad = format!("g{}", &GOOD[1..]);
        assert!(Sha::parse(&bad).is_err());
        let multibyte = format!("é{}", &GOOD[2..]);
        assert_eq!(multibyte.len(), 40);
        assert!(Sha::parse(&multibyte).is_err());
    }

    #[test]
    fn invalid_sha_names_the_input() {
        let message = Sha::parse("nope").unwrap_err().to_string();
        assert!(message.contains("\"nope\""), "{message}");
    }

    #[test]
    fn forge_error_displays_kind_and_cause() {
        let transient = ForgeError::Transient("rate limited".into());
        assert!(matches!(transient, ForgeError::Transient(_)));
        assert_eq!(transient.to_string(), "transient forge error: rate limited");
        assert_eq!(transient.source().unwrap().to_string(), "rate limited");

        let permanent = ForgeError::Permanent("unknown batch 3".into());
        assert!(matches!(permanent, ForgeError::Permanent(_)));
        assert_eq!(
            permanent.to_string(),
            "permanent forge error: unknown batch 3"
        );
    }

    #[test]
    fn empty_plan_is_empty() {
        assert!(Plan::default().is_empty());
        let plan = Plan {
            land: vec![BatchId(1)],
            ..Plan::default()
        };
        assert!(!plan.is_empty());
    }
}
