//! An in-memory [`Forge`] driven by a [`Scenario`].
//!
//! The mock's world moves only at [`Forge::observe`]: one call is one tick,
//! at which pull requests arrive, are re-pushed or withdraw as the scenario
//! says, and pending CI runs count down to their verdict. Commits are
//! synthetic, minted from a counter, so every run of a scenario is
//! identical. Trunk moves only by the queue's landings, so an observation's
//! `trunk_position` is the count of pull requests landed since the forge was
//! built, and a pull request's `behind` is in the same unit: the position
//! less the one its head's CI covered, which is the scenario's `behind`
//! below the position at arrival and the position at a re-push.
//! Observations are checked against rule 8 and effects against rules 5 and
//! 6 of the [`dorfq_core`] contract through a [`Ledger`], and
//! a defect is answered with [`ForgeError::Permanent`] carrying a
//! [`Malformed`](dorfq_core::ledger::Malformed) or a
//! [`Violation`](dorfq_core::ledger::Violation).
//!
//! A batch with no members is a probe: its tip is minted over its base like
//! any other, and it passes unless the scenario sets `trunk_broken`, which
//! fails every batch and every probe.
//!
//! A scenario pull request may be based on another. It is reported with
//! that base while the base is enqueued and reported, with `base: Trunk`
//! once the base has landed, and not at all otherwise: a pull request
//! reported before departs `Withdrawn(Blocked)` and its author is told.
//!
//! Rule 9 is a message log: every message the mock would send an author is
//! written to the sink as one line when it happens and kept in
//! [`MockForge::messages`].

mod scenario;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Write;

use dorfq_core::ledger::Ledger;
use dorfq_core::{
    BatchId, BatchMember, BatchStart, BatchState, BatchStatus, CiStatus, ConflictWith, Departed,
    EjectReason, Failure, Forge, ForgeError, Hold, LandRefusal, NoticeReason, Observation, PrBase,
    PrNumber, PrSnapshot, PrStatus, Sha, StackPolicy, Timestamp, Withdrawal,
};

use scenario::{OwnCi, PrSpec};
pub use scenario::{Scenario, ScenarioError};

/// Something the mock told a pull request's author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub pr: PrNumber,
    pub event: Event,
}

/// The transitions authors hear about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The pull request entered the queue.
    Queued,
    /// The pull request is enqueued but held back for `holds`.
    Held { holds: BTreeSet<Hold> },
    /// The pull request is in a batch under test.
    Testing { batch: BatchId },
    /// The pull request landed; trunk is now `tip`.
    Landed { tip: Sha },
    /// The scheduler ejected the pull request.
    Ejected { reason: EjectReason },
    /// The scheduler told the author what it believes, and changed nothing.
    Notice { reason: NoticeReason },
    /// The pull request's intent was cleared before it landed or was ejected.
    Withdrawn,
    /// The pull request left the queue because its base `on` did.
    Blocked { on: PrNumber },
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "message to pr {}: ", self.pr.0)?;
        match &self.event {
            Event::Queued => f.write_str("queued"),
            Event::Held { holds } => {
                let holds: Vec<String> = holds.iter().map(ToString::to_string).collect();
                write!(f, "held: {}", holds.join(", "))
            }
            Event::Testing { batch } => write!(f, "testing in batch {}", batch.0),
            Event::Landed { tip } => write!(f, "landed as {tip}"),
            Event::Ejected { reason } => write!(f, "ejected: {reason}"),
            Event::Notice { reason } => write!(f, "notice: {reason}"),
            Event::Withdrawn => f.write_str("withdrawn"),
            Event::Blocked { on } => write!(f, "blocked on #{}", on.0),
        }
    }
}

/// Where a scenario pull request is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    NotArrived,
    Enqueued,
    Merged,
    Ejected,
    Withdrawn,
}

struct MockPr {
    spec: PrSpec,
    lifecycle: Lifecycle,
    /// Set when the pull request arrives.
    head: Option<Sha>,
    /// The trunk position its head's own CI covered, set when it arrives:
    /// the position less the scenario's `behind` at arrival, the position
    /// at a re-push. Below 0 where the scenario's `behind` predates the
    /// forge.
    covered: i64,
}

struct MockBatch {
    status: BatchStatus,
    /// The CI result the batch will report once its countdown ends.
    verdict: CiStatus,
    /// Observations left until a `Testing` batch reports `verdict`.
    remaining: u64,
}

