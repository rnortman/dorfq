//! The event stream: what a journal says happened, event by event, in the
//! simulator's trace vocabulary wherever the meaning is the same.
//!
//! Facts come from observations, reasons from plans. Each journaled
//! observation is compared with the previous one, carried across `same`
//! lines and across headers; the plans supply ejection and notice reasons.
//! Across a `start` header pull requests carry over and the old process's
//! batch ids and trunk position do not.

use std::collections::{BTreeMap, BTreeSet};

use dorfq_core::{
    BatchState, BatchStatus, CiStatus, ConflictWith, Departed, Hold, Observation, Plan, PrBase,
    PrSnapshot, PrStatus, Sha, Withdrawal,
};
use serde::Serialize;
use serde_json::Value;

use super::{Cause, Entry, Failure, Header, LineKind};

/// One thing that happened, at `t` seconds on the forge's clock.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub t: u64,
    #[serde(flatten)]
    pub kind: Kind,
}

/// What happened. Pull requests and batches are their numbers.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    /// A pull request entered the queue, stacked on `base` or on trunk: a
    /// new lifetime, even for a number seen before.
    Arrive {
        pr: u64,
        base: Option<u64>,
        atomic: bool,
        head: Sha,
    },
    /// The queue sees the pull request from now; with every `arrive`.
    Admit { pr: u64, base: Option<u64> },
    /// Its head changed.
    Repush { pr: u64, head: Sha },
    /// It stopped merging into its base: `"trunk"` or a pull request's
    /// number.
    Conflicting { pr: u64, with: Value },
    /// Its holds other than its own CI's changed to `holds`.
    Held { pr: u64, holds: Vec<Hold> },
    /// Its own CI run started on its head.
    PrCi { pr: u64 },
    /// Its own CI run ended: passed, `failed`, or `cancelled` by a head
    /// change.
    PrCiEnd {
        pr: u64,
        failed: bool,
        cancelled: bool,
    },
    /// It left without its content reaching trunk, `how` saying why.
    Withdraw { pr: u64, how: Withdrawal },
    /// It left because its base pull request `on` is no longer enqueued.
    Blocked { pr: u64, on: u64, r#final: bool },
    /// Its content reached trunk by a hand other than the queue's.
    Merged { pr: u64 },
    /// The queue ejected it; `reason` is the plan's, `None` when the plan
    /// that asked for it is not in the input.
    Eject {
        pr: u64,
        reason: Option<String>,
        r#final: bool,
    },
    /// It is gone and no observation said how: it left while the process
    /// was down or in a tick the record lacks.
    Lost { pr: u64 },
    /// A batch started on the tip of batch `base`, or on trunk when `None`;
    /// a `probe` has no members.
    Start {
        batch: u64,
        base: Option<u64>,
        members: Vec<u64>,
        probe: bool,
    },
    /// The batch's run ended; `checks` are the failed required checks,
    /// empty when the forge could not say.
    RunEnd {
        batch: u64,
        passed: bool,
        checks: Vec<String>,
    },
    /// Trunk was fast-forwarded to the batch's tip.
    Land { batch: u64, members: Vec<u64> },
    /// The forge declined to land the batch.
    Refused { batch: u64, reason: String },
    /// Building the batch, pull request `pr` did not merge into `with`.
    Conflict { batch: u64, pr: u64, with: Value },
    /// The batch is no longer tracked; `cancelled` when its run was pending.
    Retire { batch: u64, cancelled: bool },
    /// Trunk moved to `position` at `sha`: `landed` are the pull requests
    /// the queue landed, `foreign` how many more reached it otherwise.
    /// Positions count from each process's first observation, so none is
    /// said across a restart.
    Trunk {
        position: u64,
        sha: String,
        landed: Vec<u64>,
        foreign: u64,
    },
    /// The queue told a pull request's author what it believes.
    Notice { pr: u64, reason: String },
    /// One scheduler step: the observation's size and the plan; `at` the
    /// host clock, `tick` the driver's count.
    Step {
        queued: usize,
        in_flight: usize,
        land: Vec<u64>,
        notify: Vec<PlannedPr>,
        start: Vec<PlannedStart>,
        eject: Vec<PlannedPr>,
        retire: Vec<u64>,
        at: u64,
        tick: u64,
    },
    /// A tick ended in an error; the process exited when `fatal`.
    Error {
        message: String,
        fatal: bool,
        at: u64,
    },
    /// A process started writing, `started` and `opened` (`at`) on the
    /// host clock.
    Restart {
        started: u64,
        dorfq: String,
        at: u64,
    },
}

