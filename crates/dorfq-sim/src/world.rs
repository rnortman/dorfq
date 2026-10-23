//! The hidden-truth world: pull requests, runners, events, and the
//! [`Forge`] a scheduler acts through.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;
use std::fmt;

use dorfq_core::{
    BatchId, BatchMember, BatchStart, BatchState, BatchStatus, CiStatus, ConflictWith, EjectReason,
    Forge, ForgeError, LandRefusal, Mergeable, Observation, PrNumber, PrSnapshot, Sha,
};

use crate::config::WorldConfig;
use crate::metrics::{Exit, Metrics, PrRecord, RunRecord};
use crate::rng::{SplitMix64, Streams, mix, unit};

const HOUR: f64 = crate::HOUR as f64;

/// A call that breaks rule 5 or rule 6: a scheduler bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    BatchIdInUse {
        batch: BatchId,
    },
    EmptyBatch {
        batch: BatchId,
    },
    DuplicateMember {
        batch: BatchId,
        pr: PrNumber,
    },
    MemberNotKnown {
        batch: BatchId,
        pr: PrNumber,
    },
    LandedInPlan {
        effect: &'static str,
        pr: PrNumber,
    },
    UnknownBase {
        batch: BatchId,
        base: Sha,
    },
    UnknownBatch {
        effect: &'static str,
        batch: BatchId,
    },
    NotPassed {
        batch: BatchId,
    },
    LandedTwice {
        batch: BatchId,
    },
    UnknownPr {
        pr: PrNumber,
    },
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

fn violation(v: Violation) -> ForgeError {
    ForgeError::Permanent(Box::new(v))
}

/// A scheduled happening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Arrival,
    CiDone(BatchId),
    Repush(PrNumber),
    Withdraw(PrNumber),
}

/// An event's place in the queue: its time, then scheduling order.
type EventKey = (u64, u64);

struct SimPr {
    record: PrRecord,
    head: Sha,
    bad: bool,
    conflicts_with_trunk: bool,
    key: u64,
    repush: Option<EventKey>,
    withdraw: Option<EventKey>,
}

impl SimPr {
    fn queued(&self) -> bool {
        self.record.left.is_none()
    }

    fn landed(&self) -> bool {
        matches!(self.record.left, Some((_, Exit::Landed)))
    }
}

/// A batch's claim on a runner.
enum Run {
    /// Conflicted while building: no run.
    None,
    Waiting {
        duration: u64,
    },
    Running {
        run: usize,
        done: EventKey,
    },
    Done {
        run: usize,
    },
}

struct SimBatch {
    status: BatchStatus,
    run: Run,
}

/// A commit: trunk's root, or a batch tip on its base.
struct Commit {
    parent: Option<Sha>,
    members: Vec<PrNumber>,
}

/// Effects performed since the latest observation, for rules 5 and 6.
#[derive(Default)]
struct Since {
    removed: BTreeSet<PrNumber>,
    started: BTreeSet<BatchId>,
    landed: BTreeSet<BatchId>,
    retired: BTreeSet<BatchId>,
}

/// The simulated forge and the truth behind it.
pub struct World {
    config: WorldConfig,
    streams: Streams,
    now: u64,
    prs: BTreeMap<PrNumber, SimPr>,
    batches: BTreeMap<BatchId, SimBatch>,
    commits: BTreeMap<Sha, Commit>,
    trunk: Sha,
    next_commit: u64,
    next_pr: u64,
    events: BTreeMap<EventKey, Event>,
    next_seq: u64,
    running: usize,
    waiting: VecDeque<BatchId>,
    runs: Vec<RunRecord>,
    latest: Option<Observation>,
    since: Since,
    /// Runner-seconds the runners have been busy.
    busy: u64,
    /// Event lines since the last [`World::take_trace`]; `None` untraced.
    trace: Option<Vec<String>>,
}

/// The synthetic commit numbered `n`.
fn commit(n: u64) -> Sha {
    Sha::parse(&format!("{n:040x}")).expect("40 lowercase hex digits")
}

/// An exponential delay in whole seconds at `per_hour`.
fn delay(rng: &mut SplitMix64, per_hour: f64) -> u64 {
    (rng.exponential(per_hour) * HOUR) as u64
}