/// The mock forge.
pub struct MockForge {
    ci_ticks: u64,
    trunk_broken: bool,
    prs: BTreeMap<PrNumber, MockPr>,
    batches: BTreeMap<BatchId, MockBatch>,
    trunk: Sha,
    /// Pull requests landed since construction: the trunk position.
    position: u64,
    next_commit: u64,
    tick: u64,
    ledger: Ledger,
    messages: Vec<Message>,
    sink: Box<dyn Write>,
}

impl MockForge {
    /// A forge at tick 0 of `scenario`, writing each message to `sink`.
    pub fn new(scenario: Scenario, sink: Box<dyn Write>) -> MockForge {
        let prs = scenario
            .prs
            .into_iter()
            .map(|(number, spec)| {
                let pr = MockPr {
                    spec,
                    lifecycle: Lifecycle::NotArrived,
                    head: None,
                    covered: 0,
                };
                (number, pr)
            })
            .collect();
        MockForge {
            ci_ticks: scenario.ci_ticks,
            trunk_broken: scenario.trunk_broken,
            prs,
            batches: BTreeMap::new(),
            trunk: commit(1),
            position: 0,
            next_commit: 2,
            tick: 0,
            ledger: Ledger::new(),
            messages: Vec::new(),
            sink,
        }
    }

    /// Every message sent so far, in order.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Whether some scenario pull request has yet to arrive. An observation
    /// with nothing queued and nothing in flight says nothing about the
    /// arrivals still to come.
    pub fn awaiting_arrivals(&self) -> bool {
        self.prs
            .values()
            .any(|pr| pr.lifecycle == Lifecycle::NotArrived)
    }

    fn mint(&mut self) -> Sha {
        let sha = commit(self.next_commit);
        self.next_commit += 1;
        sha
    }

    /// The trunk position, signed to compare with a covered position.
    fn position(&self) -> i64 {
        i64::try_from(self.position).expect("a trunk position fits i64")
    }

    fn tell(&mut self, pr: PrNumber, event: Event) -> Result<(), ForgeError> {
        let message = Message { pr, event };
        let written = writeln!(self.sink, "{message}");
        self.messages.push(message);
        written.map_err(|e| ForgeError::Permanent(Box::new(e)))
    }

    /// Moves the world to the current tick.
    fn advance(&mut self) -> Result<(), ForgeError> {
        let tick = self.tick;
        let numbers: Vec<PrNumber> = self.prs.keys().copied().collect();
        for number in numbers {
            let (arrives, repushes, dequeues, lifecycle) = {
                let pr = &self.prs[&number];
                (
                    pr.spec.arrives_at == tick,
                    pr.spec.repush_at == Some(tick),
                    pr.spec.dequeue_at == Some(tick),
                    pr.lifecycle,
                )
            };
            let mut lifecycle = lifecycle;
            if arrives && lifecycle == Lifecycle::NotArrived {
                let head = self.mint();
                self.set_pr(number, Lifecycle::Enqueued, Some(head));
                let position = self.position();
                let pr = self.prs.get_mut(&number).expect("scenario pull request");
                pr.covered = position - i64::from(pr.spec.behind);
                lifecycle = Lifecycle::Enqueued;
                self.tell(number, Event::Queued)?;
                let spec = &self.prs[&number].spec;
                if let Some(holds) = spec.mergeable.then(|| holds(spec)).flatten() {
                    self.tell(number, Event::Held { holds })?;
                }
            }
            if repushes && lifecycle == Lifecycle::Enqueued {
                let head = self.mint();
                self.set_pr(number, Lifecycle::Enqueued, Some(head));
                self.prs
                    .get_mut(&number)
                    .expect("scenario pull request")
                    .covered = self.position();
            }
            if dequeues && lifecycle == Lifecycle::Enqueued {
                self.set_pr(number, Lifecycle::Withdrawn, None);
                self.tell(number, Event::Withdrawn)?;
            }
        }
        for batch in self.batches.values_mut() {
            if let BatchState::Testing {
                ci: ci @ CiStatus::Pending,
                ..
            } = &mut batch.status.state
            {
                batch.remaining = batch.remaining.saturating_sub(1);
                if batch.remaining == 0 {
                    *ci = batch.verdict.clone();
                }
            }
        }
        Ok(())
    }

    /// Sets a pull request's lifecycle, and its head when `head` is given.
    fn set_pr(&mut self, number: PrNumber, lifecycle: Lifecycle, head: Option<Sha>) {
        let pr = self.prs.get_mut(&number).expect("scenario pull request");
        pr.lifecycle = lifecycle;
        if head.is_some() {
            pr.head = head;
        }
    }

