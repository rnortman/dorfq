//! The contract between DorfQ's scheduler and its forge.
//!
//! The boundary is data in both directions, one call in each: a [`Forge`]
//! produces an [`Observation`], a [`Scheduler`] turns it into a [`Plan`], and
//! the application's driver executes the plan through the forge. The
//! scheduler never calls the forge; the forge never sees the scheduler. The
//! forge reports facts and owns admission (whether a pull request may land,
//! [`PrStatus`]); the scheduler decides what to test, land and eject.
//!
//! Two terms the rules use. The **latest observation** is the one most
//! recently returned by [`Forge::observe`]. A pull request or batch is
//! **known** when the latest observation reported it and no effect performed
//! since has removed it: [`Forge::eject`] removes the pull request,
//! [`Forge::retire`] removes the batch, and a successful [`Forge::land`]
//! removes every member of the batch it landed. A refused landing removes
//! nothing, and so does [`Forge::notify`].
//!
//! # Rules
//!
//! Each rule closes with what enforces it.
//!
//! 1. **Facts flow one way.** [`Forge::observe`] is the only source of
//!    facts. Effects return `Ok(())`; their consequences appear in the next
//!    observation. A forge keeps a batch in [`Observation::batches`] from
//!    [`Forge::start_batch`] until [`Forge::retire`], whatever its state.
//!    What trunk a pull request's own CI covered is the forge's to report,
//!    through [`Observation::trunk_position`] and [`PrSnapshot::behind`]:
//!    the position grows with every change to trunk whether or not the
//!    queue made it, and a scheduler keeps no count of its own — the
//!    observation is its only fact about the trunk changes it did not make.
//!    *Enforced by* the types (an effect returns `Result<(), ForgeError>`)
//!    and each forge's tests, which observe a batch after every effect
//!    until it is retired.
//! 2. **The forge enforces the landing invariants, whatever the scheduler
//!    asks.** `land(b)` is accepted only for a batch with members that the
//!    latest observation reported as `Testing { ci: Passed }`; a probe never
//!    lands (rule 6). The forge fast-forwards trunk to `b`'s tip only if
//!    `b`'s base is the current trunk and every member is still enqueued at
//!    the head it was built with; otherwise the batch becomes
//!    [`BatchState::LandRefused`] with the reason, and the call returns
//!    `Ok(())`. The forge re-checks neither CI nor admission at `land`:
//!    admission is judged at the latest observation (`Ready`) and again at
//!    `start` (rule 5), and the window from the latest observation to `land`
//!    is one tick. One exception: a forge that reports the repository's own
//!    verdict on a head (a draft, protection unsatisfied) refuses to land a
//!    member the latest observation reported held by that verdict,
//!    [`LandRefusal::MemberHeld`], because landing it would bypass the rule
//!    the repository configured; a forge that reports no such verdict never
//!    refuses on it. Only the exact tree that passed CI on top of the current
//!    trunk ever lands. *Enforced by* [`ledger::Ledger::check_land`] and each
//!    forge's refusal tests.
//! 3. **Plan items are requests.** The scheduler learns facts only from
//!    observations; it may remember what it has emitted, never what it
//!    inferred the forge did with it. A batch id that never appears in an
//!    observation was never started. Rule 7 makes a partially executed plan
//!    harmless only because of this: every plan item is driven by a fact
//!    that is still in the next observation if the item did not run.
//!    *Enforced by* review: [`Scheduler::step`] sees only the observation,
//!    so the only way to break this is to remember a plan as if it ran.
//!    A notice has no echo: no observation reports that an author was told.
//!    The scheduler remembers the (pull request, head) it has notified and
//!    does not repeat it, so a plan cut short before `notify` (rule 7) loses
//!    that head's notice. *Enforced by* review.
//! 4. **The scheduler is deterministic.** [`Scheduler::step`] reads no
//!    clock, no randomness and no I/O; its output is a function of its own
//!    state and the observations it has been given, in order. Time is
//!    [`Observation::now`] and nothing else. *Enforced by* each policy's
//!    `equal_inputs_give_equal_plans` test and the simulator's seeded
//!    replications.
//! 5. **Plans are well-formed, and batch ids are the scheduler's.** A
//!    [`BatchStart::id`] must not equal the id of any batch in the latest
//!    observation. [`BatchStart::prs`] has no duplicates, and every member
//!    is known and reported [`PrStatus::Ready`] by the latest observation.
//!    There may be no members: a **probe**, a batch that tests its base's
//!    tree alone on a tip of its own (rule 8). [`BatchStart::base`] is the
//!    latest observation's `trunk` or the tip of one of its batches.
//!    **Stacks:** every member whose base is `PrBase::Pr(a)` has `a` as an
//!    earlier member of the same batch or as a member of a batch on the walk
//!    from the new batch's base down to trunk, through tracked batches' tips
//!    and bases. **Atomic:** `land` on a batch that holds an ancestor of an
//!    enqueued [`StackPolicy::Atomic`] pull request but not that pull
//!    request itself is a violation; testing the ancestor alone is not.
//!    Within one plan, `eject` names each pull request at most once, each of
//!    `start`, `land` and `retire` names a batch id at most once, `land` and
//!    `retire` do not name the same batch (a landed batch is reported
//!    `Landed` before it is retired, which [`Observation::departed`] relies
//!    on), and no member of a batch that `land` names is named by `start` or
//!    `eject`, whether or not the landing is refused. `notify` names each
//!    pull request at most once, each known by the latest observation; no
//!    pull request is named by both `notify` and `eject`, and none is a
//!    member of a batch `land` names. A violation is
//!    [`ForgeError::Permanent`]: a scheduler bug, and the process stops
//!    (rule 7). *Enforced by* the ledger's `check_start`, `check_land`,
//!    `check_retire`, `check_notify` and `check_eject`.
//! 6. **Effects on unknown things are bugs.** `land` or `retire` on a batch
//!    that is not known; `land` on a batch the latest observation did not
//!    report as `Testing { ci: Passed }`, or on a probe; `eject` on a pull
//!    request that is not known; `notify` on a pull request that is not
//!    known — all [`ForgeError::Permanent`]. "Known" is
//!    judged against the latest observation and the forge's own effects
//!    since it, not against whatever the world has done meanwhile: a pull
//!    request whose owner cleared the intent after the observation is still
//!    known, and `eject` on it succeeds. *Enforced by* the same ledger
//!    checks.
//! 7. **The driver executes a plan in a fixed order and stops at the first
//!    error.** Order: `land`, `notify`, `start`, `eject`, `retire`. Landings
//!    come first so that a refused landing is visible before anything is
//!    built on its tip, and so that a notice is never sent to a member of a
//!    batch that landed; notices come next because they are the one item
//!    nothing re-drives (rule 3) and the forge can deliver them without a
//!    write to anything but the author's thread; `retire` comes last, so a
//!    plan cut short leaves the batch, and the facts that drove the plan, in
//!    the next observation. On
//!    [`ForgeError::Transient`] the driver abandons the rest of the plan and
//!    ticks again; on [`ForgeError::Permanent`] or a [`SchedulerError`] it
//!    stops the process. `retire` on a `Testing { ci: Pending }` batch is its
//!    cancellation: the forge drops the batch and, where the platform
//!    allows, its run. *Enforced by* the driver's tests and the simulator
//!    loop's tests; the ledger enforces the order's consequence for `notify`
//!    and `eject`: `check_notify` sees the landings, `check_eject` sees the
//!    notices.
//! 8. **Observations are well-formed and canonical.** [`Observation::prs`]
//!    is sorted by number and [`Observation::batches`] by id, each without
//!    duplicates, so two forges reporting the same facts produce equal
//!    observations. `prs` is closed under [`PrSnapshot::base`] and the base
//!    relation over it is acyclic; a [`PrStatus::Waiting`] set is non-empty;
//!    a [`PrStatus::Conflicting`] pull request conflicts with its base
//!    (`ConflictWith::Trunk` on `PrBase::Trunk`, `ConflictWith::Pr(b)` on
//!    `PrBase::Pr(b)`). [`Observation::departed`] is sorted by number and
//!    names exactly the pull requests the previous observation reported and
//!    this one does not, each at the head the previous observation gave it;
//!    it is empty when there is no previous observation.
//!    [`Observation::trunk_position`] is never below the previous
//!    observation's. **Tips:** a batch
//!    that has a tip (`Testing`, `Landed`, `LandRefused`) has one that is
//!    not its own base and that no other batch has, and following bases from
//!    a batch through the batches whose tips they are always ends at a sha
//!    no batch has as tip: every walk over batches terminates, a probe's
//!    included. A forge whose observation breaks this reports
//!    [`ForgeError::Permanent`]. *Enforced by* [`ledger::Ledger::observed`].
//! 9. **Nothing about messages crosses the boundary.** The forge tells
//!    authors about the transitions its own documentation names, in its own
//!    words; which transitions, and whether any, is each forge's choice. The
//!    scheduler contributes an [`EjectReason`] and a [`NoticeReason`], each
//!    worded through its `Display`, and nothing else. *Enforced by* the
//!    types: no message type exists here.
//!
//! Forges enforce rules 2, 5, 6 and 8 through [`ledger::Ledger`], which
//! holds the latest observation and the effects performed since it, so
//! every forge judges knownness by the same code and a forge bug stops at
//! the forge.
//!
//! # Changing the contract
//!
//! Every record struct but [`Plan`] is `#[non_exhaustive]` and is built
//! through its `new` and `with_` methods outside this crate; [`Plan`] is
//! built from `Plan::default()` by pushes or functional update. The
//! identifier newtypes ([`PrNumber`], [`BatchId`], [`Timestamp`],
//! [`CheckName`]) are built directly, and [`Sha`] through [`Sha::parse`].
//! An enum a consumer must act on ([`PrStatus`], [`BatchState`],
//! [`CiStatus`], [`Departed`], [`PrBase`], [`StackPolicy`],
//! [`NoticeReason`]) is exhaustive;
//! one a consumer may ignore, because the forge renders it through
//! `Display` and nobody's behaviour depends on its variant ([`Hold`],
//! [`ConflictWith`], [`LandRefusal`], [`EjectReason`], [`Withdrawal`]), is
//! `#[non_exhaustive]`. A change is one of three kinds.
//!
//! **Additive: one pull request touching this crate only.** A new optional
//! fact (an `Option<T>` for "the forge may not know", or a value with a
//! neutral default the forge defines, like [`StackPolicy::Splittable`]): a
//! field plus a `with_` method. A new variant of an open enum, with its
//! `Display` arm. A new method on an existing type. Each side picks it up at
//! its next rebase with no edits. A default that would stand in for a fact
//! the forge should have supplied is not neutral and is not allowed: such a
//! fact is required. The new field carries `#[serde(default)]` for its
//! neutral value (`None` for an `Option<T>`), so a record serialised before
//! it existed still deserialises.
//!
//! **Required: one pull request touching this crate and one site per
//! consumer.** A new required fact is a `new` parameter: every forge's
//! construction site and every test-support helper fail to compile and are
//! fixed in the same pull request, each with the honest value; a forge that
//! cannot supply it returns [`ForgeError::Permanent`] from `observe` saying
//! so, never a placeholder. A new variant of an exhaustive enum fails to
//! compile at every match, and each consumer's arm is written in the same
//! pull request. A new effect is a [`Plan`] field and a [`Forge`] method
//! without a default body, so every forge implements it explicitly. A
//! required change, like anything under **Never**, also bumps the format
//! number of the application's journal, which records these types as JSON.
//!
//! **Never.** A renamed or re-typed field, a changed meaning behind an
//! unchanged name, a default body on a [`Forge`] method, or a `_` arm in a
//! `match` over an exhaustive contract enum outside tests. A positive
//! `matches!` naming the variants it is true for is allowed: its answer for
//! a variant it does not name is "not that one", and the crate holding it
//! also holds an exhaustive `match` over the same enum, so a new variant
//! still fails to compile there.
//!
//! A contract change and every consumer's edit land as one change. Where
//! several lines of work are in flight, the one carrying the contract change
//! lands first and the others rebase onto it.

