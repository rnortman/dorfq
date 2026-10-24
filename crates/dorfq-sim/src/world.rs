//! The hidden-truth world: pull requests, runners, events, and the
//! [`Forge`] a scheduler acts through.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use dorfq_core::ledger::Ledger;
use dorfq_core::{
    BatchId, BatchMember, BatchStart, BatchState, BatchStatus, CiStatus, ConflictWith, Departed,
    EjectReason, Failure, Forge, ForgeError, Hold, LandRefusal, Observation, PrBase, PrNumber,
    PrSnapshot, PrStatus, Sha, StackPolicy, Timestamp, Withdrawal,
};

use crate::config::{Admission, WorldConfig};
use crate::metrics::{Exit, Metrics, PrCiRecord, PrRecord, RunRecord, RunningSeconds};
use crate::rng::{SplitMix64, Streams, mix, unit};
use crate::trace::{Rebase, TraceEvent, Truth};

const HOUR: f64 = crate::HOUR as f64;

/// Work for a runner: a batch's run, or a pull request's own CI on its
/// current head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    Batch(BatchId),
    PrCi(PrNumber),
}

/// A scheduled happening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Arrival,
    CiDone(Job),
    Repush(PrNumber),
    Withdraw(PrNumber),
    /// The author pushes the rebase.
    Rebase(PrNumber),
}

/// An event's place in the queue: its time, then scheduling order.
type EventKey = (u64, u64);

/// Why a present pull request is not in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Out {
    /// Its author is rebasing it, for `why`; `push` is the scheduled push.
    Rebasing { why: Rebase, push: EventKey },
    /// `label`: out with its base, which is rebasing; restacked at the push.
    WithBase { on: PrNumber },
    /// `github`: its own CI runs on its current head; it enters when that
    /// passes.
    Checking,
    /// `github`: stacked on a pull request that has not landed.
    Stacked,
}

struct SimPr {
    record: PrRecord,
    head: Sha,
    bad: bool,
    conflicts_with_trunk: bool,
    key: u64,
    /// The earlier arrival whose branch it was cut from, whether or not it
    /// stacked on it.
    cut_from: Option<PrNumber>,
    /// Whether it stacked on `cut_from`: that one was in the world at
    /// arrival.
    stacked: bool,
    /// Lands only with its ancestors; only ever set on a stacked arrival.
    atomic: bool,
    repush: Option<EventKey>,
    withdraw: Option<EventKey>,
    /// Its own clock, seeded from `key`: every PR CI duration and rebase
    /// delay it draws.
    own: SplitMix64,
    /// Its own CI on its current head, from the request to the run's end.
    ci: Option<Run>,
    /// Why the queue does not see it while it is in the world; `None` when
    /// it does.
    out: Option<Out>,
}

impl SimPr {
    /// The pull request it stacked on at arrival; never changes.
    fn base(&self) -> Option<PrNumber> {
        self.cut_from.filter(|_| self.stacked)
    }

    /// In the world: arrived and not yet left for good.
    fn present(&self) -> bool {
        self.record.left.is_none()
    }

    /// Present and seen by the queue.
    fn enqueued(&self) -> bool {
        self.present() && self.out.is_none()
    }

    fn landed(&self) -> bool {
        matches!(self.record.left, Some((_, Exit::Landed)))
    }
}

/// A run's claim on a runner. A pull request's own run never reaches
/// `Done`: it is dropped when it ends.
enum Run {
    Waiting { duration: u64 },
    Running { run: usize, done: EventKey },
    Done { run: usize },
}

struct SimBatch {
    status: BatchStatus,
    run: Run,
}

/// A commit: trunk's root, or a batch tip on its base.
struct Commit {
    parent: Option<Sha>,
    members: Vec<PrNumber>,
    /// The batch whose tip it is; `None` for the root.
    batch: Option<BatchId>,
}

