//! An in-memory [`Forge`] driven by a [`Scenario`].
//!
//! The mock's world moves only at [`Forge::observe`]: one call is one tick,
//! at which pull requests arrive, are re-pushed or withdraw as the scenario
//! says, and pending CI runs count down to their verdict. Commits are
//! synthetic, minted from a counter, so every run of a scenario is
//! identical. Effects check rules 5 and 6 of the [`dorfq_core`] contract
//! against the latest observation and the effects performed since it, and
//! answer a violation with [`ForgeError::Permanent`] carrying a
//! [`Violation`].
//!
//! Rule 9 is a message log: every message the mock would send an author is
//! written to the sink as one line when it happens and kept in
//! [`MockForge::messages`].

mod scenario;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::io::Write;

use dorfq_core::{
    BatchId, BatchMember, BatchStart, BatchState, BatchStatus, CiStatus, ConflictWith, EjectReason,
    Forge, ForgeError, LandRefusal, Mergeable, Observation, PrNumber, PrSnapshot, Sha,
};

use scenario::PrSpec;
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
    /// The pull request is in a batch under test.
    Testing { batch: BatchId },
    /// The pull request landed; trunk is now `tip`.
    Landed { tip: Sha },
    /// The scheduler ejected the pull request.
    Ejected { reason: EjectReason },
    /// The pull request's intent was cleared before it landed or was ejected.
    Withdrawn,
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "message to pr {}: ", self.pr.0)?;
        match &self.event {
            Event::Queued => f.write_str("queued"),
            Event::Testing { batch } => write!(f, "testing in batch {}", batch.0),
            Event::Landed { tip } => write!(f, "landed as {tip}"),
            Event::Ejected { reason } => match reason {
                EjectReason::ConflictsWithTrunk => f.write_str("ejected: conflicts with trunk"),
                EjectReason::ConflictsWith { pr } => {
                    write!(f, "ejected: conflicts with pr {}", pr.0)
                }
                EjectReason::BatchFailed { batch } => {
                    write!(f, "ejected: batch {} failed", batch.0)
                }
            },
            Event::Withdrawn => f.write_str("withdrawn"),
        }
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
}

struct MockBatch {
    status: BatchStatus,
    /// The CI result the batch will report once its countdown ends.
    verdict: CiStatus,
    /// Observations left until a `Testing` batch reports `verdict`.
    remaining: u64,
}

/// Effects performed since the latest observation, for rules 5 and 6.
#[derive(Default)]
struct Since {
    /// Pull requests an effect took out of `prs`: ejected, or landed.
    removed: BTreeSet<PrNumber>,
    started: BTreeSet<BatchId>,
    /// Batches `land` was called on, refused or not.
    landed: BTreeSet<BatchId>,
    retired: BTreeSet<BatchId>,
}