pub mod ledger;

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A pull request, by the forge's number for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrNumber(pub u64);

/// A batch, by an id the scheduler chose (rule 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
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

/// Serialised as its 40-character text.
impl Serialize for Sha {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// Deserialised through [`Sha::parse`], so malformed text is an error.
impl<'de> Deserialize<'de> for Sha {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Sha, D::Error> {
        let text = String::deserialize(deserializer)?;
        Sha::parse(&text).map_err(serde::de::Error::custom)
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

/// A point in time, in seconds, on the forge's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub u64);

/// A required check's name, as the forge knows it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CheckName(pub String);

impl fmt::Display for CheckName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Everything the forge reports in one tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Observation {
    /// The forge's clock at this observation: wall time on a real forge,
    /// simulated time in a simulation. A scheduler uses only differences of
    /// `now`, and treats a value that did not increase since the previous
    /// observation as no time having passed.
    pub now: Timestamp,
    /// The current tip of the managed branch. A `trunk` that differs from
    /// the previous observation's and from every tracked batch's `Landed`
    /// tip was moved by a hand other than the queue's.
    pub trunk: Sha,
    /// The trunk position: how many pull requests have reached trunk, by
    /// the queue's hand or any other, since this forge's first observation,
    /// where it is 0. The forge counts its own landings exactly, by batch
    /// members; a change it did not make it counts as best it can and never
    /// fewer than the changes it brought.
    /// It never falls (rule 8). A scheduler reads positions from one forge
    /// and compares nothing across forges or restarts.
    pub trunk_position: u64,
    /// Every enqueued pull request, sorted by number, closed under
    /// [`PrSnapshot::base`]: every `PrBase::Pr(n)` names a pull request in
    /// `prs`.
    pub prs: Vec<PrSnapshot>,
    /// Every batch the forge tracks (started, not yet retired), sorted by id.
    pub batches: Vec<BatchStatus>,
    /// Exactly the pull requests the previous observation reported in `prs`
    /// and this one does not, sorted by number, each at the head the
    /// previous observation gave it. Empty on a forge's first observation.
    pub departed: Vec<Departure>,
}

impl Observation {
    /// A `Observation` with the given fields.
    pub fn new(
        now: Timestamp,
        trunk: Sha,
        trunk_position: u64,
        prs: Vec<PrSnapshot>,
        batches: Vec<BatchStatus>,
        departed: Vec<Departure>,
    ) -> Observation {
        Observation {
            now,
            trunk,
            trunk_position,
            prs,
            batches,
            departed,
        }
    }