/// The simulated forge and the truth behind it.
pub struct World {
    config: WorldConfig,
    admission: Admission,
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
    waiting: VecDeque<Job>,
    runs: Vec<RunRecord>,
    pr_ci_runs: Vec<PrCiRecord>,
    /// Rules 5, 6 and 8, against the latest observation.
    ledger: Ledger,
    /// Runner-seconds the runners have been busy.
    busy: u64,
    /// Events since the last [`World::take_trace`]; `None` untraced.
    trace: Option<Vec<TraceEvent>>,
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
    /// A world at time 0 under `admission` whose first arrival is drawn
    /// from `streams`.
    pub fn new(config: WorldConfig, admission: Admission, streams: Streams) -> World {
        let trunk = commit(1);
        let root = Commit {
            parent: None,
            members: Vec::new(),
            batch: None,
        };
        let mut world = World {
            config,
            admission,
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
            pr_ci_runs: Vec::new(),
            ledger: Ledger::new(),
            busy: 0,
            trace: None,
        };
        world.schedule_arrival();
        world
    }

    /// This world, recording events for [`World::take_trace`].
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

    /// The events recorded since the last call; none when untraced.
    pub fn take_trace(&mut self) -> Vec<TraceEvent> {
        self.trace.as_mut().map(std::mem::take).unwrap_or_default()
    }