/// One start of a step's plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedStart {
    pub batch: u64,
    pub members: Vec<u64>,
}

/// One pull request a step's plan ejects or notifies, its reason worded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedPr {
    pub pr: u64,
    pub reason: String,
}

/// `event` as one line of the event stream's text: its JSON, then `\n`.
pub fn line(event: &Event) -> String {
    serde_json::to_string(event).expect("an event serialises") + "\n"
}

/// The event stream of a journal's entries, in order.
pub fn derive(entries: &[Entry]) -> Vec<Event> {
    let mut fold = Fold::default();
    for entry in entries {
        fold.entry(entry);
    }
    let mut events = fold.take();
    events.extend(fold.finish());
    events
}

/// The event stream folded one journal entry at a time: each entry point
/// adds its events, which [`Fold::take`] drains. Its state is bounded by
/// the last two observations: batch tips and landings of batches absent
/// from both are forgotten, as are ejection reasons of pull requests gone.
#[derive(Default)]
pub struct Fold {
    /// The events not yet taken.
    events: Vec<Event>,
    /// The previous journaled observation.
    previous: Option<Observation>,
    /// Whether a header has been read.
    headed: bool,
    /// Whether an observation has been folded since the last header.
    observed: bool,
    /// Events whose `t` is the next observation's `now`.
    waiting: Vec<Kind>,
    /// The latest plan ejection's reason, by queued pull request.
    pub(super) ejections: BTreeMap<u64, String>,
    /// The tips of the batches in the last two observations, to the
    /// batch's id.
    pub(super) tips: BTreeMap<Sha, u64>,
    /// The batches of the last two observations seen `Landed`.
    pub(super) landed: BTreeSet<u64>,
    /// A `start` header was read and no observation has followed it.
    restarted: bool,
}

impl Fold {
    /// Folds one entry of a journal.
    pub fn entry(&mut self, entry: &Entry) {
        match entry {
            Entry::Header(header) => self.header(header),
            Entry::Tick(line) => match &line.kind {
                LineKind::Report { report, failure } => {
                    self.observe(&report.observation, &report.plan, line.at, line.tick);
                    if let Some(failure) = failure {
                        self.error(failure, line.at);
                    }
                }
                LineKind::Same { now } => self.same(*now, line.at, line.tick),
                LineKind::Error(failure) => self.error(failure, line.at),
            },
        }
    }