    /// The base `number` is reported with, or `None` when it is not
    /// reported: not enqueued, or based on a pull request that is neither
    /// reported nor landed.
    fn reported_base(&self, number: PrNumber) -> Option<PrBase> {
        let pr = &self.prs[&number];
        if pr.lifecycle != Lifecycle::Enqueued {
            return None;
        }
        match pr.spec.base {
            None => Some(PrBase::Trunk),
            Some(base) if self.prs[&base].lifecycle == Lifecycle::Merged => Some(PrBase::Trunk),
            Some(base) => self.reported_base(base).map(|_| PrBase::Pr(base)),
        }
    }

    /// The observation at `now`. A departure the queue did not cause is the
    /// scenario's `dequeue_at`, a withdrawal of intent, or a base that left.
    fn snapshot(&self, now: Timestamp) -> Result<Observation, ForgeError> {
        let prs: Vec<PrSnapshot> = self
            .prs
            .iter()
            .filter_map(|(&number, pr)| {
                let base = self.reported_base(number)?;
                let snapshot = PrSnapshot::new(
                    number,
                    pr.head
                        .clone()
                        .expect("an enqueued pull request has a head"),
                    base,
                    status(&pr.spec, base),
                    u32::try_from(self.position() - pr.covered)
                        .expect("a head's staleness fits u32"),
                );
                Some(if pr.spec.atomic {
                    snapshot.with_stack(StackPolicy::Atomic)
                } else {
                    snapshot
                })
            })
            .collect();
        let batches: Vec<BatchStatus> = self.batches.values().map(|b| b.status.clone()).collect();
        let departed = self
            .ledger
            .departures(&prs, &batches, |gone| {
                let pr = &self.prs[&gone.number];
                match (pr.lifecycle, pr.spec.base) {
                    (Lifecycle::Withdrawn, _) => Ok(Departed::Withdrawn(Withdrawal::IntentCleared)),
                    (Lifecycle::Enqueued, Some(on)) => {
                        Ok(Departed::Withdrawn(Withdrawal::Blocked { on }))
                    }
                    (lifecycle, _) => Err(ForgeError::Permanent(
                    format!(
                        "pr {} left the queue {lifecycle:?} with no batch landed or ejection to show for it",
                        gone.number.0
                    )
                    .into(),
                    )),
                }
            })?;
        Ok(Observation::new(
            now,
            self.trunk.clone(),
            self.position,
            prs,
            batches,
            departed,
        ))
    }

    /// Merges the members in order: the first that conflicts with an earlier
    /// member ends the build. Conflicts with trunk are not checked: a member
    /// that conflicts with trunk is never `Ready`, so no valid plan starts it.
    fn build(&self, prs: &[PrNumber]) -> Option<BatchState> {
        for (i, number) in prs.iter().enumerate() {
            let spec = &self.prs[number].spec;
            if let Some(earlier) = prs[..i].iter().find(|e| spec.conflicts_with.contains(e)) {
                return Some(BatchState::Conflict {
                    pr: *number,
                    with: ConflictWith::Pr(*earlier),
                });
            }
        }
        None
    }

    /// Failed when trunk is broken or a member is bad or breaks with another
    /// member; passed otherwise, a probe included.
    fn verdict(&self, prs: &[PrNumber]) -> CiStatus {
        let fails = self.trunk_broken
            || prs.iter().any(|number| {
                let spec = &self.prs[number].spec;
                spec.bad || prs.iter().any(|other| spec.breaks_with.contains(other))
            });
        if fails {
            // The scenario names no checks, so a failure carries none.
            CiStatus::Failed(Failure::new(BTreeSet::new()))
        } else {
            CiStatus::Passed
        }
    }

    fn refusal(&self, status: &BatchStatus) -> Option<LandRefusal> {
        if status.base != self.trunk {
            return Some(LandRefusal::TrunkMoved {
                trunk: self.trunk.clone(),
            });
        }
        status.members.iter().find_map(|member| {
            let still_there = self.reported_base(member.pr).is_some()
                && self.prs[&member.pr].head.as_ref() == Some(&member.head);
            (!still_there).then_some(LandRefusal::MemberGone { pr: member.pr })
        })
    }
}