    /// Records an event at the current time, building it only when traced.
    fn note(&mut self, event: impl FnOnce(u64) -> TraceEvent) {
        let t = self.now;
        if let Some(trace) = &mut self.trace {
            trace.push(event(t));
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

    /// Every pull request's own CI run that reached a runner, in start
    /// order.
    pub fn pr_ci_runs(&self) -> &[PrCiRecord] {
        &self.pr_ci_runs
    }

    /// The replication's summary as of now.
    pub fn metrics(&self) -> Metrics {
        Metrics::summarise(
            &self.pr_records(),
            &self.runs,
            &self.pr_ci_runs,
            self.running_elapsed(),
            self.config.runners,
            self.now,
        )
    }

    /// Runner-seconds used so far by the runs still running.
    fn running_elapsed(&self) -> RunningSeconds {
        let batches = self.batches.values().filter_map(|b| match b.run {
            Run::Running { run, .. } => Some(self.now - self.runs[run].started_at),
            _ => None,
        });
        let prs = self.prs.values().filter_map(|p| match p.ci {
            Some(Run::Running { run, .. }) => Some(self.now - self.pr_ci_runs[run].started_at),
            _ => None,
        });
        RunningSeconds {
            batches: batches.sum(),
            pr_ci: prs.sum(),
        }
    }

    /// Present pull requests the queue does not see.
    pub fn out(&self) -> usize {
        self.prs
            .values()
            .filter(|p| p.present() && !p.enqueued())
            .count()
    }

    /// Every pull request's arrival time, hidden truth (`bad`, conflicts
    /// with trunk, key) and the arrival it was cut from, by number.
    #[cfg(test)]
    pub(crate) fn truths(&self) -> Vec<(u64, bool, bool, u64, Option<PrNumber>)> {
        let prs = self.prs.values();
        prs.map(|p| {
            let truth = (p.bad, p.conflicts_with_trunk, p.key);
            (p.record.arrived_at, truth.0, truth.1, truth.2, p.cut_from)
        })
        .collect()
    }

    /// Whether `i` and `j` break only together: their keys say so, their
    /// queue times overlapped, and neither is stacked on the other.
    pub fn interacts(&self, i: PrNumber, j: PrNumber) -> bool {
        let (a, b) = (&self.prs[&i], &self.prs[&j]);
        let left = |p: &SimPr| p.record.left.as_ref().map_or(self.now, |(t, _)| *t);
        i != j
            && unit(mix(a.key, b.key)) < self.config.p_interact
            && a.record.arrived_at < left(b)
            && b.record.arrived_at < left(a)
            && !self.descends(i, j)
            && !self.descends(j, i)
    }

    /// Whether `ancestor` is on `pr`'s chain of original bases.
    fn descends(&self, pr: PrNumber, ancestor: PrNumber) -> bool {
        let mut at = self.prs[&pr].base();
        while let Some(base) = at {
            if base == ancestor {
                return true;
            }
            at = self.prs[&base].base();
        }
        false
    }

    /// The present pull requests stacked on `pr`, directly or not, by
    /// number.
    fn descendants(&self, pr: PrNumber) -> Vec<PrNumber> {
        let present = self.prs.iter().filter(|(_, p)| p.present());
        present
            .filter(|&(&n, _)| self.descends(n, pr))
            .map(|(&n, _)| n)
            .collect()
    }

    /// Checks the world's invariants: nothing bad landed, no two landed pull
    /// requests interact, a landed pull request's base landed at or before
    /// it, trunk's tree is exactly the landed set, the runner time batch
    /// and PR CI runs consumed is the time runners were busy, no more runs
    /// of either kind hold runners than there are, and every present pull
    /// request's place agrees with its admission (see `check_places`).
    pub fn check_invariants(&self) -> Result<(), String> {
        self.check_places()?;
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
            if let Some(base) = self.prs[&i].base() {
                let left_at = |n: PrNumber| self.prs[&n].record.left.as_ref().map(|(t, _)| *t);
                if !self.prs[&base].landed() || left_at(base) > left_at(i) {
                    return Err(format!(
                        "pr {} landed but its base {} did not land before it",
                        i.0, base.0
                    ));
                }
            }
        }
        let mut trunk_tree = self.tree(&self.trunk);
        trunk_tree.sort();
        if trunk_tree != landed {
            return Err("trunk's tree is not the landed set".to_owned());
        }
        let batches = self.batches.values();
        let busy = batches.filter(|b| matches!(b.run, Run::Running { .. }));
        let prs = self.prs.values();
        let pr_ci = prs.filter(|p| matches!(p.ci, Some(Run::Running { .. })));
        let (busy, pr_ci) = (busy.count(), pr_ci.count());
        if busy + pr_ci != self.running || self.running > self.config.runners {
            return Err(format!(
                "{busy} batches and {pr_ci} PR CI runs running, {} runners held of {}",
                self.running, self.config.runners
            ));
        }
        let consumed: u64 = self.runs.iter().map(|r| r.consumed).sum::<u64>()
            + self.pr_ci_runs.iter().map(|r| r.consumed).sum::<u64>();
        let running = self.running_elapsed();
        let elapsed = running.batches + running.pr_ci;
        if consumed + elapsed != self.busy {
            return Err(format!(
                "runs consumed {consumed}s and running runs {elapsed}s, runners were busy {}s",
                self.busy
            ));
        }
        Ok(())
    }

    /// Checks where the present pull requests are: an enqueued one's base is
    /// enqueued or landed; one out with its base has a base that is out;
    /// under `github` an enqueued one has no run in flight and does not
    /// conflict, and a stacked one has no run in flight.
    fn check_places(&self) -> Result<(), String> {
        let github = self.admission == Admission::Github;
        for (n, p) in self.prs.iter().filter(|(_, p)| p.present()) {
            let n = n.0;
            match p.out {
                None => {
                    if let Some(b) = p.base()
                        && !(self.prs[&b].enqueued() || self.prs[&b].landed())
                    {
                        return Err(format!("pr {n} is enqueued but its base {} is not", b.0));
                    }
                    if github && (p.ci.is_some() || p.conflicts_with_trunk) {
                        return Err(format!("pr {n} is enqueued unchecked"));
                    }
                }
                Some(Out::WithBase { on }) if self.prs[&on].enqueued() => {
                    return Err(format!("pr {n} is out with its base {}, which is in", on.0));
                }
                Some(Out::Stacked) if p.ci.is_some() => {
                    return Err(format!("pr {n} waits for its base with a run in flight"));
                }
                Some(_) => {}
            }
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
                let conflicts = self.streams.truth.bernoulli(self.config.p_conflict);
                let key = self.streams.truth.next_u64();
                // Every draw is taken whatever the queue holds, so the draws
                // are the same under every policy.
                let cut = self.streams.stacks.bernoulli(self.config.p_stacked);
                // The k-th most recent arrival, with probability 2^-k; the
                // exponential is at most 53 from a 53-bit draw.
                let back = 1 + self.streams.stacks.exponential(std::f64::consts::LN_2) as u64;
                let atomic = self.streams.stacks.bernoulli(self.config.p_atomic);
                let cut_from = (cut && back < number.0).then(|| PrNumber(number.0 - back));
                let stacked = cut_from.is_some_and(|b| self.prs[&b].present());
                let head = self.mint();
                let record = PrRecord {
                    number,
                    arrived_at: self.now,
                    left: None,
                    rebases: 0,
                };
                let pr = SimPr {
                    record,
                    head,
                    bad,
                    // A branch cut from another cannot conflict with it, and
                    // whatever of it is on trunk is that branch's content.
                    conflicts_with_trunk: conflicts && cut_from.is_none(),
                    key,
                    cut_from,
                    stacked,
                    atomic: atomic && stacked,
                    repush: None,
                    withdraw: None,
                    own: SplitMix64::new(key),
                    ci: None,
                    out: None,
                };
                let (base, atomic) = (pr.base(), pr.atomic);
                let truth = Truth {
                    bad,
                    conflicts: pr.conflicts_with_trunk,
                };
                self.prs.insert(number, pr);
                self.note(|t| TraceEvent::Arrive {
                    t,
                    pr: number.0,
                    base: base.map(|b| b.0),
                    atomic,
                    truth,
                });
                self.request_pr_ci(number);
                match (self.admission, base) {
                    (Admission::Github, _) => self.set_out(number, Some(Out::Checking)),
                    (Admission::Label, Some(b)) if !self.prs[&b].enqueued() => {
                        self.out_with_base(number);
                    }
                    (Admission::Label, _) => self.admit(number),
                }
                self.assign_runners();
                self.schedule_repush(number);
                self.schedule_withdraw(number);
                self.schedule_arrival();
            }
            Event::CiDone(Job::Batch(batch)) => self.complete(batch),
            Event::CiDone(Job::PrCi(pr)) => self.complete_pr_ci(pr),
            Event::Repush(pr) => {
                // Its author is mid-rebase, its own or its base's: no head.
                let rebasing = matches!(
                    self.prs[&pr].out,
                    Some(Out::Rebasing { .. } | Out::WithBase { .. })
                );
                if !rebasing {
                    let pushed = self.push(pr);
                    if self.admission == Admission::Github {
                        for p in pushed {
                            let was_in = self.prs[&p].enqueued();
                            self.set_out(p, Some(Out::Checking));
                            if was_in {
                                self.note(|t| TraceEvent::Recheck { t, pr: p.0 });
                            }
                        }
                    }
                    self.assign_runners();
                }
                self.schedule_repush(pr);
            }
            Event::Rebase(pr) => {
                let p = self.prs.get_mut(&pr).expect("arrived");
                debug_assert!(matches!(p.out, Some(Out::Rebasing { .. })));
                p.conflicts_with_trunk = false;
                p.record.rebases += 1;
                let pushed = self.push(pr);
                for p in pushed {
                    match self.admission {
                        Admission::Label => self.admit(p),
                        Admission::Github => self.set_out(p, Some(Out::Checking)),
                    }
                }
                self.assign_runners();
            }
            Event::Withdraw(pr) => {
                self.note(|t| TraceEvent::Withdraw { t, pr: pr.0 });
                self.leave(pr, Exit::Withdrawn);
            }
        }
    }