/// The mock forge.
pub struct MockForge {
    ci_ticks: u64,
    prs: BTreeMap<PrNumber, MockPr>,
    batches: BTreeMap<BatchId, MockBatch>,
    trunk: Sha,
    next_commit: u64,
    tick: u64,
    latest: Option<Observation>,
    since: Since,
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
                };
                (number, pr)
            })
            .collect();
        MockForge {
            ci_ticks: scenario.ci_ticks,
            prs,
            batches: BTreeMap::new(),
            trunk: commit(1),
            next_commit: 2,
            tick: 0,
            latest: None,
            since: Since::default(),
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
                lifecycle = Lifecycle::Enqueued;
                self.tell(number, Event::Queued)?;
            }
            if repushes && lifecycle == Lifecycle::Enqueued {
                let head = self.mint();
                self.set_pr(number, Lifecycle::Enqueued, Some(head));
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
                    *ci = batch.verdict;
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

    fn snapshot(&self) -> Observation {
        let prs = self
            .prs
            .iter()
            .filter(|(_, pr)| pr.lifecycle == Lifecycle::Enqueued)
            .map(|(&number, pr)| PrSnapshot {
                number,
                head: pr
                    .head
                    .clone()
                    .expect("an enqueued pull request has a head"),
                mergeable: if pr.spec.mergeable {
                    Mergeable::Clean
                } else {
                    Mergeable::Conflicting
                },
                head_ci: pr.spec.own_ci,
            })
            .collect();
        let batches = self.batches.values().map(|b| b.status.clone()).collect();
        Observation {
            trunk: self.trunk.clone(),
            prs,
            batches,
        }
    }

    fn observed_batch(&self, id: BatchId) -> Option<&BatchStatus> {
        self.latest
            .as_ref()
            .and_then(|o| o.batches.iter().find(|b| b.id == id))
    }

    fn batch_known(&self, id: BatchId) -> bool {
        self.observed_batch(id).is_some() && !self.since.retired.contains(&id)
    }

    fn pr_known(&self, pr: PrNumber) -> bool {
        self.observed_pr(pr) && !self.since.removed.contains(&pr)
    }

    /// True when `pr` is a member, as the latest observation reports it, of
    /// a batch `land` was called on since that observation.
    fn in_landing(&self, pr: PrNumber) -> bool {
        self.since.landed.iter().any(|&b| {
            self.observed_batch(b)
                .is_some_and(|s| s.members.iter().any(|m| m.pr == pr))
        })
    }

    fn observed_pr(&self, pr: PrNumber) -> bool {
        self.latest
            .as_ref()
            .is_some_and(|o| o.prs.iter().any(|p| p.number == pr))
    }

    fn check_start(&self, batch: &BatchStart) -> Result<(), Violation> {
        let id = batch.id;
        if self.observed_batch(id).is_some() || self.since.started.contains(&id) {
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

    /// Merges the members in order: the first that conflicts ends the build.
    fn build(&self, prs: &[PrNumber]) -> Option<BatchState> {
        for (i, number) in prs.iter().enumerate() {
            let spec = &self.prs[number].spec;
            if !spec.mergeable {
                return Some(BatchState::Conflict {
                    pr: *number,
                    with: ConflictWith::Trunk,
                });
            }
            if let Some(earlier) = prs[..i].iter().find(|e| spec.conflicts_with.contains(e)) {
                return Some(BatchState::Conflict {
                    pr: *number,
                    with: ConflictWith::Pr(*earlier),
                });
            }
        }
        None
    }

    fn verdict(&self, prs: &[PrNumber]) -> CiStatus {
        let fails = prs.iter().any(|number| {
            let spec = &self.prs[number].spec;
            spec.bad || prs.iter().any(|other| spec.breaks_with.contains(other))
        });
        if fails {
            CiStatus::Failed
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
            let pr = &self.prs[&member.pr];
            let still_there =
                pr.lifecycle == Lifecycle::Enqueued && pr.head.as_ref() == Some(&member.head);
            (!still_there).then_some(LandRefusal::MemberGone { pr: member.pr })
        })
    }
}

/// The synthetic commit numbered `n`.
fn commit(n: u64) -> Sha {
    Sha::parse(&format!("{n:040x}")).expect("40 lowercase hex digits")
}

fn violation(v: Violation) -> ForgeError {
    ForgeError::Permanent(Box::new(v))
}

impl Forge for MockForge {
    fn observe(&mut self) -> Result<Observation, ForgeError> {
        self.advance()?;
        self.tick += 1;
        let observation = self.snapshot();
        self.latest = Some(observation.clone());
        self.since = Since::default();
        Ok(observation)
    }

    fn start_batch(&mut self, batch: &BatchStart) -> Result<(), ForgeError> {
        self.check_start(batch).map_err(violation)?;
        let members = batch
            .prs
            .iter()
            .map(|&pr| BatchMember {
                pr,
                head: self.prs[&pr]
                    .head
                    .clone()
                    .expect("an observed pull request has a head"),
            })
            .collect();
        let verdict = self.verdict(&batch.prs);
        let state = match self.build(&batch.prs) {
            Some(conflict) => conflict,
            None => BatchState::Testing {
                tip: self.mint(),
                ci: if self.ci_ticks == 0 {
                    verdict
                } else {
                    CiStatus::Pending
                },
            },
        };
        let testing = matches!(state, BatchState::Testing { .. });
        self.batches.insert(
            batch.id,
            MockBatch {
                status: BatchStatus {
                    id: batch.id,
                    base: batch.base.clone(),
                    members,
                    state,
                },
                verdict,
                remaining: self.ci_ticks,
            },
        );
        self.since.started.insert(batch.id);
        if testing {
            for &pr in &batch.prs {
                self.tell(pr, Event::Testing { batch: batch.id })?;
            }
        }
        Ok(())
    }

    fn land(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        if !self.batch_known(batch) {
            return Err(violation(Violation::UnknownBatch {
                effect: "land",
                batch,
            }));
        }
        if self.since.landed.contains(&batch) {
            return Err(violation(Violation::LandedTwice { batch }));
        }
        let observed = self
            .observed_batch(batch)
            .expect("a known batch is in the latest observation");
        let BatchState::Testing {
            tip,
            ci: CiStatus::Passed,
        } = observed.state.clone()
        else {
            return Err(violation(Violation::NotPassed { batch }));
        };
        self.since.landed.insert(batch);
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
            self.since.removed.insert(member.pr);
        }
        for member in &status.members {
            self.tell(member.pr, Event::Landed { tip: tip.clone() })?;
        }
        Ok(())
    }

    fn retire(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        if !self.batch_known(batch) {
            return Err(violation(Violation::UnknownBatch {
                effect: "retire",
                batch,
            }));
        }
        self.batches.remove(&batch);
        self.since.retired.insert(batch);
        Ok(())
    }

    fn eject(&mut self, pr: PrNumber, reason: &EjectReason) -> Result<(), ForgeError> {
        if self.in_landing(pr) {
            return Err(violation(Violation::LandedInPlan {
                effect: "eject",
                pr,
            }));
        }
        if !self.pr_known(pr) {
            return Err(violation(Violation::UnknownPr { pr }));
        }
        self.set_pr(pr, Lifecycle::Ejected, None);
        self.since.removed.insert(pr);
        self.tell(
            pr,
            Event::Ejected {
                reason: reason.clone(),
            },
        )
    }
}

#[cfg(test)]
mod tests;