    /// The events produced since the last call, in order.
    pub fn take(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    fn push(&mut self, t: u64, kind: Kind) {
        self.events.push(Event { t, kind });
    }

    /// Folds a header.
    pub fn header(&mut self, header: &Header) {
        if self.headed && header.cause == Cause::Start {
            self.waiting.push(Kind::Restart {
                started: header.started,
                dorfq: header.dorfq.clone(),
                at: header.opened,
            });
        }
        self.restarted |= header.cause == Cause::Start;
        self.headed = true;
        self.observed = false;
    }

    /// What a restart keeps: pull requests and shas are the repository's
    /// and carry over, while batch ids and the trunk position are the
    /// process's. A restarted process's first observation precedes its
    /// first step, so its batches are the ones it adopted; every other
    /// batch id is forgotten, free for the new process to mint again.
    fn forget_the_old_process(&mut self, o: &Observation) {
        let adopted = |id: &u64| o.batches.iter().any(|b| b.id.0 == *id);
        self.tips.retain(|_, id| adopted(id));
        self.landed.retain(adopted);
    }

    /// Folds the error a tick ended with, `at` on the host clock.
    pub fn error(&mut self, failure: &Failure, at: u64) {
        let kind = Kind::Error {
            message: failure.message.clone(),
            fatal: failure.fatal,
            at,
        };
        match &self.previous {
            // An error after a header waits for the next observation: one
            // after a restart stays behind it, and a fold that begins at a
            // `rotate` header places it where one that read the segment
            // before does.
            Some(previous) if self.observed => {
                let t = previous.now.0;
                self.push(t, kind);
            }
            _ => self.waiting.push(kind),
        }
    }

    /// Gives every waiting event the time `t`.
    fn release(&mut self, t: u64) {
        for kind in std::mem::take(&mut self.waiting) {
            self.push(t, kind);
        }
    }

    /// Ends the stream: the events still waiting for an observation, at the
    /// last observation's `now`, or 0 with none. Every event before them was
    /// taken.
    pub fn finish(mut self) -> Vec<Event> {
        let t = self.previous.as_ref().map_or(0, |o| o.now.0);
        self.release(t);
        self.events
    }

    /// Folds a `same` line: the previous observation again at `now`.
    pub fn same(&mut self, now: dorfq_core::Timestamp, at: u64, tick: u64) {
        let Some(previous) = &mut self.previous else {
            // A `same` line follows a `report` in every journal the writer
            // makes; without one there is nothing to repeat.
            return;
        };
        previous.now = now;
        let step = step(previous, &Plan::default(), at, tick);
        self.release(now.0);
        self.observed = true;
        self.push(now.0, step);
    }

    /// Folds an observation and the plan made on it.
    pub fn observe(&mut self, o: &Observation, plan: &Plan, at: u64, tick: u64) {
        let t = o.now.0;
        self.release(t);
        self.observed = true;
        let restarted = std::mem::take(&mut self.restarted);
        if restarted {
            self.forget_the_old_process(o);
        }
        let previous = self.previous.take();
        let p = previous.as_ref();
        let before: BTreeMap<u64, &PrSnapshot> = p
            .map(|p| p.prs.iter().map(|s| (s.number.0, s)).collect())
            .unwrap_or_default();
        let now: BTreeMap<u64, &PrSnapshot> = o.prs.iter().map(|s| (s.number.0, s)).collect();
        let was: BTreeMap<u64, &BatchStatus> = p
            .map(|p| p.batches.iter().map(|b| (b.id.0, b)).collect())
            .unwrap_or_default();

        self.departures(t, o, &before, &now);
        self.ejections.retain(|pr, _| now.contains_key(pr));
        // A batch first seen gets its endings after its `start`.
        let mut fresh = Vec::new();
        let mut newly_landed = Vec::new();
        for batch in &o.batches {
            let before = was.get(&batch.id.0).map(|b| &b.state);
            let endings = self.endings(batch, before);
            for ending in &endings {
                if let Kind::Land { members, .. } = ending {
                    newly_landed.extend(members);
                }
            }
            if before.is_some() {
                for ending in endings {
                    self.push(t, ending);
                }
            } else {
                fresh.push((batch, endings));
            }
        }
        for (&id, batch) in &was {
            if o.batches.binary_search_by_key(&id, |b| b.id.0).is_err() {
                let cancelled = matches!(
                    batch.state,
                    BatchState::Testing {
                        ci: CiStatus::Pending,
                        ..
                    }
                );
                self.push(
                    t,
                    Kind::Retire {
                        batch: id,
                        cancelled,
                    },
                );
            }
        }
        if let Some(p) = p
            && !restarted
            && o.trunk_position != p.trunk_position
        {
            let gained = o.trunk_position.saturating_sub(p.trunk_position);
            self.push(
                t,
                Kind::Trunk {
                    position: o.trunk_position,
                    sha: o.trunk.to_string(),
                    foreign: gained.saturating_sub(newly_landed.len() as u64),
                    landed: newly_landed,
                },
            );
        }
        for snapshot in &o.prs {
            self.pr_changes(t, before.get(&snapshot.number.0).copied(), snapshot);
        }
        for (batch, endings) in fresh {
            self.start(t, batch);
            for ending in endings {
                self.push(t, ending);
            }
        }
        for notice in &plan.notify {
            self.push(
                t,
                Kind::Notice {
                    pr: notice.pr.0,
                    reason: notice.reason.to_string(),
                },
            );
        }
        for ejection in &plan.eject {
            self.ejections
                .insert(ejection.pr.0, ejection.reason.to_string());
        }
        self.push(t, step(o, plan, at, tick));
        // A batch starts on trunk or the tip of a batch tracked at its
        // plan's observation, and is observed at the next: its base is in
        // this observation or the one before.
        let kept = |id: &u64| was.contains_key(id) || o.batches.iter().any(|b| b.id.0 == *id);
        self.tips.retain(|_, id| kept(id));
        self.landed.retain(kept);
        self.previous = Some(o.clone());
    }

    fn departures(
        &mut self,
        t: u64,
        o: &Observation,
        before: &BTreeMap<u64, &PrSnapshot>,
        now: &BTreeMap<u64, &PrSnapshot>,
    ) {
        let mut said = BTreeSet::new();
        for departure in &o.departed {
            let pr = departure.pr.0;
            said.insert(pr);
            let kind = match &departure.how {
                Departed::Landed { .. } => continue,
                Departed::Merged => Kind::Merged { pr },
                Departed::Ejected => Kind::Eject {
                    pr,
                    reason: self.ejections.remove(&pr),
                    r#final: true,
                },
                Departed::Withdrawn(Withdrawal::Blocked { on }) => Kind::Blocked {
                    pr,
                    on: on.0,
                    r#final: true,
                },
                Departed::Withdrawn(how) => Kind::Withdraw { pr, how: *how },
            };
            self.push(t, kind);
        }
        for &pr in before.keys() {
            if !now.contains_key(&pr) && !said.contains(&pr) {
                self.push(t, Kind::Lost { pr });
            }
        }
    }

    /// The endings `batch` reached since `before`, its state in the
    /// previous observation; records its tip, and whether it landed.
    fn endings(&mut self, batch: &BatchStatus, before: Option<&BatchState>) -> Vec<Kind> {
        let id = batch.id.0;
        let mut endings = Vec::new();
        if let Some(tip) = batch.state.tip() {
            self.tips.insert(tip.clone(), id);
        }
        if let Some((passed, checks)) = verdict(&batch.state)
            && before.is_none_or(|s| s.tip().is_some() && verdict(s).is_none())
        {
            endings.push(Kind::RunEnd {
                batch: id,
                passed,
                checks,
            });
        }
        match &batch.state {
            BatchState::Landed { .. } if !matches!(before, Some(BatchState::Landed { .. })) => {
                self.landed.insert(id);
                endings.push(Kind::Land {
                    batch: id,
                    members: batch.members.iter().map(|m| m.pr.0).collect(),
                });
            }
            BatchState::LandRefused { reason, .. }
                if !matches!(before, Some(BatchState::LandRefused { .. })) =>
            {
                endings.push(Kind::Refused {
                    batch: id,
                    reason: reason.to_string(),
                });
            }
            BatchState::Conflict { pr, with } if before.is_none() => {
                endings.push(Kind::Conflict {
                    batch: id,
                    pr: pr.0,
                    with: against(with),
                });
            }
            _ => {}
        }
        endings
    }

    fn pr_changes(&mut self, t: u64, before: Option<&PrSnapshot>, now: &PrSnapshot) {
        let pr = now.number.0;
        let base = match now.base {
            PrBase::Trunk => None,
            PrBase::Pr(n) => Some(n.0),
        };
        let repushed = before.is_some_and(|b| b.head != now.head);
        match before {
            None => {
                self.push(
                    t,
                    Kind::Arrive {
                        pr,
                        base,
                        atomic: now.stack == dorfq_core::StackPolicy::Atomic,
                        head: now.head.clone(),
                    },
                );
                self.push(t, Kind::Admit { pr, base });
            }
            Some(_) if repushed => self.push(
                t,
                Kind::Repush {
                    pr,
                    head: now.head.clone(),
                },
            ),
            Some(_) => {}
        }
        if let PrStatus::Conflicting(with) = &now.status
            && before.is_none_or(|b| b.status != now.status)
        {
            self.push(
                t,
                Kind::Conflicting {
                    pr,
                    with: against(with),
                },
            );
        }
        let holds = other_holds(now);
        if before.map_or(!holds.is_empty(), |b| other_holds(b) != holds) {
            self.push(t, Kind::Held { pr, holds });
        }
        let pending = holding(now, Hold::OwnCiPending);
        let was_pending = before.is_some_and(|b| holding(b, Hold::OwnCiPending));
        // A head change ends the old head's run; a new one pending on the
        // new head is a fresh start.
        if was_pending && (!pending || repushed) {
            self.push(
                t,
                Kind::PrCiEnd {
                    pr,
                    failed: !repushed && holding(now, Hold::OwnCiFailed),
                    cancelled: repushed,
                },
            );
        }
        if pending && (!was_pending || repushed) {
            self.push(t, Kind::PrCi { pr });
        }
    }

    fn start(&mut self, t: u64, batch: &BatchStatus) {
        let base = self
            .tips
            .get(&batch.base)
            .copied()
            .filter(|id| *id != batch.id.0 && !self.landed.contains(id));
        let members: Vec<u64> = batch.members.iter().map(|m| m.pr.0).collect();
        self.push(
            t,
            Kind::Start {
                batch: batch.id.0,
                base,
                probe: members.is_empty(),
                members,
            },
        );
    }
}

/// The run's verdict in `state`, when it has one: `Landed` and
/// `LandRefused` follow a passed run.
fn verdict(state: &BatchState) -> Option<(bool, Vec<String>)> {
    match state {
        BatchState::Testing {
            ci: CiStatus::Passed,
            ..
        }
        | BatchState::Landed { .. }
        | BatchState::LandRefused { .. } => Some((true, Vec::new())),
        BatchState::Testing {
            ci: CiStatus::Failed(failure),
            ..
        } => Some((false, failure.checks.iter().map(|c| c.0.clone()).collect())),
        BatchState::Testing {
            ci: CiStatus::Pending,
            ..
        }
        | BatchState::Conflict { .. } => None,
    }
}

/// `"trunk"`, or the pull request's number.
fn against(with: &ConflictWith) -> Value {
    match with {
        ConflictWith::Trunk => Value::from("trunk"),
        ConflictWith::Pr(pr) => Value::from(pr.0),
        other => Value::from(other.to_string()),
    }
}

fn holding(snapshot: &PrSnapshot, hold: Hold) -> bool {
    matches!(&snapshot.status, PrStatus::Waiting(holds) if holds.contains(&hold))
}

/// Its holds, its own CI's left out.
fn other_holds(snapshot: &PrSnapshot) -> Vec<Hold> {
    match &snapshot.status {
        PrStatus::Waiting(holds) => holds
            .iter()
            .filter(|h| !matches!(h, Hold::OwnCiPending | Hold::OwnCiFailed))
            .copied()
            .collect(),
        PrStatus::Ready | PrStatus::Conflicting(_) => Vec::new(),
    }
}

fn step(o: &Observation, plan: &Plan, at: u64, tick: u64) -> Kind {
    let in_flight = o
        .batches
        .iter()
        .filter(|b| {
            matches!(
                b.state,
                BatchState::Testing {
                    ci: CiStatus::Pending,
                    ..
                }
            )
        })
        .count();
    let planned = |pr: u64, reason: String| PlannedPr { pr, reason };
    Kind::Step {
        queued: o.prs.len(),
        in_flight,
        land: plan.land.iter().map(|b| b.0).collect(),
        notify: plan
            .notify
            .iter()
            .map(|n| planned(n.pr.0, n.reason.to_string()))
            .collect(),
        start: plan
            .start
            .iter()
            .map(|s| PlannedStart {
                batch: s.id.0,
                members: s.prs.iter().map(|p| p.0).collect(),
            })
            .collect(),
        eject: plan
            .eject
            .iter()
            .map(|e| planned(e.pr.0, e.reason.to_string()))
            .collect(),
        retire: plan.retire.iter().map(|b| b.0).collect(),
        at,
        tick,
    }
}

#[cfg(test)]
mod tests;