    /// Gives `pr` and every present pull request stacked on it a new head,
    /// each with its own CI requested, and returns them, `pr` first. The
    /// caller assigns runners.
    fn push(&mut self, pr: PrNumber) -> Vec<PrNumber> {
        let pushed: Vec<PrNumber> = std::iter::once(pr).chain(self.descendants(pr)).collect();
        for &p in &pushed {
            let head = self.mint();
            self.prs.get_mut(&p).expect("arrived").head = head;
            self.note(|t| TraceEvent::Repush { t, pr: p.0 });
            self.request_pr_ci(p);
        }
        pushed
    }

    fn set_out(&mut self, pr: PrNumber, out: Option<Out>) {
        self.prs.get_mut(&pr).expect("arrived").out = out;
    }

    /// The queue sees `pr` from now.
    fn admit(&mut self, pr: PrNumber) {
        self.set_out(pr, None);
        let base = self.prs[&pr].base().filter(|b| self.prs[b].enqueued());
        self.note(|t| TraceEvent::Admit {
            t,
            pr: pr.0,
            base: base.map(|b| b.0),
        });
    }

    /// `label`: `pr` goes out with its base, which is out, until the push
    /// of the rebase below it.
    fn out_with_base(&mut self, pr: PrNumber) {
        let on = self.prs[&pr].base().expect("stacked");
        self.set_out(pr, Some(Out::WithBase { on }));
        self.note(|t| TraceEvent::Blocked {
            t,
            pr: pr.0,
            on: on.0,
            r#final: false,
        });
    }