/// A scenario pull request's admission on `base`: `Conflicting` with its
/// base when it does not merge, else held while its own CI is pending or
/// failed, else `Ready`.
fn status(spec: &PrSpec, base: PrBase) -> PrStatus {
    if !spec.mergeable {
        return PrStatus::Conflicting(match base {
            PrBase::Trunk => ConflictWith::Trunk,
            PrBase::Pr(pr) => ConflictWith::Pr(pr),
        });
    }
    holds(spec).map_or(PrStatus::Ready, PrStatus::Waiting)
}

/// What holds a scenario pull request back, if anything: its own CI.
fn holds(spec: &PrSpec) -> Option<BTreeSet<Hold>> {
    match spec.own_ci {
        OwnCi::Passed => None,
        OwnCi::Pending => Some(BTreeSet::from([Hold::OwnCiPending])),
        OwnCi::Failed => Some(BTreeSet::from([Hold::OwnCiFailed])),
    }
}

/// The synthetic commit numbered `n`.
fn commit(n: u64) -> Sha {
    Sha::parse(&format!("{n:040x}")).expect("40 lowercase hex digits")
}

impl Forge for MockForge {
    fn observe(&mut self) -> Result<Observation, ForgeError> {
        self.advance()?;
        let now = Timestamp(self.tick);
        self.tick += 1;
        let observation = self.snapshot(now)?;
        self.ledger.observed(&observation)?;
        for departure in &observation.departed {
            if let Departed::Withdrawn(Withdrawal::Blocked { on }) = departure.how {
                self.tell(departure.pr, Event::Blocked { on })?;
            }
        }
        Ok(observation)
    }

    fn start_batch(&mut self, batch: &BatchStart) -> Result<(), ForgeError> {
        self.ledger.check_start(batch)?;
        let members = batch
            .prs
            .iter()
            .map(|&pr| {
                let head = self.prs[&pr]
                    .head
                    .clone()
                    .expect("an observed pull request has a head");
                BatchMember::new(pr, head)
            })
            .collect();
        let verdict = self.verdict(&batch.prs);
        let state = match self.build(&batch.prs) {
            Some(conflict) => conflict,
            None => BatchState::Testing {
                tip: self.mint(),
                ci: if self.ci_ticks == 0 {
                    verdict.clone()
                } else {
                    CiStatus::Pending
                },
            },
        };
        let testing = matches!(state, BatchState::Testing { .. });
        self.batches.insert(
            batch.id,
            MockBatch {
                status: BatchStatus::new(batch.id, batch.base.clone(), members, state),
                verdict,
                remaining: self.ci_ticks,
            },
        );
        self.ledger.started(batch.id);
        if testing {
            for &pr in &batch.prs {
                self.tell(pr, Event::Testing { batch: batch.id })?;
            }
        }
        Ok(())
    }

    fn land(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        let tip = self.ledger.check_land(batch)?.tip;
        self.ledger.landing(batch);
        let status = self.batches[&batch].status.clone();
        if let Some(reason) = self.refusal(&status) {
            self.batches
                .get_mut(&batch)
                .expect("known batch")
                .status
                .state = BatchState::LandRefused { tip, reason };
            return Ok(());
        }
        self.trunk = tip.clone();
        self.batches
            .get_mut(&batch)
            .expect("known batch")
            .status
            .state = BatchState::Landed { tip: tip.clone() };
        for member in &status.members {
            self.set_pr(member.pr, Lifecycle::Merged, None);
        }
        self.position += u64::try_from(status.members.len()).expect("a batch's members fit u64");
        self.ledger.landed(batch);
        for member in &status.members {
            self.tell(member.pr, Event::Landed { tip: tip.clone() })?;
        }
        Ok(())
    }

    fn retire(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        self.ledger.check_retire(batch)?;
        self.batches.remove(&batch);
        self.ledger.retired(batch);
        Ok(())
    }

    fn eject(&mut self, pr: PrNumber, reason: &EjectReason) -> Result<(), ForgeError> {
        self.ledger.check_eject(pr)?;
        self.set_pr(pr, Lifecycle::Ejected, None);
        self.ledger.ejected(pr);
        self.tell(
            pr,
            Event::Ejected {
                reason: reason.clone(),
            },
        )
    }

    fn notify(&mut self, pr: PrNumber, reason: &NoticeReason) -> Result<(), ForgeError> {
        self.ledger.check_notify(pr)?;
        self.ledger.notified(pr);
        self.tell(
            pr,
            Event::Notice {
                reason: reason.clone(),
            },
        )
    }
}

#[cfg(test)]
mod tests;