impl World {
    /// A world at time 0 whose first arrival is drawn from `streams`.
    pub fn new(config: WorldConfig, streams: Streams) -> World {
        let trunk = commit(1);
        let root = Commit {
            parent: None,
            members: Vec::new(),
        };
        let mut world = World {
            config,
            streams,
            now: 0,
            prs: BTreeMap::new(),
            batches: BTreeMap::new(),
            commits: BTreeMap::from([(trunk.clone(), root)]),
            trunk,
            next_commit: 2,
            next_pr: 1,
            events: BTreeMap::new(),
            next_seq: 0,
            running: 0,
            waiting: VecDeque::new(),
            runs: Vec::new(),
            latest: None,
            since: Since::default(),
            busy: 0,
            trace: None,
        };
        world.schedule_arrival();
        world
    }

    /// This world, recording event lines for [`World::take_trace`].
    pub fn traced(mut self) -> World {
        self.trace = Some(Vec::new());
        self
    }

    /// The current time, in seconds.
    pub fn now(&self) -> u64 {
        self.now
    }

    /// The time of the next scheduled event.
    pub fn next_event(&self) -> Option<u64> {
        self.events.keys().next().map(|&(t, _)| t)
    }

    /// Moves the clock to `t` and fires every event due by then, in
    /// scheduling order.
    pub fn advance_to(&mut self, t: u64) {
        self.busy += self.running as u64 * (t - self.now);
        self.now = t;
        while let Some(entry) = self.events.first_entry() {
            if entry.key().0 > t {
                break;
            }
            let event = entry.remove();
            self.fire(event);
        }
    }