    /// `pr`'s author starts rebasing it for `why`; the push is drawn from
    /// its own clock.
    fn begin_rebase(&mut self, pr: PrNumber, why: Rebase) {
        let (min, max) = (
            self.config.rebase_minutes.min,
            self.config.rebase_minutes.max,
        );
        let delay = self
            .prs
            .get_mut(&pr)
            .expect("arrived")
            .own
            .uniform(min, max)
            * 60;
        let push = self.schedule(self.now + delay, Event::Rebase(pr));
        self.set_out(pr, Some(Out::Rebasing { why, push }));
        self.note(|t| TraceEvent::Rebase { t, pr: pr.0, why });
    }

    /// An ejection in the same plan supersedes a block of `pr` earlier in
    /// this instant, final or not: the block's `blocked` event is taken
    /// back out of the trace, so every pull request departs once.
    fn untrace_blocked(&mut self, pr: PrNumber) {
        let now = self.now;
        if let Some(trace) = &mut self.trace {
            trace.retain(
                |e| !matches!(e, TraceEvent::Blocked { t, pr: p, .. } if *t == now && *p == pr.0),
            );
        }
    }

    /// Records `pr` leaving now for good and cancels its churn clocks and
    /// any pending rebase push; its own CI run is not cancelled. Unless it
    /// landed, every present pull request stacked on it leaves `Blocked` on
    /// it, recursively. A pull request blocked earlier in this instant may
    /// leave again, superseding the block (see [`Self::untrace_blocked`]).
    fn leave(&mut self, pr: PrNumber, exit: Exit) {
        let now = self.now;
        let cascades = exit != Exit::Landed;
        let p = self.prs.get_mut(&pr).expect("arrived");
        let superseded = matches!(&p.record.left, Some((t, Exit::Blocked { .. })) if *t == now);
        debug_assert!(
            p.record.left.is_none() || superseded,
            "pr {} leaves twice",
            pr.0
        );
        p.record.left = Some((now, exit));
        let push = match p.out.take() {
            Some(Out::Rebasing { push, .. }) => Some(push),
            _ => None,
        };
        let clocks = [p.repush.take(), p.withdraw.take(), push];
        if superseded {
            self.untrace_blocked(pr);
        }
        for key in clocks.into_iter().flatten() {
            self.events.remove(&key);
        }
        if cascades {
            let children: Vec<PrNumber> = self
                .prs
                .iter()
                .filter(|(_, c)| c.present() && c.base() == Some(pr))
                .map(|(&n, _)| n)
                .collect();
            for child in children {
                self.note(|t| TraceEvent::Blocked {
                    t,
                    pr: child.0,
                    on: pr.0,
                    r#final: true,
                });
                self.leave(child, Exit::Blocked { on: pr });
            }
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
        let done = self.schedule(self.now + duration, Event::CiDone(Job::Batch(batch)));
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
        self.note(|t| TraceEvent::RunBegin { t, batch: batch.0 });
        self.running += 1;
        debug_assert!(self.running <= self.config.runners);
    }

    /// Cancels `pr`'s own CI run, waiting or running, and requests one for
    /// its current head, drawing the duration from its own clock. The
    /// caller assigns runners.
    fn request_pr_ci(&mut self, pr: PrNumber) {
        self.cancel_pr_ci(pr);
        let (min, max) = (self.config.pr_ci_minutes.min, self.config.pr_ci_minutes.max);
        let p = self.prs.get_mut(&pr).expect("arrived");
        let duration = p.own.uniform(min, max) * 60;
        p.ci = Some(Run::Waiting { duration });
        self.waiting.push_back(Job::PrCi(pr));
        self.note(|t| TraceEvent::PrCi { t, pr: pr.0 });
    }

    /// Drops `pr`'s own CI run if it has one: off the runner queue, or cut
    /// short with its elapsed time charged. The caller assigns runners.
    fn cancel_pr_ci(&mut self, pr: PrNumber) {
        let Some(run) = self.prs.get_mut(&pr).expect("arrived").ci.take() else {
            return;
        };
        match run {
            Run::Waiting { .. } => self.waiting.retain(|&j| j != Job::PrCi(pr)),
            Run::Running { run, done } => {
                self.events.remove(&done);
                let record = &mut self.pr_ci_runs[run];
                record.consumed = self.now - record.started_at;
                record.cancelled = true;
                self.running -= 1;
            }
            Run::Done { .. } => unreachable!("a pull request's run is dropped when it ends"),
        }
        self.note(|t| TraceEvent::PrCiEnd {
            t,
            pr: pr.0,
            cancelled: true,
        });
    }

    fn start_pr_ci(&mut self, pr: PrNumber, duration: u64) {
        let done = self.schedule(self.now + duration, Event::CiDone(Job::PrCi(pr)));
        let p = self.prs.get_mut(&pr).expect("arrived");
        self.pr_ci_runs.push(PrCiRecord {
            pr,
            head: p.head.clone(),
            started_at: self.now,
            consumed: 0,
            cancelled: false,
        });
        let run = self.pr_ci_runs.len() - 1;
        p.ci = Some(Run::Running { run, done });
        self.note(|t| TraceEvent::PrCiBegin { t, pr: pr.0 });
        self.running += 1;
        debug_assert!(self.running <= self.config.runners);
    }

    /// Ends `pr`'s own CI run: it passed. Whether the pull request is still
    /// in the world does not matter; the run was paid for. Under `github` a
    /// pull request checking goes on: out for a rebase if it conflicts,
    /// waiting if its base is in the world, else into the queue.
    fn complete_pr_ci(&mut self, pr: PrNumber) {
        let p = self.prs.get_mut(&pr).expect("arrived");
        let Some(Run::Running { run, .. }) = p.ci.take() else {
            unreachable!("a completing PR CI run is running");
        };
        let record = &mut self.pr_ci_runs[run];
        record.consumed = self.now - record.started_at;
        self.note(|t| TraceEvent::PrCiEnd {
            t,
            pr: pr.0,
            cancelled: false,
        });
        self.running -= 1;
        let p = &self.prs[&pr];
        if p.present() && p.out == Some(Out::Checking) {
            if p.conflicts_with_trunk {
                self.begin_rebase(pr, Rebase::Conflict);
            } else if let Some(on) = p.base().filter(|b| self.prs[b].present()) {
                self.set_out(pr, Some(Out::Stacked));
                self.note(|t| TraceEvent::Wait {
                    t,
                    pr: pr.0,
                    on: on.0,
                });
            } else {
                self.admit(pr);
            }
        }
        self.assign_runners();
    }

    /// Gives every free runner to the head of the runner queue.
    fn assign_runners(&mut self) {
        while self.running < self.config.runners {
            let Some(next) = self.waiting.pop_front() else {
                break;
            };
            match next {
                Job::Batch(batch) => {
                    let Run::Waiting { duration } = self.batches[&batch].run else {
                        unreachable!("a queued batch is waiting");
                    };
                    self.start_run(batch, duration);
                }
                Job::PrCi(pr) => {
                    let Some(Run::Waiting { duration }) = self.prs[&pr].ci else {
                        unreachable!("a queued PR CI run is waiting");
                    };
                    self.start_pr_ci(pr, duration);
                }
            }
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
        let broken = self.broken(&tree);
        let ci = if flake || broken {
            // The world has no check names, so a failure carries none.
            CiStatus::Failed(Failure::new(BTreeSet::new()))
        } else {
            CiStatus::Passed
        };
        let beyond_trunk = tree.into_iter().filter(|i| !self.prs[i].landed());
        let tree = beyond_trunk.collect();
        let record = &mut self.runs[run];
        record.consumed = self.now - record.started_at;
        let passed = ci == CiStatus::Passed;
        record.verdict = Some(ci.clone());
        record.tree = tree;
        let b = self.batches.get_mut(&batch).expect("tracked");
        b.status.state = BatchState::Testing { tip, ci };
        b.run = Run::Done { run };
        self.note(|t| TraceEvent::RunEnd {
            t,
            batch: batch.0,
            passed,
            broken,
            flake,
        });
        self.running -= 1;
        self.assign_runners();
    }

    /// The observation now: the enqueued pull requests. One conflicting
    /// with trunk is reported so, as the forge reports it whatever else
    /// holds it; one whose own CI on its current head has not passed is
    /// held `OwnCiPending`. A departure the queue did not cause is a
    /// withdrawal: of intent, or because a base left or went out.
    fn snapshot(&self) -> Result<Observation, ForgeError> {
        let prs: Vec<PrSnapshot> = self
            .prs
            .values()
            .filter(|p| p.enqueued())
            .map(|p| {
                let status = if p.conflicts_with_trunk {
                    PrStatus::Conflicting(ConflictWith::Trunk)
                } else if p.ci.is_some() {
                    PrStatus::Waiting(BTreeSet::from([Hold::OwnCiPending]))
                } else {
                    PrStatus::Ready
                };
                // An enqueued pull request's base is enqueued or landed.
                let base = match p.base() {
                    Some(b) if self.prs[&b].enqueued() => PrBase::Pr(b),
                    _ => PrBase::Trunk,
                };
                let snapshot = PrSnapshot::new(p.record.number, p.head.clone(), base, status);
                if p.atomic {
                    snapshot.with_stack(StackPolicy::Atomic)
                } else {
                    snapshot
                }
            })
            .collect();
        let batches: Vec<BatchStatus> = self.batches.values().map(|b| b.status.clone()).collect();
        let departed = self.ledger.departures(&prs, &batches, |gone| {
            let p = &self.prs[&gone.number];
            match (&p.record.left, p.out) {
                (Some((_, Exit::Withdrawn)), _) | (None, Some(Out::Checking)) => {
                    Ok(Departed::Withdrawn(Withdrawal::IntentCleared))
                }
                (Some((_, Exit::Blocked { on })), _) => {
                    Ok(Departed::Withdrawn(Withdrawal::Blocked { on: *on }))
                }
                (None, Some(Out::WithBase { on })) => {
                    Ok(Departed::Withdrawn(Withdrawal::Blocked { on }))
                }
                (left, out) => Err(ForgeError::Permanent(
                    format!(
                        "pr {} left as {left:?}, out as {out:?}, with no batch landed or \
                         ejection to show for it",
                        gone.number.0
                    )
                    .into(),
                )),
            }
        })?;
        Ok(Observation::new(
            Timestamp(self.now),
            self.trunk.clone(),
            prs,
            batches,
            departed,
        ))
    }

    fn refusal(&self, status: &BatchStatus) -> Option<LandRefusal> {
        if status.base != self.trunk {
            return Some(LandRefusal::TrunkMoved {
                trunk: self.trunk.clone(),
            });
        }
        status.members.iter().find_map(|m| {
            let p = &self.prs[&m.pr];
            let still_there = p.enqueued() && p.head == m.head;
            (!still_there).then_some(LandRefusal::MemberGone { pr: m.pr })
        })
    }

    /// Whether ejecting `pr` for good now is justified by the truth: it is
    /// bad or interacts with a pull request in the world or landed. A
    /// conflict does not justify it; a conflict is a rebase.
    fn justified(&self, pr: PrNumber) -> bool {
        self.prs[&pr].bad
            || self
                .prs
                .iter()
                .any(|(&j, other)| (other.present() || other.landed()) && self.interacts(pr, j))
    }
}

impl Forge for World {
    fn observe(&mut self) -> Result<Observation, ForgeError> {
        let observation = self.snapshot()?;
        self.ledger.observed(&observation)?;
        Ok(observation)
    }

    fn start_batch(&mut self, batch: &BatchStart) -> Result<(), ForgeError> {
        self.ledger.check_start(batch)?;
        let members: Vec<BatchMember> = batch
            .prs
            .iter()
            .map(|&pr| BatchMember::new(pr, self.prs[&pr].head.clone()))
            .collect();
        // Every build succeeds: a valid plan never starts a member that
        // conflicts with trunk, since it is never `Ready`, and the world has
        // no conflicts between members. A probe's tip adds no members, so
        // its run tests the base's tree alone.
        let tip = self.mint();
        let tip_commit = Commit {
            parent: Some(batch.base.clone()),
            members: batch.prs.clone(),
            batch: Some(batch.id),
        };
        self.commits.insert(tip.clone(), tip_commit);
        let (min, max) = (self.config.ci_minutes.min, self.config.ci_minutes.max);
        let duration = self.streams.ci.uniform(min, max) * 60;
        let state = BatchState::Testing {
            tip,
            ci: CiStatus::Pending,
        };
        let status = BatchStatus::new(batch.id, batch.base.clone(), members, state);
        let run = Run::Waiting { duration };
        self.batches.insert(batch.id, SimBatch { status, run });
        self.ledger.started(batch.id);
        // This plan's landings ran first, so a base landed in it is trunk.
        let base = if batch.base == self.trunk {
            None
        } else {
            self.commits[&batch.base].batch.map(|b| b.0)
        };
        self.note(|t| TraceEvent::Start {
            t,
            batch: batch.id.0,
            base,
            members: batch.prs.iter().map(|p| p.0).collect(),
        });
        self.waiting.push_back(Job::Batch(batch.id));
        self.assign_runners();
        Ok(())
    }

    fn land(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        let tip = self.ledger.check_land(batch)?.tip;
        self.ledger.landing(batch);
        let status = self.batches[&batch].status.clone();
        let refusal = self.refusal(&status);
        if let Some(reason) = refusal {
            self.note(|t| TraceEvent::Refused {
                t,
                batch: batch.0,
                reason: reason.to_string(),
            });
            let b = self.batches.get_mut(&batch).expect("known");
            b.status.state = BatchState::LandRefused { tip, reason };
            return Ok(());
        }
        let b = self.batches.get_mut(&batch).expect("known");
        b.status.state = BatchState::Landed { tip: tip.clone() };
        if let Run::Done { run } = b.run {
            self.runs[run].landed = true;
        }
        self.trunk = tip;
        self.note(|t| TraceEvent::Land {
            t,
            batch: batch.0,
            members: status.members.iter().map(|m| m.pr.0).collect(),
        });
        for m in &status.members {
            self.leave(m.pr, Exit::Landed);
        }
        if self.admission == Admission::Github {
            let landed: BTreeSet<PrNumber> = status.members.iter().map(|m| m.pr).collect();
            let children: Vec<PrNumber> = self
                .prs
                .iter()
                .filter(|(_, c)| c.present() && c.base().is_some_and(|b| landed.contains(&b)))
                .map(|(&n, _)| n)
                .collect();
            for child in children {
                self.begin_rebase(child, Rebase::BaseLanded);
            }
        }
        self.ledger.landed(batch);
        debug_assert_eq!(self.check_landed(&status.members), Ok(()));
        Ok(())
    }

    fn retire(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        self.ledger.check_retire(batch)?;
        let b = self.batches.remove(&batch).expect("known");
        self.ledger.retired(batch);
        let cancelled = !matches!(b.run, Run::Done { .. });
        self.note(|t| TraceEvent::Retire {
            t,
            batch: batch.0,
            cancelled,
        });
        match b.run {
            Run::Waiting { .. } => self.waiting.retain(|&w| w != Job::Batch(batch)),
            Run::Running { run, done } => {
                self.events.remove(&done);
                self.runs[run].consumed = self.now - self.runs[run].started_at;
                self.running -= 1;
                self.assign_runners();
            }
            Run::Done { .. } => {}
        }
        Ok(())
    }

    /// Under `label`, ejecting a pull request for the conflict with trunk
    /// it has sends it and its descendants out for a rebase; every other
    /// ejection is final.
    fn eject(&mut self, pr: PrNumber, reason: &EjectReason) -> Result<(), ForgeError> {
        self.ledger.check_eject(pr)?;
        let justified = self.justified(pr);
        let rebase = self.admission == Admission::Label
            && matches!(
                reason,
                EjectReason::Conflicts {
                    with: ConflictWith::Trunk
                }
            )
            && self.prs[&pr].conflicts_with_trunk;
        if !rebase && matches!(self.prs[&pr].out, Some(Out::WithBase { .. })) {
            self.untrace_blocked(pr);
        }
        self.note(|t| TraceEvent::Eject {
            t,
            pr: pr.0,
            reason: reason.to_string(),
            justified,
            r#final: !rebase,
        });
        if rebase {
            let descendants = self.descendants(pr);
            self.begin_rebase(pr, Rebase::Conflict);
            for d in descendants {
                self.out_with_base(d);
            }
        } else {
            self.leave(
                pr,
                Exit::Ejected {
                    reason: reason.clone(),
                    justified,
                },
            );
        }
        self.ledger.ejected(pr);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