    /// The first `Atomic` pull request, by number, that this observation
    /// reports and `batch` omits while holding one of its ancestors: landing
    /// `batch` would split that stack (rule 5).
    ///
    /// # Panics
    ///
    /// If `prs` is not sorted by number and closed under `base` (rule 8).
    pub fn split_atomic(&self, batch: &BatchStatus) -> Option<PrNumber> {
        let members: BTreeSet<PrNumber> = batch.members.iter().map(|m| m.pr).collect();
        self.prs
            .iter()
            .filter(|p| p.stack == StackPolicy::Atomic && !members.contains(&p.number))
            .find(|p| {
                let mut at = p.base;
                // Rule 8 makes the walk acyclic.
                while let PrBase::Pr(n) = at {
                    if members.contains(&n) {
                        return true;
                    }
                    let i = self
                        .prs
                        .binary_search_by_key(&n, |p| p.number)
                        .expect("rule 8: prs is sorted and closed under base");
                    at = self.prs[i].base;
                }
                false
            })
            .map(|p| p.number)
    }
}

/// One enqueued pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PrSnapshot {
    pub number: PrNumber,
    pub head: Sha,
    /// What it merges into: trunk, or another enqueued pull request.
    pub base: PrBase,
    /// Whether the forge would land it now, as the forge judges admission.
    pub status: PrStatus,
    /// Whether its ancestors may land without it. Default
    /// [`StackPolicy::Splittable`].
    #[serde(default)]
    pub stack: StackPolicy,
    /// Trunk changes the latest own-CI run on this head did not cover: the
    /// pull requests that reached trunk after the trunk that run started
    /// against. Grows by every change to trunk while the head and its run
    /// stand; falls when a newer own-CI run starts on the same head. A forge
    /// that cannot tell what a run covered reports the changes since the
    /// head's merge base with trunk, which is never fewer.
    /// `trunk_position − behind` is the position the run covered, so the two
    /// are reported in one unit.
    pub behind: u32,
}

impl PrSnapshot {
    /// A `PrSnapshot` with the given fields.
    pub fn new(
        number: PrNumber,
        head: Sha,
        base: PrBase,
        status: PrStatus,
        behind: u32,
    ) -> PrSnapshot {
        PrSnapshot {
            number,
            head,
            base,
            status,
            stack: StackPolicy::Splittable,
            behind,
        }
    }