    /// The event lines written since the last call; none when untraced.
    pub fn take_trace(&mut self) -> Vec<String> {
        self.trace.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// Records an event line, formatting it only when traced.
    fn note(&mut self, line: impl FnOnce() -> String) {
        if let Some(trace) = &mut self.trace {
            trace.push(line());
        }
    }

    /// Every pull request that arrived, by number.
    pub fn pr_records(&self) -> Vec<PrRecord> {
        self.prs.values().map(|p| p.record.clone()).collect()
    }

    /// Every run that reached a runner, in start order.
    pub fn runs(&self) -> &[RunRecord] {
        &self.runs
    }

    /// The replication's summary as of now.
    pub fn metrics(&self) -> Metrics {
        Metrics::summarise(
            &self.pr_records(),
            &self.runs,
            self.running_elapsed(),
            self.config.runners,
            self.now,
        )
    }

    /// Runner-seconds used so far by the runs still running.
    fn running_elapsed(&self) -> u64 {
        self.batches
            .values()
            .filter_map(|b| match b.run {
                Run::Running { run, .. } => Some(self.now - self.runs[run].started_at),
                _ => None,
            })
            .sum()
    }

    /// Every pull request's arrival time and hidden truth (`bad`, conflicts
    /// with trunk, key), by number.
    #[cfg(test)]
    pub(crate) fn truths(&self) -> Vec<(u64, bool, bool, u64)> {
        let prs = self.prs.values();
        prs.map(|p| (p.record.arrived_at, p.bad, p.conflicts_with_trunk, p.key))
            .collect()
    }

    /// Whether `i` and `j` break only together: their keys say so and their
    /// queue times overlapped.
    pub fn interacts(&self, i: PrNumber, j: PrNumber) -> bool {
        let (a, b) = (&self.prs[&i], &self.prs[&j]);
        let left = |p: &SimPr| p.record.left.as_ref().map_or(self.now, |(t, _)| *t);
        i != j
            && unit(mix(a.key, b.key)) < self.config.p_interact
            && a.record.arrived_at < left(b)
            && b.record.arrived_at < left(a)
    }

    /// Checks the world's invariants: nothing bad landed, no two landed pull
    /// requests interact, trunk's tree is exactly the landed set, the runner
    /// time runs consumed is the time runners were busy, and no more runs
    /// hold runners than there are.
    pub fn check_invariants(&self) -> Result<(), String> {
        let landed: Vec<PrNumber> = self
            .prs
            .iter()
            .filter(|(_, p)| p.landed())
            .map(|(&n, _)| n)
            .collect();
        for (k, &i) in landed.iter().enumerate() {
            if self.prs[&i].bad {
                return Err(format!("pr {} landed but is bad", i.0));
            }
            if let Some(&j) = landed[k + 1..].iter().find(|&&j| self.interacts(i, j)) {
                return Err(format!("prs {} and {} landed but interact", i.0, j.0));
            }
        }
        let mut trunk_tree = self.tree(&self.trunk);
        trunk_tree.sort();
        if trunk_tree != landed {
            return Err("trunk's tree is not the landed set".to_owned());
        }
        let busy = self
            .batches
            .values()
            .filter(|b| matches!(b.run, Run::Running { .. }))
            .count();
        if busy != self.running || self.running > self.config.runners {
            return Err(format!(
                "{busy} batches running, {} runners held of {}",
                self.running, self.config.runners
            ));
        }
        let consumed: u64 = self.runs.iter().map(|r| r.consumed).sum();
        let elapsed = self.running_elapsed();
        if consumed + elapsed != self.busy {
            return Err(format!(
                "runs consumed {consumed}s and running runs {elapsed}s, runners were busy {}s",
                self.busy
            ));
        }
        Ok(())
    }

    /// Checks the pull requests of a batch just landed: none is bad, and
    /// none interacts with a landed pull request.
    fn check_landed(&self, members: &[BatchMember]) -> Result<(), String> {
        for m in members {
            let i = m.pr;
            if self.prs[&i].bad {
                return Err(format!("pr {} landed but is bad", i.0));
            }
            let landed = self.prs.iter().filter(|(_, p)| p.landed());
            if let Some((j, _)) = landed.into_iter().find(|&(&j, _)| self.interacts(i, j)) {
                return Err(format!("prs {} and {} landed but interact", i.0, j.0));
            }
        }
        Ok(())
    }

    fn schedule(&mut self, at: u64, event: Event) -> EventKey {
        let key = (at, self.next_seq);
        self.next_seq += 1;
        self.events.insert(key, event);
        key
    }

    fn schedule_arrival(&mut self) {
        if self.config.arrivals_per_hour <= 0.0 {
            return;
        }
        let delay = delay(&mut self.streams.arrivals, self.config.arrivals_per_hour);
        if let Some(at) = self.now.checked_add(delay)
            && at < self.config.hours * crate::HOUR
        {
            self.schedule(at, Event::Arrival);
        }
    }

    /// The time of the next firing of a churn clock at `per_hour`; `None`
    /// when the rate is 0 or the firing would reach the end of the clock,
    /// which no deadline does.
    fn churn_at(&mut self, per_hour: f64) -> Option<u64> {
        if per_hour <= 0.0 {
            return None;
        }
        self.now
            .checked_add(delay(&mut self.streams.churn, per_hour))
            .filter(|&at| at < u64::MAX)
    }

    fn schedule_repush(&mut self, pr: PrNumber) {
        if let Some(at) = self.churn_at(self.config.repush_per_hour) {
            let key = self.schedule(at, Event::Repush(pr));
            self.prs.get_mut(&pr).expect("arrived").repush = Some(key);
        }
    }

    fn schedule_withdraw(&mut self, pr: PrNumber) {
        if let Some(at) = self.churn_at(self.config.withdraw_per_hour) {
            let key = self.schedule(at, Event::Withdraw(pr));
            self.prs.get_mut(&pr).expect("arrived").withdraw = Some(key);
        }
    }

    fn mint(&mut self) -> Sha {
        let sha = commit(self.next_commit);
        self.next_commit += 1;
        sha
    }

    fn fire(&mut self, event: Event) {
        match event {
            Event::Arrival => {
                let number = PrNumber(self.next_pr);
                self.next_pr += 1;
                let bad = self.streams.truth.bernoulli(self.config.p_bad);
                let conflicts_with_trunk = self.streams.truth.bernoulli(self.config.p_conflict);
                let key = self.streams.truth.next_u64();
                let head = self.mint();
                let record = PrRecord {
                    number,
                    arrived_at: self.now,
                    left: None,
                };
                let pr = SimPr {
                    record,
                    head,
                    bad,
                    conflicts_with_trunk,
                    key,
                    repush: None,
                    withdraw: None,
                };
                self.prs.insert(number, pr);
                self.note(|| format!("arrive {}", number.0));
                self.schedule_repush(number);
                self.schedule_withdraw(number);
                self.schedule_arrival();
            }
            Event::CiDone(batch) => self.complete(batch),
            Event::Repush(pr) => {
                let head = self.mint();
                self.prs.get_mut(&pr).expect("arrived").head = head;
                self.note(|| format!("repush {}", pr.0));
                self.schedule_repush(pr);
            }
            Event::Withdraw(pr) => {
                self.leave(pr, Exit::Withdrawn);
                self.note(|| format!("withdraw {}", pr.0));
            }
        }
    }

    /// Records `pr` leaving now and cancels its churn clocks.
    fn leave(&mut self, pr: PrNumber, exit: Exit) {
        let now = self.now;
        let p = self.prs.get_mut(&pr).expect("arrived");
        p.record.left = Some((now, exit));
        let clocks = [p.repush.take(), p.withdraw.take()];
        for key in clocks.into_iter().flatten() {
            self.events.remove(&key);
        }
    }

    /// The tree members of `sha`: every own member on the walk to the root.
    fn tree(&self, sha: &Sha) -> Vec<PrNumber> {
        let mut members = Vec::new();
        let mut at = Some(sha);
        while let Some(sha) = at {
            let c = &self.commits[sha];
            members.extend(&c.members);
            at = c.parent.as_ref();
        }
        members
    }

    /// Whether a run of `tree` fails for its content: some member not yet
    /// landed is bad, or interacts with another tree member.
    fn broken(&self, tree: &[PrNumber]) -> bool {
        tree.iter()
            .filter(|i| !self.prs[i].landed())
            .any(|&i| self.prs[&i].bad || tree.iter().any(|&j| self.interacts(i, j)))
    }

    fn start_run(&mut self, batch: BatchId, duration: u64) {
        let members = self.batches[&batch].status.members.len();
        let done = self.schedule(self.now + duration, Event::CiDone(batch));
        self.runs.push(RunRecord {
            batch,
            members,
            started_at: self.now,
            consumed: 0,
            verdict: None,
            tree: Vec::new(),
            landed: false,
        });
        let run = self.runs.len() - 1;
        self.batches.get_mut(&batch).expect("tracked").run = Run::Running { run, done };
        self.running += 1;
        debug_assert!(self.running <= self.config.runners);
    }

    /// Gives every free runner to the head of the runner queue.
    fn assign_runners(&mut self) {
        while self.running < self.config.runners {
            let Some(next) = self.waiting.pop_front() else {
                break;
            };
            let Run::Waiting { duration } = self.batches[&next].run else {
                unreachable!("a queued batch is waiting");
            };
            self.start_run(next, duration);
        }
    }

    fn complete(&mut self, batch: BatchId) {
        let b = &self.batches[&batch];
        let Run::Running { run, .. } = b.run else {
            unreachable!("a completing batch is running");
        };
        let BatchState::Testing { tip, .. } = b.status.state.clone() else {
            unreachable!("a running batch is testing");
        };
        let flake = self.streams.ci.bernoulli(self.config.p_flake);
        let tree = self.tree(&tip);
        let ci = if flake || self.broken(&tree) {
            CiStatus::Failed
        } else {
            CiStatus::Passed
        };
        let beyond_trunk = tree.into_iter().filter(|i| !self.prs[i].landed());
        let tree = beyond_trunk.collect();
        let record = &mut self.runs[run];
        record.consumed = self.now - record.started_at;
        record.verdict = Some(ci);
        record.tree = tree;
        let b = self.batches.get_mut(&batch).expect("tracked");
        b.status.state = BatchState::Testing { tip, ci };
        b.run = Run::Done { run };
        let verdict = if ci == CiStatus::Passed {
            "passed"
        } else {
            "failed"
        };
        self.note(|| format!("ci {} {verdict}", batch.0));
        self.running -= 1;
        self.assign_runners();
    }

    fn snapshot(&self) -> Observation {
        let prs = self
            .prs
            .values()
            .filter(|p| p.queued())
            .map(|p| PrSnapshot {
                number: p.record.number,
                head: p.head.clone(),
                mergeable: if p.conflicts_with_trunk {
                    Mergeable::Conflicting
                } else {
                    Mergeable::Clean
                },
                head_ci: CiStatus::Passed,
            })
            .collect();
        Observation {
            trunk: self.trunk.clone(),
            prs,
            batches: self.batches.values().map(|b| b.status.clone()).collect(),
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
        let observed = self
            .latest
            .as_ref()
            .is_some_and(|o| o.prs.iter().any(|p| p.number == pr));
        observed && !self.since.removed.contains(&pr)
    }

    fn in_landing(&self, pr: PrNumber) -> bool {
        self.since.landed.iter().any(|&b| {
            self.observed_batch(b)
                .is_some_and(|s| s.members.iter().any(|m| m.pr == pr))
        })
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

    fn refusal(&self, status: &BatchStatus) -> Option<LandRefusal> {
        if status.base != self.trunk {
            return Some(LandRefusal::TrunkMoved {
                trunk: self.trunk.clone(),
            });
        }
        status.members.iter().find_map(|m| {
            let p = &self.prs[&m.pr];
            let still_there = p.queued() && p.head == m.head;
            (!still_there).then_some(LandRefusal::MemberGone { pr: m.pr })
        })
    }

    /// Whether ejecting `pr` now is justified by the truth.
    fn justified(&self, pr: PrNumber) -> bool {
        let p = &self.prs[&pr];
        p.bad
            || p.conflicts_with_trunk
            || self
                .prs
                .iter()
                .any(|(&j, other)| (other.queued() || other.landed()) && self.interacts(pr, j))
    }
}

impl Forge for World {
    fn observe(&mut self) -> Result<Observation, ForgeError> {
        let observation = self.snapshot();
        self.latest = Some(observation.clone());
        self.since = Since::default();
        Ok(observation)
    }

    fn start_batch(&mut self, batch: &BatchStart) -> Result<(), ForgeError> {
        self.check_start(batch).map_err(violation)?;
        let members: Vec<BatchMember> = batch
            .prs
            .iter()
            .map(|&pr| BatchMember {
                pr,
                head: self.prs[&pr].head.clone(),
            })
            .collect();
        let conflict = batch
            .prs
            .iter()
            .find(|pr| self.prs[pr].conflicts_with_trunk);
        let (state, run) = match conflict {
            Some(&pr) => {
                let with = ConflictWith::Trunk;
                (BatchState::Conflict { pr, with }, Run::None)
            }
            None => {
                let tip = self.mint();
                let tip_commit = Commit {
                    parent: Some(batch.base.clone()),
                    members: batch.prs.clone(),
                };
                self.commits.insert(tip.clone(), tip_commit);
                let (min, max) = (self.config.ci_minutes.min, self.config.ci_minutes.max);
                let duration = self.streams.ci.uniform(min, max) * 60;
                let ci = CiStatus::Pending;
                (BatchState::Testing { tip, ci }, Run::Waiting { duration })
            }
        };
        let waiting = matches!(run, Run::Waiting { .. });
        let status = BatchStatus {
            id: batch.id,
            base: batch.base.clone(),
            members,
            state,
        };
        self.batches.insert(batch.id, SimBatch { status, run });
        self.since.started.insert(batch.id);
        if waiting {
            self.waiting.push_back(batch.id);
            self.assign_runners();
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
        let observed = self.observed_batch(batch).expect("known");
        let BatchState::Testing {
            tip,
            ci: CiStatus::Passed,
        } = observed.state.clone()
        else {
            return Err(violation(Violation::NotPassed { batch }));
        };
        self.since.landed.insert(batch);
        let status = self.batches[&batch].status.clone();
        let refusal = self.refusal(&status);
        let b = self.batches.get_mut(&batch).expect("known");
        if let Some(reason) = refusal {
            b.status.state = BatchState::LandRefused { tip, reason };
            return Ok(());
        }
        b.status.state = BatchState::Landed { tip: tip.clone() };
        if let Run::Done { run } = b.run {
            self.runs[run].landed = true;
        }
        self.trunk = tip;
        for m in &status.members {
            self.leave(m.pr, Exit::Landed);
            self.since.removed.insert(m.pr);
        }
        debug_assert_eq!(self.check_landed(&status.members), Ok(()));
        Ok(())
    }

    fn retire(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        if !self.batch_known(batch) {
            return Err(violation(Violation::UnknownBatch {
                effect: "retire",
                batch,
            }));
        }
        let b = self.batches.remove(&batch).expect("known");
        self.since.retired.insert(batch);
        match b.run {
            Run::Waiting { .. } => self.waiting.retain(|&w| w != batch),
            Run::Running { run, done } => {
                self.events.remove(&done);
                self.runs[run].consumed = self.now - self.runs[run].started_at;
                self.running -= 1;
                self.assign_runners();
            }
            Run::None | Run::Done { .. } => {}
        }
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
        let justified = self.justified(pr);
        let reason = reason.clone();
        self.leave(pr, Exit::Ejected { reason, justified });
        self.since.removed.insert(pr);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
