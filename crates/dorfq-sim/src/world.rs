//! The hidden-truth world: pull requests, CI runs, events, and the
//! [`Forge`] a scheduler acts through. Trunk moves only by the queue's
//! landings, so the trunk position it reports is its count of landed pull
//! requests.

use std::collections::{BTreeMap, BTreeSet};

use dorfq_core::ledger::Ledger;
use dorfq_core::{
    BatchId, BatchMember, BatchStart, BatchState, BatchStatus, CiStatus, ConflictWith, Departed,
    EjectReason, Failure, Forge, ForgeError, Hold, LandRefusal, NoticeReason, Observation, PrBase,
    PrNumber, PrSnapshot, PrStatus, Sha, StackPolicy, Timestamp, Withdrawal,
};

use crate::config::{Admission, Minutes, WorldConfig};
use crate::metrics::{
    Break, End, Exit, FirstNotice, Metrics, PrCiRecord, PrRecord, RunRecord, RunningSeconds,
};
use crate::rng::{SplitMix64, Streams, mix, unit};
use crate::trace::{Rebase, TraceEvent};

const HOUR: f64 = crate::HOUR as f64;

/// What a `CiDone` ends: a batch's run, or a pull request's own CI on its
/// current head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    Batch(BatchId),
    PrCi(PrNumber),
}

/// A scheduled happening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    /// A stack arrives on trunk.
    Arrival,
    /// A stack's second burst of `size` pull requests arrives on top of
    /// what is left of it below `on`, its first burst's top.
    Grow {
        on: PrNumber,
        size: usize,
        atomic: bool,
    },
    CiDone(Job),
    Repush(PrNumber),
    Withdraw(PrNumber),
    /// The author pushes the rebase.
    Rebase(PrNumber),
    /// The author acts on a failed own-CI run: pushes the fix when the
    /// pull request is broken, else re-runs it on the same head once trunk
    /// is green.
    Fix(PrNumber),
    /// The author acts on the queue's notice that the pull request is
    /// probably broken: rebases it onto trunk and pushes, broken or not.
    Heed(PrNumber),
    /// A push repairs, in place, the break that the landing of this batch
    /// put on trunk.
    Repair(BatchId),
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
    /// Own breakage: when a pull request that landed since its arrival or
    /// its last fix push first conflicted with it; `None` while nothing has.
    broke_at: Option<u64>,
    /// When its author last pushed a fix for its breakage.
    last_fix_push: Option<u64>,
    /// Its author's pending action after its own CI failed: the fix push,
    /// or a re-run on the same head.
    fix: Option<EventKey>,
    /// The pending `fix` was scheduled as a re-run and has no `rebase` line
    /// yet.
    rerun: bool,
    /// Its author's pending push in answer to the queue's notice.
    heed: Option<EventKey>,
    conflicts_with_trunk: bool,
    key: u64,
    /// The pull request it is stacked on, set at arrival.
    base: Option<PrNumber>,
    /// Lands only with its ancestors; only ever set on a stacked arrival.
    atomic: bool,
    repush: Option<EventKey>,
    withdraw: Option<EventKey>,
    /// Its own clock, seeded from `key`: every PR CI duration and rebase
    /// delay it draws.
    own: SplitMix64,
    /// Its own CI on its current head, from the request to the run's end.
    ci: Option<Run>,
    /// The world's `landed_count` when its latest own-CI run started.
    ci_landed: usize,
    /// Why the queue does not see it while it is in the world; `None` when
    /// it does.
    out: Option<Out>,
}

impl SimPr {
    /// The pull request it stacked on at arrival; never changes.
    fn base(&self) -> Option<PrNumber> {
        self.base
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
        matches!(self.record.left, Some((_, Exit::Landed { .. })))
    }
}

/// A run, which starts the instant it is requested: `run` indexes its
/// record and `done` is its scheduled end. A pull request's own run never
/// reaches `Done`: it is dropped when it ends.
enum Run {
    Running { run: usize, done: EventKey },
    Done { run: usize },
}

struct SimBatch {
    status: BatchStatus,
    run: Run,
    /// The verdict its tree earns, fixed when the run started: some member
    /// not yet landed was bad, two of them conflicted, or the tree holds an
    /// unrepaired break.
    fails: bool,
    /// The run's interval touches a CI outage, so it fails whatever its
    /// tree; known at its start, its end being drawn there.
    outage: bool,
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
    /// Pull requests landed so far.
    landed_count: usize,
    next_commit: u64,
    next_pr: u64,
    events: BTreeMap<EventKey, Event>,
    next_seq: u64,
    runs: Vec<RunRecord>,
    pr_ci_runs: Vec<PrCiRecord>,
    /// When the last landing or executed ejection happened; 0 before any.
    last_progress: u64,
    /// Every break a bad landing put on trunk, by the landing batch, with
    /// the key of its repair.
    breaks: BTreeMap<BatchId, (Break, EventKey)>,
    /// Rules 5, 6 and 8, against the latest observation.
    ledger: Ledger,
    /// Events since the last [`World::take_trace`]; `None` untraced.
    trace: Option<Vec<TraceEvent>>,
}

/// The synthetic commit numbered `n`.
fn commit(n: u64) -> Sha {
    Sha::parse(&format!("{n:040x}")).expect("40 lowercase hex digits")
}

/// A size drawn from `weights`, where `weights[k - 1]` is the weight of
/// size `k`.
fn draw_size(rng: &mut SplitMix64, weights: &[f64]) -> usize {
    rng.weighted(weights) + 1
}