    /// The same pull request with `stack` as its stack policy.
    pub fn with_stack(self, stack: StackPolicy) -> PrSnapshot {
        PrSnapshot { stack, ..self }
    }
}

/// A pull request's base branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrBase {
    Trunk,
    /// Another enqueued pull request's branch.
    Pr(PrNumber),
}

/// Whether a stacked pull request's ancestors may land without it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackPolicy {
    /// Any base-closed part of its stack may land on its own.
    #[default]
    Splittable,
    /// It and every ancestor land in one batch or not at all: no ancestor
    /// lands without it while it is enqueued.
    Atomic,
}

/// The forge's admission verdict on a pull request at its head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrStatus {
    /// The forge would land it now at this head.
    Ready,
    /// Not yet; the world may change it. Never empty.
    Waiting(BTreeSet<Hold>),
    /// It does not merge into its base; the author must act. `Trunk` for a
    /// pull request based on trunk, `Pr(base)` for one based on a pull
    /// request.
    Conflicting(ConflictWith),
}

/// What holds a pull request back. Open: every scheduler's response to a
/// hold is to wait, whatever it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Hold {
    /// Its own required checks have not finished on its head.
    OwnCiPending,
    /// Its own required checks failed on its head.
    OwnCiFailed,
    /// The forge has not yet computed whether it merges into its base.
    MergeabilityUnknown,
    /// The base branch's protection rules are not satisfied: a review, a
    /// code owner, an unresolved conversation, or a required check the
    /// queue does not list.
    Protection,
    /// It is a draft.
    Draft,
    /// Its own CI did not cover ancestor `pr`'s current head.
    AncestorMoved { pr: PrNumber },
}

impl fmt::Display for Hold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Hold::OwnCiPending => f.write_str("own CI pending"),
            Hold::OwnCiFailed => f.write_str("own CI failed"),
            Hold::MergeabilityUnknown => f.write_str("mergeability unknown"),
            Hold::Protection => f.write_str("blocked by branch protection"),
            Hold::Draft => f.write_str("draft"),
            Hold::AncestorMoved { pr } => write!(f, "ancestor #{} moved", pr.0),
        }
    }
}

/// The state of a set of CI checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiStatus {
    Pending,
    Passed,
    Failed(Failure),
}

/// What failed in a CI run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Failure {
    /// The required checks that failed. Empty when the forge cannot say
    /// which, such as a run it timed out itself.
    pub checks: BTreeSet<CheckName>,
}

impl Failure {
    /// A `Failure` with the given fields.
    pub fn new(checks: BTreeSet<CheckName>) -> Failure {
        Failure { checks }
    }
}

/// One tracked batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BatchStatus {
    pub id: BatchId,
    /// The base it was started on, echoed from the [`BatchStart`].
    pub base: Sha,
    /// Its members, in batch order, echoed from the [`BatchStart`].
    pub members: Vec<BatchMember>,
    pub state: BatchState,
}

impl BatchStatus {
    /// A `BatchStatus` with the given fields.
    pub fn new(
        id: BatchId,
        base: Sha,
        members: Vec<BatchMember>,
        state: BatchState,
    ) -> BatchStatus {
        BatchStatus {
            id,
            base,
            members,
            state,
        }
    }
}

/// A batch member and the head the batch was built with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BatchMember {
    pub pr: PrNumber,
    pub head: Sha,
}

impl BatchMember {
    /// A `BatchMember` with the given fields.
    pub fn new(pr: PrNumber, head: Sha) -> BatchMember {
        BatchMember { pr, head }
    }
}

/// Where a batch is in its life.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

impl BatchState {
    /// The batch's tip, in every state but `Conflict`.
    pub fn tip(&self) -> Option<&Sha> {
        match self {
            BatchState::Testing { tip, .. }
            | BatchState::Landed { tip }
            | BatchState::LandRefused { tip, .. } => Some(tip),
            BatchState::Conflict { .. } => None,
        }
    }
}

/// What a pull request or a batch member conflicts with. Open: a consumer
/// copies it or renders it through `Display`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConflictWith {
    Trunk,
    /// A pull request: an earlier member of the same batch, or the base of
    /// a stacked pull request.
    Pr(PrNumber),
}

impl fmt::Display for ConflictWith {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConflictWith::Trunk => f.write_str("trunk"),
            ConflictWith::Pr(pr) => write!(f, "#{}", pr.0),
        }
    }
}

/// Why the forge declined to land a batch. Open: every consumer's response
/// to a refusal is the same, whatever the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LandRefusal {
    /// The batch's base is no longer trunk.
    TrunkMoved { trunk: Sha },
    /// A member is no longer enqueued at the head the batch was built with.
    MemberGone { pr: PrNumber },
    /// The latest observation reported a member held by the repository's
    /// own verdict on its head.
    MemberHeld { pr: PrNumber },
}

impl fmt::Display for LandRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LandRefusal::TrunkMoved { trunk } => write!(f, "trunk moved to {trunk}"),
            LandRefusal::MemberGone { pr } => write!(f, "#{} is gone", pr.0),
            LandRefusal::MemberHeld { pr } => {
                write!(f, "#{} is held by the repository's rules", pr.0)
            }
        }
    }
}

/// A pull request that left the queue between two observations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Departure {
    pub pr: PrNumber,
    /// Its head at the previous observation.
    pub head: Sha,
    pub how: Departed,
}

impl Departure {
    /// A `Departure` with the given fields.
    pub fn new(pr: PrNumber, head: Sha, how: Departed) -> Departure {
        Departure { pr, head, how }
    }
}

/// How a pull request left the queue. A forge classifies in this order: a
/// member of a batch the observation reports `Landed` is `Landed`; one the
/// forge ejected since the previous observation is `Ejected`; the rest are
/// `Merged` when the platform says its content reached trunk, else
/// `Withdrawn`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Departed {
    /// The queue landed it with `batch`.
    Landed { batch: BatchId },
    /// Its content reached trunk by a hand other than the queue's.
    Merged,
    /// The forge ejected it at the scheduler's request.
    Ejected,
    /// It left without its content reaching trunk.
    Withdrawn(Withdrawal),
}

impl fmt::Display for Departed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Departed::Landed { batch } => write!(f, "landed in batch {}", batch.0),
            Departed::Merged => f.write_str("merged outside the queue"),
            Departed::Ejected => f.write_str("ejected"),
            Departed::Withdrawn(why) => write!(f, "withdrawn: {why}"),
        }
    }
}

/// Why a pull request left without landing or ejection. Open: a scheduler
/// treats every withdrawal alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Withdrawal {
    /// Its intent was cleared.
    IntentCleared,
    /// It was closed unmerged.
    Closed,
    /// Its base pull request is no longer enqueued.
    Blocked { on: PrNumber },
    /// It no longer targets trunk or an enqueued pull request.
    Retargeted,
}

impl fmt::Display for Withdrawal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Withdrawal::IntentCleared => f.write_str("intent cleared"),
            Withdrawal::Closed => f.write_str("closed"),
            Withdrawal::Blocked { on } => write!(f, "blocked on #{}", on.0),
            Withdrawal::Retargeted => f.write_str("retargeted"),
        }
    }
}

/// What the scheduler asks the forge to do in one tick, executed in the
/// order `land`, `notify`, `start`, `eject`, `retire` (rule 7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub start: Vec<BatchStart>,
    pub land: Vec<BatchId>,
    pub retire: Vec<BatchId>,
    pub eject: Vec<Ejection>,
    pub notify: Vec<Notice>,
}

impl Plan {
    /// True when the plan asks for nothing.
    pub fn is_empty(&self) -> bool {
        self.start.is_empty()
            && self.land.is_empty()
            && self.retire.is_empty()
            && self.eject.is_empty()
            && self.notify.is_empty()
    }
}

/// A request to build, push and track a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BatchStart {
    pub id: BatchId,
    /// Trunk, or the tip of a tracked batch (rule 5).
    pub base: Sha,
    /// The members in merge order, no duplicates; none for a probe.
    pub prs: Vec<PrNumber>,
}

impl BatchStart {
    /// A `BatchStart` with the given fields.
    pub fn new(id: BatchId, base: Sha, prs: Vec<PrNumber>) -> BatchStart {
        BatchStart { id, base, prs }
    }
}

/// A request to clear a pull request's intent and tell its author why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Ejection {
    pub pr: PrNumber,
    pub reason: EjectReason,
}

impl Ejection {
    /// A `Ejection` with the given fields.
    pub fn new(pr: PrNumber, reason: EjectReason) -> Ejection {
        Ejection { pr, reason }
    }
}

/// Why a pull request leaves the queue without landing. Open: a forge words
/// the variants it knows as it likes and renders the rest through `Display`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EjectReason {
    /// It does not merge into what the [`ConflictWith`] names.
    Conflicts { with: ConflictWith },
    /// It is held responsible for `batch`'s failure.
    BatchFailed { batch: BatchId },
}

impl fmt::Display for EjectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EjectReason::Conflicts { with } => write!(f, "conflicts with {with}"),
            EjectReason::BatchFailed { batch } => write!(f, "batch {} failed", batch.0),
        }
    }
}

/// A request to tell a pull request's author what the scheduler believes
/// about it, without acting on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Notice {
    pub pr: PrNumber,
    pub reason: NoticeReason,
}

impl Notice {
    /// A `Notice` with the given fields.
    pub fn new(pr: PrNumber, reason: NoticeReason) -> Notice {
        Notice { pr, reason }
    }
}

/// What the scheduler believes about a pull request it is not ejecting.
/// Exhaustive: the simulator judges each reason against its world's truth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeReason {
    /// It is probably broken against trunk: `batch` failed and nothing
    /// else in that run explains the failure.
    Broken { batch: BatchId },
    /// It probably fails only together with `pr`, which is also queued;
    /// `batch` is the run that singled the pair out. Each half of the pair
    /// is told, naming the other.
    InteractsWith { pr: PrNumber, batch: BatchId },
}

impl fmt::Display for NoticeReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoticeReason::Broken { batch } => {
                write!(f, "probably broken against trunk (batch {})", batch.0)
            }
            NoticeReason::InteractsWith { pr, batch } => write!(
                f,
                "probably fails together with #{} (batch {})",
                pr.0, batch.0
            ),
        }
    }
}

/// The decision side of the boundary.
pub trait Scheduler {
    /// Turns the latest observation into a plan. Deterministic (rule 4).
    /// An error is fatal: the scheduler cannot decide, and the process
    /// stops rather than act on a guess.
    // TODO(scheduler-introspection): a method returning why this step
    // planned what it did, for the journal to record beside the plan.
    fn step(&mut self, observation: &Observation) -> Result<Plan, SchedulerError>;
}

/// A scheduler that cannot produce a plan. Always fatal.
#[derive(Debug)]
pub struct SchedulerError(Box<dyn Error + Send + Sync>);

impl SchedulerError {
    /// A `SchedulerError` carrying `cause`.
    pub fn new(cause: impl Into<Box<dyn Error + Send + Sync>>) -> SchedulerError {
        SchedulerError(cause.into())
    }
}

impl fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "scheduler error: {}", self.0)
    }
}