/// A uniform draw of `m` in whole seconds.
fn seconds(rng: &mut SplitMix64, m: Minutes) -> u64 {
    rng.uniform(m.min, m.max) * 60
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
            landed_count: 0,
            next_commit: 2,
            next_pr: 1,
            events: BTreeMap::new(),
            next_seq: 0,
            runs: Vec::new(),
            pr_ci_runs: Vec::new(),
            last_progress: 0,
            breaks: BTreeMap::new(),
            ledger: Ledger::new(),
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

    /// Fires every event due by `t`, in scheduling order, each with the
    /// clock at its own time, then moves the clock to `t`.
    pub fn advance_to(&mut self, t: u64) {
        while let Some(entry) = self.events.first_entry() {
            let at = entry.key().0;
            if at > t {
                break;
            }
            self.now = at;
            let event = entry.remove();
            self.fire(event);
        }
        self.now = t;
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

    /// Every batch run, in start order.
    pub fn runs(&self) -> &[RunRecord] {
        &self.runs
    }

    /// Every pull request's own CI run, billed or not, in start order.
    pub fn pr_ci_runs(&self) -> &[PrCiRecord] {
        &self.pr_ci_runs
    }

    /// When the forge last executed a landing or an ejection, conflict
    /// ejections under `label` included; 0 before any.
    pub(crate) fn last_progress(&self) -> u64 {
        self.last_progress
    }

    /// The summary of a replication that ended as `end`, with the runs
    /// still running billed up to the clock.
    pub fn metrics(&self, end: End) -> Metrics {
        let breaks: Vec<Break> = self.breaks.values().map(|(b, _)| *b).collect();
        Metrics::summarise(
            &self.pr_records(),
            &self.runs,
            &self.pr_ci_runs,
            &breaks,
            self.running_elapsed(),
            end,
        )
    }

    /// Runner-seconds used so far by the batch runs and billed PR CI runs
    /// still running.
    fn running_elapsed(&self) -> RunningSeconds {
        let batches = self.batches.values().filter_map(|b| match b.run {
            Run::Running { run, .. } => Some(self.now - self.runs[run].started_at),
            Run::Done { .. } => None,
        });
        let prs = self.prs.values().filter_map(|p| match p.ci {
            Some(Run::Running { run, .. }) if self.pr_ci_runs[run].billed => {
                Some(self.now - self.pr_ci_runs[run].started_at)
            }
            _ => None,
        });
        RunningSeconds {
            batches: batches.sum(),
            billed_pr_ci: prs.sum(),
        }
    }

    /// Present pull requests the queue does not see.
    pub fn out(&self) -> usize {
        self.prs
            .values()
            .filter(|p| p.present() && !p.enqueued())
            .count()
    }

    /// Every pull request's arrival time, hidden truth (conflicts with
    /// trunk, key) and base, by number.
    #[cfg(test)]
    pub(crate) fn truths(&self) -> Vec<(u64, bool, u64, Option<PrNumber>)> {
        let prs = self.prs.values();
        prs.map(|p| (p.record.arrived_at, p.conflicts_with_trunk, p.key, p.base))
            .collect()
    }

    /// Whether `i` and `j` break each other: their keys say so and neither
    /// is stacked on the other. A run holding both unlanded fails; whichever
    /// lands first breaks the other.
    pub(crate) fn conflicts(&self, i: PrNumber, j: PrNumber) -> bool {
        let (a, b) = (&self.prs[&i], &self.prs[&j]);
        i != j
            && unit(mix(a.key, b.key)) < self.config.p_interact
            && !self.descends(i, j)
            && !self.descends(j, i)
    }

    /// Since when `pr` has been bad: the earliest of its own breakage and,
    /// while its base is in the world, its base's badness, which its branch
    /// carries. `None` while it is good.
    pub(crate) fn bad_since(&self, pr: PrNumber) -> Option<u64> {
        let p = &self.prs[&pr];
        let base = p.base().filter(|b| self.prs[b].present());
        let inherited = base.and_then(|b| self.bad_since(b));
        match (p.broke_at, inherited) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Whether a run on `tip` fails for its tree: some member not yet
    /// landed is bad, two of them conflict, or the walk to the root crosses
    /// an unrepaired break.
    fn fails(&self, tip: &Sha) -> bool {
        let unlanded: Vec<PrNumber> = self
            .tree(tip)
            .into_iter()
            .filter(|i| !self.prs[i].landed())
            .collect();
        let content = unlanded.iter().enumerate().any(|(k, &i)| {
            self.bad_since(i).is_some() || unlanded[k + 1..].iter().any(|&j| self.conflicts(i, j))
        });
        content || self.trunk_broken(tip)
    }

    /// The commits on the walk from `sha` to the root, `sha` first.
    fn ancestry<'a>(&'a self, sha: &'a Sha) -> impl Iterator<Item = &'a Commit> {
        let first = &self.commits[sha];
        std::iter::successors(Some(first), |c| c.parent.as_ref().map(|p| &self.commits[p]))
    }

    /// The scheduled repair of every break on the walk from `sha` to the
    /// root that is not yet repaired.
    fn open_repairs<'a>(&'a self, sha: &'a Sha) -> impl Iterator<Item = EventKey> {
        let any_open = self.breaks.values().any(|(b, _)| b.until.is_none());
        let walk = any_open.then(|| self.ancestry(sha)).into_iter().flatten();
        walk.filter_map(|c| {
            let (b, repair) = self.breaks.get(&c.batch?)?;
            b.until.is_none().then_some(*repair)
        })
    }

    /// Whether the walk from `sha` to the root crosses a commit whose
    /// landing broke trunk and is not yet repaired.
    fn trunk_broken(&self, sha: &Sha) -> bool {
        self.open_repairs(sha).next().is_some()
    }

    /// Whether a run over `[start, end]` touches a CI outage.
    fn outage(&self, start: u64, end: u64) -> bool {
        self.config.outages.iter().any(|o| o.touches(start, end))
    }

    /// When a run re-run now for want of a green CI could pass: the latest
    /// of the end of every outage window holding now and the scheduled
    /// repair of every open break on trunk's walk; `None` when nothing
    /// holds.
    fn clears_at(&self) -> Option<u64> {
        let now = self.now;
        let outages = self.config.outages.iter();
        let down = outages.filter(|o| o.from() <= now && now < o.until());
        let down = down.map(|o| o.until()).max();
        down.max(self.trunk_green_at().map(|(at, _)| at))
    }

    /// When every open break on trunk's walk will have been repaired: the
    /// latest of their scheduled repairs; `None` while trunk is green.
    fn trunk_green_at(&self) -> Option<EventKey> {
        self.open_repairs(&self.trunk).max()
    }

    /// Breaks every present pull request that conflicts with one of
    /// `landed` and is not broken already.
    fn break_against(&mut self, landed: &[PrNumber]) {
        let present: Vec<PrNumber> = self
            .prs
            .iter()
            .filter(|(_, p)| p.present() && p.broke_at.is_none())
            .map(|(&n, _)| n)
            .collect();
        for p in present {
            if let Some(&by) = landed.iter().find(|&&m| self.conflicts(p, m)) {
                self.prs.get_mut(&p).expect("arrived").broke_at = Some(self.now);
                for bad in std::iter::once(p).chain(self.descendants(p)) {
                    self.prs.get_mut(&bad).expect("arrived").record.ever_bad = true;
                }
                self.note(|t| TraceEvent::Broken {
                    t,
                    pr: p.0,
                    by: by.0,
                });
            }
        }
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

    /// Checks the world's invariants: of two landed pull requests that
    /// conflict, the later arrived or was fixed after the earlier landed (see
    /// [`Self::landed_apart`]); a landed pull request's base landed at or before
    /// it, trunk's tree is exactly the landed set, every running run's end
    /// and every open break's repair is scheduled, and every present pull
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
            for &j in &landed[k + 1..] {
                self.landed_apart(i, j)?;
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
        if self.landed_count != landed.len() {
            return Err(format!(
                "{} landings counted, {} pull requests landed",
                self.landed_count,
                landed.len()
            ));
        }
        let batches = self.batches.values().map(|b| &b.run);
        let prs = self.prs.values().filter_map(|p| p.ci.as_ref());
        for run in batches.chain(prs) {
            if let Run::Running { done, .. } = run
                && !matches!(self.events.get(done), Some(Event::CiDone(_)))
            {
                return Err(format!("a running run's end at {done:?} is not scheduled"));
            }
        }
        for (batch, (b, repair)) in &self.breaks {
            if b.until.is_none() && self.events.get(repair) != Some(&Event::Repair(*batch)) {
                return Err(format!(
                    "batch {}'s break of trunk has no repair scheduled",
                    batch.0
                ));
            }
        }
        Ok(())
    }

    /// Checks where the present pull requests are: an enqueued one's base is
    /// enqueued or landed; one out with its base has a base that is out;
    /// under `github` an enqueued one has no run in flight, no fix pending
    /// and does not conflict, and a stacked one has no run in flight.
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
                    if github && (p.ci.is_some() || p.fix.is_some() || p.conflicts_with_trunk) {
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

    /// Checks that two landed pull requests landed apart: unless they do
    /// not conflict, the one that landed second arrived or had a fix pushed
    /// after the first landed (see [`Self::fresh_since`]), or landed bad
    /// through a run that missed it.
    fn landed_apart(&self, i: PrNumber, j: PrNumber) -> Result<(), String> {
        if !self.conflicts(i, j) {
            return Ok(());
        }
        let (first, second) = if self.left_at(i) <= self.left_at(j) {
            (i, j)
        } else {
            (j, i)
        };
        let landed_bad = matches!(
            self.prs[&second].record.left,
            Some((_, Exit::Landed { bad: true }))
        );
        if !landed_bad && !self.fresh_since(second, self.left_at(first)) {
            return Err(format!(
                "prs {} and {} landed but conflict",
                first.0, second.0
            ));
        }
        Ok(())
    }

    /// Whether `pr`'s content postdates `t`: it arrived at or after `t`, or
    /// its author last pushed a fix at or after it.
    fn fresh_since(&self, pr: PrNumber, t: u64) -> bool {
        let p = &self.prs[&pr];
        p.record.arrived_at >= t || p.last_fix_push.is_some_and(|f| f >= t)
    }

    /// Checks the pull requests of a batch landing now, before they leave:
    /// unless its run `missed` a break, none is bad, and none conflicts with
    /// a landed pull request whose landing its content predates.
    fn check_landed(&self, members: &[BatchMember], missed: bool) -> Result<(), String> {
        if missed {
            return Ok(());
        }
        for m in members {
            let i = m.pr;
            if self.bad_since(i).is_some() {
                return Err(format!("pr {} landed but is bad", i.0));
            }
            let landed = self.prs.iter().filter(|(_, p)| p.landed());
            let others = members.iter().map(|o| o.pr).filter(|&o| o != i);
            for j in landed.map(|(&j, _)| j).chain(others) {
                let together = !self.prs[&j].landed();
                if self.conflicts(i, j) && (together || !self.fresh_since(i, self.left_at(j))) {
                    return Err(format!("prs {} and {} landed but conflict", i.0, j.0));
                }
            }
        }
        Ok(())
    }

    /// When `pr` left, or now while it is in the world.
    fn left_at(&self, pr: PrNumber) -> u64 {
        let left = self.prs[&pr].record.left.as_ref();
        left.map_or(self.now, |(t, _)| *t)
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
        let per_hour = self.config.arrivals_per_hour / self.config.mean_pull_requests_per_stack();
        let delay = delay(&mut self.streams.arrivals, per_hour);
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
                // Every draw is taken whatever the queue holds, so the draws
                // are the same under every policy.
                let stacks = &mut self.streams.stacks;
                let size = draw_size(stacks, &self.config.stack_sizes);
                let atomic = stacks.bernoulli(self.config.p_atomic);
                let grows = stacks.bernoulli(self.config.p_stack_grows);
                let growth = draw_size(stacks, &self.config.stack_growth_sizes);
                let after = seconds(stacks, self.config.stack_growth_minutes);
                let top = self.burst(None, size, atomic, true);
                let at = self.now + after;
                if grows && at < self.config.hours * crate::HOUR {
                    let on = top;
                    self.schedule(
                        at,
                        Event::Grow {
                            on,
                            size: growth,
                            atomic,
                        },
                    );
                }
                self.schedule_arrival();
            }
            Event::Grow { on, size, atomic } => {
                let mut at = Some(on);
                while let Some(n) = at
                    && !self.prs[&n].present()
                {
                    at = self.prs[&n].base();
                }
                self.burst(at, size, atomic, false);
            }
            Event::CiDone(Job::Batch(batch)) => self.complete(batch),
            Event::CiDone(Job::PrCi(pr)) => self.complete_pr_ci(pr),
            Event::Repush(pr) => {
                if !self.pushing(pr) {
                    let pushed = self.push(pr, false);
                    self.recheck(pushed);
                }
                self.schedule_repush(pr);
            }
            Event::Fix(pr) if self.prs[&pr].broke_at.is_none() => {
                // The run failed for trunk or an outage, not for the pull
                // request: its author re-runs it on the same head once CI is
                // back and trunk is green.
                if let Some(at) = self.clears_at() {
                    let key = self.schedule(at, Event::Fix(pr));
                    self.prs.get_mut(&pr).expect("arrived").fix = Some(key);
                } else {
                    self.request_pr_ci(pr, false);
                }
            }
            Event::Fix(pr) => {
                // A pending re-run that a landing broke meanwhile is a fix
                // push now, and its author starts it here.
                if std::mem::take(&mut self.prs.get_mut(&pr).expect("arrived").rerun) {
                    self.note(|t| TraceEvent::Rebase {
                        t,
                        pr: pr.0,
                        why: Rebase::Broken,
                    });
                }
                self.prs.get_mut(&pr).expect("arrived").fix = None;
                self.fix_push(pr);
            }
            Event::Heed(pr) => {
                self.prs.get_mut(&pr).expect("arrived").heed = None;
                if !self.pushing(pr) {
                    self.note(|t| TraceEvent::Rebase {
                        t,
                        pr: pr.0,
                        why: Rebase::Broken,
                    });
                    self.fix_push(pr);
                }
            }
            Event::Rebase(pr) => {
                let p = self.prs.get_mut(&pr).expect("arrived");
                let Some(Out::Rebasing { why, .. }) = p.out else {
                    unreachable!("a pushed rebase is rebasing");
                };
                p.conflicts_with_trunk = false;
                p.record.rebases += 1;
                // A landed base's rebase is the queue's split of the stack:
                // the rebased pull request's own run is billed to it.
                let pushed = self.push(pr, why == Rebase::BaseLanded);
                for p in pushed {
                    match self.admission {
                        Admission::Label => self.admit(p),
                        Admission::Github => self.set_out(p, Some(Out::Checking)),
                    }
                }
            }
            Event::Withdraw(pr) => {
                self.note(|t| TraceEvent::Withdraw { t, pr: pr.0 });
                self.leave(pr, Exit::Withdrawn);
            }
            Event::Repair(batch) => {
                let (b, _) = self
                    .breaks
                    .get_mut(&batch)
                    .expect("a scheduled repair's break");
                b.until = Some(self.now);
                self.note(|t| TraceEvent::TrunkRepaired { t });
            }
        }
    }

    /// `github`: every pull request in `pushed`, which have new heads, goes
    /// out to check them; one that was in the queue is traced leaving it.
    /// `label` keeps them where they are, held until their runs pass.
    fn recheck(&mut self, pushed: Vec<PrNumber>) {
        if self.admission != Admission::Github {
            return;
        }
        for p in pushed {
            let was_in = self.prs[&p].enqueued();
            self.set_out(p, Some(Out::Checking));
            if was_in {
                self.note(|t| TraceEvent::Recheck { t, pr: p.0 });
            }
        }
    }

    /// Drops `pr`'s pending fix push or re-run, if any.
    fn cancel_fix(&mut self, pr: PrNumber) {
        if let Some(key) = self.prs.get_mut(&pr).expect("arrived").fix.take() {
            self.events.remove(&key);
        }
    }

    /// Drops every push `pr`'s author has pending — the fix or re-run, and
    /// the answer to a notice: a push by someone else is about to re-head
    /// it, or it is leaving.
    fn cancel_pushes(&mut self, pr: PrNumber) {
        self.cancel_fix(pr);
        if let Some(key) = self.prs.get_mut(&pr).expect("arrived").heed.take() {
            self.events.remove(&key);
        }
    }

    /// Whether `pr`'s author is pushing already: mid-rebase, its own or its
    /// base's, or about to push a fix. A pending heed is not counted: every
    /// push cancels it.
    fn pushing(&self, pr: PrNumber) -> bool {
        let p = &self.prs[&pr];
        p.fix.is_some() || matches!(p.out, Some(Out::Rebasing { .. } | Out::WithBase { .. }))
    }

    /// `pr`'s author pushes a rebase onto trunk as it is now: the fix of
    /// its breakage when it is broken, which `fixed` records, and a new
    /// head either way.
    fn fix_push(&mut self, pr: PrNumber) {
        let p = self.prs.get_mut(&pr).expect("arrived");
        if p.broke_at.take().is_some() {
            p.last_fix_push = Some(self.now);
            self.note(|t| TraceEvent::Fixed { t, pr: pr.0 });
        }
        self.prs.get_mut(&pr).expect("arrived").record.rebases += 1;
        let pushed = self.push(pr, false);
        self.recheck(pushed);
    }

    /// Gives `pr` and every present pull request stacked on it a new head,
    /// each with its own CI requested and any pending push of its author's
    /// dropped, and returns them, `pr` first. `pr`'s run is `billed` as
    /// given; its descendants' never are.
    fn push(&mut self, pr: PrNumber, billed: bool) -> Vec<PrNumber> {
        let pushed: Vec<PrNumber> = std::iter::once(pr).chain(self.descendants(pr)).collect();
        for &p in &pushed {
            self.cancel_pushes(p);
            let head = self.mint();
            self.prs.get_mut(&p).expect("arrived").head = head;
            self.note(|t| TraceEvent::Repush { t, pr: p.0 });
            self.request_pr_ci(p, billed && p == pr);
        }
        pushed
    }

    /// Creates `size` pull requests at once, each stacked on the one before
    /// it and the first on `base`, and returns the last. Every one above a
    /// root is `atomic` when the stack is; only the first can conflict with
    /// trunk, and only when `root_may_conflict` (the rest were cut from the
    /// stack's branch and are its content).
    fn burst(
        &mut self,
        base: Option<PrNumber>,
        size: usize,
        atomic: bool,
        root_may_conflict: bool,
    ) -> PrNumber {
        let mut base = base;
        let mut may_conflict = root_may_conflict;
        for _ in 0..size {
            base = Some(self.arrive(base, atomic, may_conflict));
            may_conflict = false;
        }
        base.expect("a burst brings at least one pull request")
    }

    /// One pull request arrives on `base`, with its truth drawn, its own CI
    /// requested, its place under the admission and its churn clocks.
    fn arrive(&mut self, base: Option<PrNumber>, atomic: bool, may_conflict: bool) -> PrNumber {
        let number = PrNumber(self.next_pr);
        self.next_pr += 1;
        let conflicts = self.streams.truth.bernoulli(self.config.p_conflict);
        let key = self.streams.truth.next_u64();
        let head = self.mint();
        let record = PrRecord {
            number,
            arrived_at: self.now,
            left: None,
            rebases: 0,
            ever_bad: false,
            notice: None,
        };
        let atomic = atomic && base.is_some();
        let pr = SimPr {
            record,
            head,
            broke_at: None,
            last_fix_push: None,
            fix: None,
            rerun: false,
            heed: None,
            conflicts_with_trunk: conflicts && may_conflict,
            key,
            base,
            atomic,
            repush: None,
            withdraw: None,
            own: SplitMix64::new(key),
            ci: None,
            ci_landed: self.landed_count,
            out: None,
        };
        let conflicts = pr.conflicts_with_trunk;
        self.prs.insert(number, pr);
        // Its branch carries its base's commits, and so its badness.
        let bad = self.bad_since(number).is_some();
        self.prs.get_mut(&number).expect("arrived").record.ever_bad = bad;
        self.note(|t| TraceEvent::Arrive {
            t,
            pr: number.0,
            base: base.map(|b| b.0),
            atomic,
            conflicts,
        });
        self.request_pr_ci(number, false);
        match (self.admission, base) {
            (Admission::Github, _) => self.set_out(number, Some(Out::Checking)),
            (Admission::Label, Some(b)) if !self.prs[&b].enqueued() => {
                self.out_with_base(number);
            }
            (Admission::Label, _) => self.admit(number),
        }
        self.schedule_repush(number);
        self.schedule_withdraw(number);
        number
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
        self.cancel_pushes(pr);
        self.set_out(pr, Some(Out::WithBase { on }));
        let bad = self.bad_since(pr).is_some();
        self.note(|t| TraceEvent::Blocked {
            t,
            pr: pr.0,
            on: on.0,
            r#final: false,
            bad,
        });
    }

    /// `pr`'s author starts rebasing it for `why`, dropping any other push
    /// pending; the push is drawn from its own clock.
    fn begin_rebase(&mut self, pr: PrNumber, why: Rebase) {
        self.cancel_pushes(pr);
        let minutes = self.config.rebase_minutes;
        let delay = seconds(&mut self.prs.get_mut(&pr).expect("arrived").own, minutes);
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

    /// Records `pr` leaving now for good. Unless it landed, every present
    /// pull request stacked on it, directly or not, leaves `Blocked` on its
    /// own base, with its badness as it was before `pr` left, `pr`'s own
    /// included. A pull request blocked earlier in this instant may leave
    /// again, superseding the block (see [`Self::untrace_blocked`]).
    fn leave(&mut self, pr: PrNumber, exit: Exit) {
        if matches!(exit, Exit::Landed { .. }) {
            self.depart(pr, exit);
            return;
        }
        // Parents before children: `descends` follows the bases, and a base
        // has a lower number than every pull request stacked on it.
        let blocked: Vec<(PrNumber, PrNumber, Option<u64>)> = self
            .descendants(pr)
            .into_iter()
            .map(|d| (d, self.prs[&d].base().expect("stacked"), self.bad_since(d)))
            .collect();
        self.depart(pr, exit);
        for (child, on, bad_since) in blocked {
            self.note(|t| TraceEvent::Blocked {
                t,
                pr: child.0,
                on: on.0,
                r#final: true,
                bad: bad_since.is_some(),
            });
            self.depart(child, Exit::Blocked { on, bad_since });
        }
    }

    /// Records `pr`'s exit now and cancels its churn clocks and any pending
    /// rebase, fix push or answer to a notice; its own CI run is not
    /// cancelled.
    fn depart(&mut self, pr: PrNumber, exit: Exit) {
        let now = self.now;
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
        let clocks = [
            p.repush.take(),
            p.withdraw.take(),
            p.fix.take(),
            p.heed.take(),
            push,
        ];
        if superseded {
            self.untrace_blocked(pr);
        }
        for key in clocks.into_iter().flatten() {
            self.events.remove(&key);
        }
    }

    /// The tree members of `sha`: every own member on the walk to the root.
    fn tree(&self, sha: &Sha) -> Vec<PrNumber> {
        self.ancestry(sha)
            .flat_map(|c| c.members.iter().copied())
            .collect()
    }

    /// Cancels `pr`'s own CI run if it has one and starts one on its
    /// current head, `billed` to the queue or not, drawing the duration
    /// from its own clock. The run tests the head merged with trunk now: it
    /// will fail when the pull request is broken or trunk is, unless it
    /// misses the break, and whenever it touches an outage. On a pull
    /// request that is not broken it is the re-run its author was waiting
    /// to make, so a pending one is dropped.
    fn request_pr_ci(&mut self, pr: PrNumber, billed: bool) {
        self.cancel_pr_ci(pr);
        if self.prs[&pr].broke_at.is_none() {
            self.cancel_fix(pr);
        }
        let minutes = self.config.pr_ci_minutes;
        let duration = seconds(&mut self.prs.get_mut(&pr).expect("arrived").own, minutes);
        let done = self.schedule(self.now + duration, Event::CiDone(Job::PrCi(pr)));
        let broken = self.prs[&pr].broke_at.is_some() || self.trunk_broken(&self.trunk);
        let missed = broken && self.streams.misses.bernoulli(self.config.p_miss);
        let failed = (broken && !missed) || self.outage(self.now, self.now + duration);
        let p = self.prs.get_mut(&pr).expect("arrived");
        self.pr_ci_runs.push(PrCiRecord {
            pr,
            head: p.head.clone(),
            started_at: self.now,
            consumed: 0,
            cancelled: false,
            billed,
            failed,
        });
        let run = self.pr_ci_runs.len() - 1;
        p.ci = Some(Run::Running { run, done });
        p.ci_landed = self.landed_count;
        self.note(|t| TraceEvent::PrCi {
            t,
            pr: pr.0,
            billed,
        });
    }

    /// Cuts `pr`'s own CI run short if it has one, its elapsed time
    /// charged.
    fn cancel_pr_ci(&mut self, pr: PrNumber) {
        let Some(run) = self.prs.get_mut(&pr).expect("arrived").ci.take() else {
            return;
        };
        let Run::Running { run, done } = run else {
            unreachable!("a pull request's run is dropped when it ends");
        };
        self.events.remove(&done);
        let record = &mut self.pr_ci_runs[run];
        record.consumed = self.now - record.started_at;
        record.cancelled = true;
        self.note(|t| TraceEvent::PrCiEnd {
            t,
            pr: pr.0,
            cancelled: true,
            failed: false,
        });
    }

    /// Ends `pr`'s own CI run, passed or failed as recorded at its start.
    /// Whether the pull request is still in the world does not matter: the
    /// forge ran it to its end. A failed run sends its author to fix it,
    /// unless it is out for a rebase (its author, or its base's, is about to
    /// push) or a fix is pending; under `github` it stays out checking.
    /// Under `github` a pull request checking whose run passed goes on: out
    /// for a rebase if it conflicts, waiting if its base is in the world,
    /// else into the queue.
    fn complete_pr_ci(&mut self, pr: PrNumber) {
        let p = self.prs.get_mut(&pr).expect("arrived");
        let Some(Run::Running { run, .. }) = p.ci.take() else {
            unreachable!("a completing PR CI run is running");
        };
        let record = &mut self.pr_ci_runs[run];
        record.consumed = self.now - record.started_at;
        let failed = record.failed;
        self.note(|t| TraceEvent::PrCiEnd {
            t,
            pr: pr.0,
            cancelled: false,
            failed,
        });
        let p = &self.prs[&pr];
        if failed {
            let rebasing = matches!(p.out, Some(Out::Rebasing { .. } | Out::WithBase { .. }));
            if p.present() && !rebasing && p.fix.is_none() {
                self.begin_fix(pr);
            }
        } else if p.present() && p.out == Some(Out::Checking) {
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
    }

    /// `pr`'s author acts on its failed own CI after a delay drawn from its
    /// own clock: fixing it, against trunk as it is when the push lands, or
    /// re-running it when it was not broken (see [`Event::Fix`]). It stays
    /// where it is: held in the queue under `label`, checking under
    /// `github`.
    fn begin_fix(&mut self, pr: PrNumber) {
        let minutes = self.config.rebase_minutes;
        let delay = seconds(&mut self.prs.get_mut(&pr).expect("arrived").own, minutes);
        let key = self.schedule(self.now + delay, Event::Fix(pr));
        let p = self.prs.get_mut(&pr).expect("arrived");
        p.fix = Some(key);
        p.rerun = p.broke_at.is_none();
        if p.rerun {
            self.note(|t| TraceEvent::Rerun { t, pr: pr.0 });
        } else {
            self.note(|t| TraceEvent::Rebase {
                t,
                pr: pr.0,
                why: Rebase::Broken,
            });
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
        let broken = b.fails;
        let outage = b.outage;
        let tree = self.tree(&tip);
        let ci = if flake || outage || (broken && !self.runs[run].missed) {
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
            outage,
        });
    }

    /// The observation now: the enqueued pull requests. One conflicting
    /// with trunk is reported so, as the forge reports it whatever else
    /// holds it; one whose own CI on its current head is running is held
    /// `OwnCiPending`, and one whose author is fixing it `OwnCiFailed`. A
    /// departure the queue did not cause is a
    /// withdrawal: of intent, or because a base left or went out.
    fn snapshot(&self) -> Result<Observation, ForgeError> {
        let prs: Vec<PrSnapshot> = self
            .prs
            .values()
            .filter(|p| p.enqueued())
            .map(|p| {
                let mut holds = BTreeSet::new();
                if p.ci.is_some() {
                    holds.insert(Hold::OwnCiPending);
                }
                if p.fix.is_some() {
                    holds.insert(Hold::OwnCiFailed);
                }
                let status = if p.conflicts_with_trunk {
                    PrStatus::Conflicting(ConflictWith::Trunk)
                } else if !holds.is_empty() {
                    PrStatus::Waiting(holds)
                } else {
                    PrStatus::Ready
                };
                // An enqueued pull request's base is enqueued or landed.
                let base = match p.base() {
                    Some(b) if self.prs[&b].enqueued() => PrBase::Pr(b),
                    _ => PrBase::Trunk,
                };
                let behind = u32::try_from(self.landed_count - p.ci_landed)
                    .expect("a run's untested landings fit u32");
                let snapshot =
                    PrSnapshot::new(p.record.number, p.head.clone(), base, status, behind);
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
                (Some((_, Exit::Blocked { on, .. })), _) => {
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
            u64::try_from(self.landed_count).expect("a landed count fits u64"),
            prs,
            batches,
            departed,
        ))
    }

    /// The landing of `batch` put a bad pull request on trunk: trunk is
    /// broken from now until a push repairs it in place, after a delay drawn
    /// from the own clock of `first`, the batch's first bad member.
    fn break_trunk(&mut self, batch: BatchId, first: PrNumber) {
        let minutes = self.config.trunk_fix_minutes;
        let delay = seconds(&mut self.prs.get_mut(&first).expect("arrived").own, minutes);
        let repair = self.schedule(self.now + delay, Event::Repair(batch));
        let open = Break {
            at: self.now,
            until: None,
        };
        self.breaks.insert(batch, (open, repair));
        self.note(|t| TraceEvent::TrunkBroken { t, batch: batch.0 });
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
        // no textual conflicts between members. A probe's tip adds no
        // members, so its run tests the base's tree alone. The run's verdict
        // is the tree's truth now, a miss of its breakage drawn with it: a
        // landing or a repair while it runs cannot change what it tested. An
        // outage its interval touches fails it, its end being drawn here.
        let tip = self.mint();
        let tip_commit = Commit {
            parent: Some(batch.base.clone()),
            members: batch.prs.clone(),
            batch: Some(batch.id),
        };
        self.commits.insert(tip.clone(), tip_commit);
        let fails = self.fails(&tip);
        let missed = fails && self.streams.misses.bernoulli(self.config.p_miss);
        let duration = seconds(&mut self.streams.ci, self.config.ci_minutes);
        let outage = self.outage(self.now, self.now + duration);
        let state = BatchState::Testing {
            tip,
            ci: CiStatus::Pending,
        };
        let status = BatchStatus::new(batch.id, batch.base.clone(), members, state);
        let done = self.schedule(self.now + duration, Event::CiDone(Job::Batch(batch.id)));
        self.runs.push(RunRecord {
            batch: batch.id,
            members: batch.prs.len(),
            started_at: self.now,
            consumed: 0,
            verdict: None,
            tree: Vec::new(),
            landed: false,
            cancelled: false,
            missed,
        });
        let run = Run::Running {
            run: self.runs.len() - 1,
            done,
        };
        self.batches.insert(
            batch.id,
            SimBatch {
                status,
                run,
                fails,
                outage,
            },
        );
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
        let mut missed = false;
        if let Run::Done { run } = b.run {
            self.runs[run].landed = true;
            missed = self.runs[run].missed;
        }
        debug_assert_eq!(self.check_landed(&status.members, missed), Ok(()));
        let members: Vec<PrNumber> = status.members.iter().map(|m| m.pr).collect();
        let bad: Vec<bool> = members
            .iter()
            .map(|&i| self.bad_since(i).is_some() || members.iter().any(|&j| self.conflicts(i, j)))
            .collect();
        self.trunk = tip;
        self.landed_count += status.members.len();
        self.last_progress = self.now;
        self.note(|t| TraceEvent::Land {
            t,
            batch: batch.0,
            members: status.members.iter().map(|m| m.pr.0).collect(),
        });
        for (&m, &bad) in members.iter().zip(&bad) {
            if bad {
                self.prs.get_mut(&m).expect("arrived").record.ever_bad = true;
            }
            self.leave(m, Exit::Landed { bad });
        }
        if let Some(first) = bad.iter().position(|&b| b) {
            self.break_trunk(batch, members[first]);
        }
        self.break_against(&members);
        let landed: BTreeSet<PrNumber> = members.into_iter().collect();
        let children: Vec<PrNumber> = self
            .prs
            .iter()
            .filter(|(_, c)| c.present() && c.base().is_some_and(|b| landed.contains(&b)))
            .map(|(&n, _)| n)
            .collect();
        for child in children {
            match (self.config.pr_ci_on_base_landed, self.admission) {
                // The author rebases the rest of the stack; the child's run
                // is billed at the push.
                (true, Admission::Github) => self.begin_rebase(child, Rebase::BaseLanded),
                // Its branch already holds what landed: retargeted onto
                // trunk, it runs its own CI again on its current head.
                (true, Admission::Label) => self.request_pr_ci(child, true),
                // Retargeted onto trunk with its head passed, it enters; one
                // still checking enters when its run passes.
                (false, Admission::Github) if self.prs[&child].out == Some(Out::Stacked) => {
                    self.admit(child);
                }
                (false, _) => {}
            }
        }
        self.ledger.landed(batch);
        Ok(())
    }

    fn retire(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        self.ledger.check_retire(batch)?;
        let b = self.batches.remove(&batch).expect("known");
        self.ledger.retired(batch);
        let cancelled = matches!(b.run, Run::Running { .. });
        self.note(|t| TraceEvent::Retire {
            t,
            batch: batch.0,
            cancelled,
        });
        if let Run::Running { run, done } = b.run {
            self.events.remove(&done);
            self.runs[run].consumed = self.now - self.runs[run].started_at;
            self.runs[run].cancelled = true;
        }
        Ok(())
    }

    /// Under `label`, ejecting a pull request for the conflict with trunk
    /// it has sends it and its descendants out for a rebase; every other
    /// ejection is final.
    fn eject(&mut self, pr: PrNumber, reason: &EjectReason) -> Result<(), ForgeError> {
        self.ledger.check_eject(pr)?;
        // Justified only when it is bad against trunk now; one blocked
        // earlier in this instant keeps the badness its block recorded, from
        // before its base left.
        let bad_since = match &self.prs[&pr].record.left {
            Some((t, Exit::Blocked { bad_since, .. })) if *t == self.now => *bad_since,
            _ => self.bad_since(pr),
        };
        let justified = bad_since.is_some();
        self.last_progress = self.now;
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
                    bad_since,
                },
            );
        }
        self.ledger.ejected(pr);
        Ok(())
    }

    /// Judges the notice against the truth — broken against trunk now, or
    /// in conflict with the named pull request — and keeps the pull
    /// request's first. Told it is probably broken, its author rebases and
    /// pushes with probability `p_heed`, `rebase_minutes` later, unless a
    /// push of theirs is already pending; told it fails together with
    /// another, the author can do nothing before that one lands.
    fn notify(&mut self, pr: PrNumber, reason: &NoticeReason) -> Result<(), ForgeError> {
        self.ledger.check_notify(pr)?;
        let (justified, waited) = match reason {
            NoticeReason::Broken { .. } => match self.bad_since(pr) {
                Some(since) => {
                    let from = since.max(self.prs[&pr].record.arrived_at);
                    (true, Some(self.now - from))
                }
                None => (false, None),
            },
            NoticeReason::InteractsWith { pr: other, .. } => (self.conflicts(pr, *other), None),
        };
        let first = FirstNotice {
            t: self.now,
            justified,
            waited,
        };
        let p = self.prs.get_mut(&pr).expect("arrived");
        p.record.notice.get_or_insert(first);
        self.note(|t| TraceEvent::Notice {
            t,
            pr: pr.0,
            reason: reason.to_string(),
            justified,
        });
        if matches!(reason, NoticeReason::Broken { .. }) {
            let p = self.prs.get_mut(&pr).expect("arrived");
            let heeds = p.own.bernoulli(self.config.p_heed);
            if heeds && p.fix.is_none() && p.heed.is_none() {
                let delay = seconds(&mut p.own, self.config.rebase_minutes);
                let key = self.schedule(self.now + delay, Event::Heed(pr));
                self.prs.get_mut(&pr).expect("arrived").heed = Some(key);
            }
        }
        self.ledger.notified(pr);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