impl Error for SchedulerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.as_ref())
    }
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
    /// Tells a pull request's author what the scheduler believes, and
    /// changes nothing (rules 5, 9).
    fn notify(&mut self, pr: PrNumber, reason: &NoticeReason) -> Result<(), ForgeError>;
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
    fn scheduler_error_wraps_and_displays_its_cause() {
        let error = SchedulerError::new("posterior underflow");
        assert_eq!(error.to_string(), "scheduler error: posterior underflow");
        assert_eq!(error.source().unwrap().to_string(), "posterior underflow");
    }

    #[test]
    fn timestamps_and_check_names_order_by_value() {
        assert!(Timestamp(3) < Timestamp(10));
        let names = BTreeSet::from([CheckName("lint".into()), CheckName("build".into())]);
        let words: Vec<String> = names.iter().map(ToString::to_string).collect();
        assert_eq!(words, ["build", "lint"]);
    }

    #[test]
    fn reasons_display_their_data() {
        let sha = Sha::parse(GOOD).unwrap();
        assert_eq!(ConflictWith::Trunk.to_string(), "trunk");
        assert_eq!(ConflictWith::Pr(PrNumber(12)).to_string(), "#12");
        assert_eq!(
            LandRefusal::TrunkMoved { trunk: sha }.to_string(),
            format!("trunk moved to {GOOD}")
        );
        assert_eq!(
            LandRefusal::MemberGone { pr: PrNumber(3) }.to_string(),
            "#3 is gone"
        );
        assert_eq!(
            LandRefusal::MemberHeld { pr: PrNumber(3) }.to_string(),
            "#3 is held by the repository's rules"
        );
        let holds = [
            Hold::OwnCiPending,
            Hold::OwnCiFailed,
            Hold::MergeabilityUnknown,
            Hold::Protection,
            Hold::Draft,
            Hold::AncestorMoved { pr: PrNumber(3) },
        ];
        let words: Vec<String> = holds.iter().map(ToString::to_string).collect();
        assert_eq!(
            words,
            [
                "own CI pending",
                "own CI failed",
                "mergeability unknown",
                "blocked by branch protection",
                "draft",
                "ancestor #3 moved",
            ]
        );
        let reasons = [
            EjectReason::Conflicts {
                with: ConflictWith::Trunk,
            },
            EjectReason::Conflicts {
                with: ConflictWith::Pr(PrNumber(12)),
            },
            EjectReason::BatchFailed { batch: BatchId(7) },
        ];
        let words: Vec<String> = reasons.iter().map(ToString::to_string).collect();
        assert_eq!(
            words,
            [
                "conflicts with trunk",
                "conflicts with #12",
                "batch 7 failed",
            ]
        );
        let notices = [
            NoticeReason::Broken { batch: BatchId(7) },
            NoticeReason::InteractsWith {
                pr: PrNumber(12),
                batch: BatchId(7),
            },
        ];
        let words: Vec<String> = notices.iter().map(ToString::to_string).collect();
        assert_eq!(
            words,
            [
                "probably broken against trunk (batch 7)",
                "probably fails together with #12 (batch 7)",
            ]
        );
        let departures = [
            Departed::Landed { batch: BatchId(7) },
            Departed::Merged,
            Departed::Ejected,
            Departed::Withdrawn(Withdrawal::IntentCleared),
            Departed::Withdrawn(Withdrawal::Closed),
            Departed::Withdrawn(Withdrawal::Blocked { on: PrNumber(3) }),
            Departed::Withdrawn(Withdrawal::Retargeted),
        ];
        let words: Vec<String> = departures.iter().map(ToString::to_string).collect();
        assert_eq!(
            words,
            [
                "landed in batch 7",
                "merged outside the queue",
                "ejected",
                "withdrawn: intent cleared",
                "withdrawn: closed",
                "withdrawn: blocked on #3",
                "withdrawn: retargeted",
            ]
        );
    }

    #[test]
    fn constructors_set_every_field() {
        let sha = Sha::parse(GOOD).unwrap();
        let status = PrStatus::Waiting(BTreeSet::from([Hold::OwnCiPending]));
        let pr = PrSnapshot::new(
            PrNumber(3),
            sha.clone(),
            PrBase::Pr(PrNumber(2)),
            status.clone(),
            7,
        );
        assert_eq!(
            (pr.number, &pr.head, pr.base, &pr.status, pr.behind),
            (PrNumber(3), &sha, PrBase::Pr(PrNumber(2)), &status, 7)
        );
        assert_eq!(pr.stack, StackPolicy::Splittable);
        let atomic = pr.clone().with_stack(StackPolicy::Atomic);
        assert_eq!(atomic.stack, StackPolicy::Atomic);
        assert_eq!(atomic.number, pr.number);
        let member = BatchMember::new(PrNumber(2), sha.clone());
        assert_eq!((member.pr, &member.head), (PrNumber(2), &sha));
        let start = BatchStart::new(BatchId(4), sha.clone(), vec![PrNumber(2)]);
        assert_eq!(
            (start.id, &start.base, start.prs.as_slice()),
            (BatchId(4), &sha, [PrNumber(2)].as_slice())
        );
        let reason = EjectReason::BatchFailed { batch: BatchId(4) };
        let ejection = Ejection::new(PrNumber(2), reason.clone());
        assert_eq!((ejection.pr, &ejection.reason), (PrNumber(2), &reason));
        let reason = NoticeReason::Broken { batch: BatchId(4) };
        let notice = Notice::new(PrNumber(2), reason.clone());
        assert_eq!((notice.pr, &notice.reason), (PrNumber(2), &reason));
        let departure = Departure::new(PrNumber(5), sha.clone(), Departed::Merged);
        assert_eq!(
            (departure.pr, &departure.head, &departure.how),
            (PrNumber(5), &sha, &Departed::Merged)
        );
        let observation = Observation::new(
            Timestamp(9),
            sha.clone(),
            11,
            vec![pr.clone()],
            vec![],
            vec![departure.clone()],
        );
        assert_eq!(
            (
                observation.now,
                &observation.trunk,
                observation.trunk_position,
                observation.prs.as_slice(),
                observation.departed.as_slice()
            ),
            (
                Timestamp(9),
                &sha,
                11,
                [pr].as_slice(),
                [departure].as_slice()
            )
        );
        let checks = BTreeSet::from([CheckName("build".into())]);
        assert_eq!(Failure::new(checks.clone()).checks, checks);
    }

    #[test]
    fn empty_plan_is_empty() {
        assert!(Plan::default().is_empty());
        let plan = Plan {
            land: vec![BatchId(1)],
            ..Plan::default()
        };
        assert!(!plan.is_empty());
        let plan = Plan {
            notify: vec![Notice::new(
                PrNumber(1),
                NoticeReason::Broken { batch: BatchId(1) },
            )],
            ..Plan::default()
        };
        assert!(!plan.is_empty());
    }

    fn sha_n(n: u64) -> Sha {
        Sha::parse(&format!("{n:040x}")).unwrap()
    }

    /// An observation holding every variant of every enum it can carry.
    fn every_variant_observation() -> Observation {
        let every_hold = BTreeSet::from([
            Hold::OwnCiPending,
            Hold::OwnCiFailed,
            Hold::MergeabilityUnknown,
            Hold::Protection,
            Hold::Draft,
            Hold::AncestorMoved { pr: PrNumber(1) },
        ]);
        let prs = vec![
            PrSnapshot::new(PrNumber(1), sha_n(1), PrBase::Trunk, PrStatus::Ready, 0),
            PrSnapshot::new(
                PrNumber(2),
                sha_n(2),
                PrBase::Pr(PrNumber(1)),
                PrStatus::Waiting(every_hold),
                3,
            )
            .with_stack(StackPolicy::Atomic),
            PrSnapshot::new(
                PrNumber(3),
                sha_n(3),
                PrBase::Trunk,
                PrStatus::Conflicting(ConflictWith::Trunk),
                1,
            ),
            PrSnapshot::new(
                PrNumber(4),
                sha_n(4),
                PrBase::Pr(PrNumber(1)),
                PrStatus::Conflicting(ConflictWith::Pr(PrNumber(1))),
                2,
            ),
        ];
        let member = |pr: u64| BatchMember::new(PrNumber(pr), sha_n(pr));
        let testing = |ci: CiStatus| BatchState::Testing { tip: sha_n(99), ci };
        let batches = vec![
            BatchStatus::new(
                BatchId(1),
                sha_n(10),
                vec![member(1), member(2)],
                BatchState::Conflict {
                    pr: PrNumber(2),
                    with: ConflictWith::Pr(PrNumber(1)),
                },
            ),
            BatchStatus::new(
                BatchId(2),
                sha_n(10),
                vec![member(1)],
                BatchState::Conflict {
                    pr: PrNumber(1),
                    with: ConflictWith::Trunk,
                },
            ),
            BatchStatus::new(BatchId(3), sha_n(10), vec![], testing(CiStatus::Pending)),
            BatchStatus::new(
                BatchId(4),
                sha_n(11),
                vec![member(1)],
                testing(CiStatus::Passed),
            ),
            BatchStatus::new(
                BatchId(5),
                sha_n(12),
                vec![member(1)],
                testing(CiStatus::Failed(Failure::new(BTreeSet::new()))),
            ),
            BatchStatus::new(
                BatchId(6),
                sha_n(13),
                vec![member(1)],
                testing(CiStatus::Failed(Failure::new(BTreeSet::from([
                    CheckName("lint".into()),
                    CheckName("build".into()),
                ])))),
            ),
            BatchStatus::new(
                BatchId(7),
                sha_n(14),
                vec![member(1)],
                BatchState::Landed { tip: sha_n(15) },
            ),
            BatchStatus::new(
                BatchId(8),
                sha_n(16),
                vec![member(1)],
                BatchState::LandRefused {
                    tip: sha_n(17),
                    reason: LandRefusal::TrunkMoved { trunk: sha_n(18) },
                },
            ),
            BatchStatus::new(
                BatchId(9),
                sha_n(19),
                vec![member(1)],
                BatchState::LandRefused {
                    tip: sha_n(20),
                    reason: LandRefusal::MemberGone { pr: PrNumber(1) },
                },
            ),
            BatchStatus::new(
                BatchId(10),
                sha_n(21),
                vec![member(1)],
                BatchState::LandRefused {
                    tip: sha_n(22),
                    reason: LandRefusal::MemberHeld { pr: PrNumber(1) },
                },
            ),
        ];
        let departed = [
            Departed::Landed { batch: BatchId(7) },
            Departed::Merged,
            Departed::Ejected,
            Departed::Withdrawn(Withdrawal::IntentCleared),
            Departed::Withdrawn(Withdrawal::Closed),
            Departed::Withdrawn(Withdrawal::Blocked { on: PrNumber(5) }),
            Departed::Withdrawn(Withdrawal::Retargeted),
        ]
        .into_iter()
        .zip(20u64..)
        .map(|(how, pr)| Departure::new(PrNumber(pr), sha_n(pr), how))
        .collect();
        Observation::new(
            Timestamp(1_761_520_000),
            sha_n(10),
            42,
            prs,
            batches,
            departed,
        )
    }

    /// A plan holding every effect and every reason.
    fn every_variant_plan() -> Plan {
        Plan {
            start: vec![
                BatchStart::new(BatchId(11), sha_n(10), vec![PrNumber(1), PrNumber(2)]),
                BatchStart::new(BatchId(12), sha_n(99), vec![]),
            ],
            land: vec![BatchId(4)],
            retire: vec![BatchId(5), BatchId(7)],
            eject: vec![
                Ejection::new(
                    PrNumber(3),
                    EjectReason::Conflicts {
                        with: ConflictWith::Trunk,
                    },
                ),
                Ejection::new(
                    PrNumber(4),
                    EjectReason::Conflicts {
                        with: ConflictWith::Pr(PrNumber(1)),
                    },
                ),
                Ejection::new(PrNumber(2), EjectReason::BatchFailed { batch: BatchId(6) }),
            ],
            notify: vec![
                Notice::new(PrNumber(1), NoticeReason::Broken { batch: BatchId(6) }),
                Notice::new(
                    PrNumber(2),
                    NoticeReason::InteractsWith {
                        pr: PrNumber(1),
                        batch: BatchId(6),
                    },
                ),
            ],
        }
    }

    #[test]
    fn every_variant_round_trips_through_json() {
        let observation = every_variant_observation();
        let text = serde_json::to_string(&observation).unwrap();
        let back: Observation = serde_json::from_str(&text).unwrap();
        assert_eq!(back, observation);

        let plan = every_variant_plan();
        let text = serde_json::to_string(&plan).unwrap();
        let back: Plan = serde_json::from_str(&text).unwrap();
        assert_eq!(back, plan);
    }

    #[test]
    fn a_small_observation_and_plan_have_a_fixed_encoding() {
        let observation = Observation::new(
            Timestamp(100),
            sha_n(1),
            3,
            vec![
                PrSnapshot::new(PrNumber(7), sha_n(7), PrBase::Trunk, PrStatus::Ready, 0),
                PrSnapshot::new(
                    PrNumber(8),
                    sha_n(8),
                    PrBase::Pr(PrNumber(7)),
                    PrStatus::Waiting(BTreeSet::from([
                        Hold::AncestorMoved { pr: PrNumber(7) },
                        Hold::OwnCiPending,
                    ])),
                    1,
                )
                .with_stack(StackPolicy::Atomic),
            ],
            vec![BatchStatus::new(
                BatchId(3),
                sha_n(1),
                vec![BatchMember::new(PrNumber(7), sha_n(7))],
                BatchState::Testing {
                    tip: sha_n(9),
                    ci: CiStatus::Failed(Failure::new(BTreeSet::from([CheckName("check".into())]))),
                },
            )],
            vec![Departure::new(
                PrNumber(5),
                sha_n(5),
                Departed::Landed { batch: BatchId(2) },
            )],
        );
        let s = |n: u64| format!("{n:040x}");
        let expected = format!(
            concat!(
                r#"{{"now":100,"trunk":"{t}","trunk_position":3,"prs":["#,
                r#"{{"number":7,"head":"{h7}","base":"trunk","status":"ready","stack":"splittable","behind":0}},"#,
                r#"{{"number":8,"head":"{h8}","base":{{"pr":7}},"#,
                r#""status":{{"waiting":["own_ci_pending",{{"ancestor_moved":{{"pr":7}}}}]}},"#,
                r#""stack":"atomic","behind":1}}],"#,
                r#""batches":[{{"id":3,"base":"{t}","members":[{{"pr":7,"head":"{h7}"}}],"#,
                r#""state":{{"testing":{{"tip":"{h9}","ci":{{"failed":{{"checks":["check"]}}}}}}}}}}],"#,
                r#""departed":[{{"pr":5,"head":"{h5}","how":{{"landed":{{"batch":2}}}}}}]}}"#,
            ),
            t = s(1),
            h5 = s(5),
            h7 = s(7),
            h8 = s(8),
            h9 = s(9),
        );
        assert_eq!(serde_json::to_string(&observation).unwrap(), expected);

        let plan = Plan {
            start: vec![BatchStart::new(BatchId(4), sha_n(1), vec![PrNumber(7)])],
            land: vec![BatchId(3)],
            retire: vec![BatchId(2)],
            eject: vec![Ejection::new(
                PrNumber(8),
                EjectReason::Conflicts {
                    with: ConflictWith::Trunk,
                },
            )],
            notify: vec![Notice::new(
                PrNumber(7),
                NoticeReason::InteractsWith {
                    pr: PrNumber(8),
                    batch: BatchId(3),
                },
            )],
        };
        let expected = format!(
            concat!(
                r#"{{"start":[{{"id":4,"base":"{t}","prs":[7]}}],"land":[3],"retire":[2],"#,
                r#""eject":[{{"pr":8,"reason":{{"conflicts":{{"with":"trunk"}}}}}}],"#,
                r#""notify":[{{"pr":7,"reason":{{"interacts_with":{{"pr":8,"batch":3}}}}}}]}}"#,
            ),
            t = s(1),
        );
        assert_eq!(serde_json::to_string(&plan).unwrap(), expected);
    }

    #[test]
    fn a_malformed_sha_fails_to_deserialise_naming_the_input() {
        for bad in [GOOD[..39].to_owned(), GOOD.to_uppercase()] {
            let error = serde_json::from_str::<Sha>(&format!("\"{bad}\"")).unwrap_err();
            let message = error.to_string();
            assert!(message.contains(&format!("{bad:?}")), "{message}");
        }
        let sha: Sha = serde_json::from_str(&format!("\"{GOOD}\"")).unwrap();
        assert_eq!(sha.as_str(), GOOD);
    }

    #[test]
    fn a_snapshot_without_stack_reads_as_splittable() {
        let text =
            format!(r#"{{"number":7,"head":"{GOOD}","base":"trunk","status":"ready","behind":0}}"#);
        let pr: PrSnapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(pr.stack, StackPolicy::Splittable);
        assert_eq!(
            pr,
            PrSnapshot::new(
                PrNumber(7),
                Sha::parse(GOOD).unwrap(),
                PrBase::Trunk,
                PrStatus::Ready,
                0
            )
        );
    }
}
