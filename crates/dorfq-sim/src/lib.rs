//! DorfQ's simulator: any [`Scheduler`] run against a seeded, event-driven
//! world of pull requests with hidden truths.
//!
//! The [`World`] is a [`Forge`]: it checks rules 5, 6 and 8 of the
//! [`dorfq_core`] contract through its [`Ledger`](dorfq_core::ledger::Ledger)
//! and answers a breach with [`ForgeError::Permanent`]. Behind it each pull
//! request conflicts textually with trunk or not, and carries a key from
//! which whether two pull requests conflict is a function: a pair that
//! conflicts breaks together, and whichever lands first breaks the other.
//! A pull request is bad while something that landed since it arrived or
//! since its author's last fix conflicts with it, or while its base is
//! bad. A batch's run starts the instant the batch does, takes a uniform time and fails when, at its
//! start, its tree held a bad pull request or a conflicting pair not yet
//! landed, or an unrepaired break of trunk, unless it missed that at
//! `p_miss`; or when the flake draw fires. A batch landed through a miss
//! with a bad member breaks trunk: every run on a tree holding the break
//! fails until a push repairs it in place, `trunk_fix_minutes` later. Any
//! run, a batch's or a pull request's own, whose interval touches one of
//! the `outages` windows fails whatever its tree. An
//! ejection is justified only of a pull request bad at that instant. The world has no runner pool: CI capacity is not the
//! constraint, and the only bound on concurrent batch runs is the
//! scheduler's own `lanes`. Time is the simulator's: the scheduler never
//! sees it.
//!
//! Pull requests arrive as stacks: a burst of one or more created at one
//! instant, each stacked on the one before it, its size drawn from
//! `stack_sizes`. A stack may be atomic, every pull request above its root
//! `Atomic`, so it lands only whole. A stack may grow once, later: a second
//! burst on top of what is left of it, or on trunk if nothing is. Only a
//! stack's root can conflict with trunk; everything above it was cut from
//! the stack's branch. A base that leaves without landing takes everything
//! stacked on it with it (`Blocked`), each counted under its base's exit
//! with its own truth, and a re-push restacks every descendant.
//!
//! Every pull request also runs its own CI, beside the queue, each time it
//! gets a head: at arrival and at every re-push. A run starts at once and
//! takes a uniform time; a head change cancels the old head's run. It
//! tests the head merged with trunk as of its start: it fails when the
//! pull request is itself broken (not merely on a broken base) or trunk
//! is, unless it misses that at `p_miss`, and whenever it touches an
//! outage. After a rebase delay its author then pushes a fix, the only push
//! that clears the breakage, or, when the pull request was not broken,
//! re-runs it on the same head once no outage holds and trunk is green,
//! the wait traced `re-run N`. As
//! the GitHub forge does, the world reports a pull request `Waiting(OwnCiPending)` until its current head's run has passed, and
//! `Waiting(OwnCiFailed)` while its author fixes it, so no batch takes it
//! before then. With `pr_ci_on_base_landed`, a landed base
//! makes the pull requests stacked on it run again: the run of each direct
//! child is the queue's doing, splitting the stack there, and is *billed*,
//! counted in `runner_hours`; every other PR CI run is the author's and
//! costs the queue nothing. Each pull request draws its runs' durations
//! and its rebase delays from its own clock, seeded from its key, so the
//! n-th draw of a pull request is the same under every policy.
//!
//! A scheduler's notice reaches the author. Told that the pull request is
//! probably broken against trunk, the author acts at `p_heed`, a draw from
//! the pull request's own clock: after a rebase delay they rebase it onto
//! trunk and push, broken or not, as for a failed own-CI run, the push
//! traced `rebase N broken` when it happens. Until then the pull request
//! stays where it was, batchable. Told that it probably fails together
//! with another, the author can do nothing until that one lands. Each
//! notice is judged against the truth — broken now, or in conflict with
//! the pull request it names — and the first a pull request receives is
//! kept for the report.
//!
//! Each policy runs under an [`Admission`]: what the queue asks of a pull
//! request before it sees it. Under `label`, dorfq's, a pull request is in
//! the queue from arrival; ejected for the conflict with trunk it has, it
//! and everything stacked on it go out while its author rebases, and come
//! back with new heads at the push. Under `github`, GitHub's, the queue
//! sees a pull request only once it is on trunk and its current head has
//! passed its own CI: a stacked pull request waits for its base to land,
//! is rebased then, and is checked again before it enters; a conflict is
//! rebased before the queue ever sees it; a push takes an enqueued pull
//! request out until the new head passes.
//!
//! Every draw comes from SplitMix64 streams split by purpose (arrivals,
//! truth, CI, churn, stacks), so two policies at one seed see the same
//! stacks and the same truths. Where a growth lands is the policy's doing:
//! one that has already landed or ejected the stack sees it arrive on what
//! is left of it, or on trunk.
//!
//! [`simulate`] runs every policy of a [`SimConfig`] at every replication
//! and returns a [`Report`]: runner-hours per landed pull request, time to
//! land, to reject and from breaking to ejection, from breaking to the
//! first notice, wasted runner time, false ejections and false notices,
//! one column per policy. Traced, the first replication of each policy is recorded as
//! [`TraceEvent`]s: every step and its plan, and everything the world did.
//! [`Report::to_json`] writes the report, its configuration and those
//! traces as one JSON document, the input of the report site.
//!
//! [`sweep`] runs a [`SweepConfig`], a grid of configurations over one
//! base, every replication an independent task on a pool of threads, and
//! returns a [`SweepReport`]: every point's means, and every policy's mean
//! relative delta against a baseline policy over the points.

mod config;
mod metrics;
mod report;
mod rng;
mod sweep;
mod trace;
mod world;

use std::error::Error;
use std::fmt;

use dorfq_core::{BatchState, CiStatus, Forge, ForgeError, Observation, Plan, Scheduler};
use dorfq_scheduler::SchedulerConfig;

pub use config::{Admission, Minutes, Policy, SimConfig, SimConfigError, WorldConfig};
pub use metrics::{End, Exit, Metrics, PrCiRecord, PrRecord, RunRecord, RunningSeconds};
pub use report::{PolicyReport, Report, SCHEMA};
pub use rng::{SplitMix64, Streams};
pub use sweep::{
    Axis, Point, PointReport, SWEEP_SCHEMA, SweepConfig, SweepConfigError, SweepReport, sweep,
};
pub use trace::{PlannedPr, PlannedStart, Rebase, TraceEvent};
pub use world::World;

/// Non-empty plans at one instant after which a scheduler is livelocked.
const SAME_INSTANT_LIMIT: u32 = 100;

/// Seconds in an hour.
const HOUR: u64 = 3600;

/// Why a replication ended without draining or reaching its deadline: the
/// policy's failure, at a time in seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimError {
    /// The plan stayed non-empty at one instant too many times.
    Livelock { at: u64 },
    /// Nothing is scheduled to happen, yet something is queued, tracked or
    /// out of the queue.
    Stuck {
        at: u64,
        queued: usize,
        tracked: usize,
        out: usize,
    },
    /// The scheduler broke a contract rule.
    Violation { at: u64, message: String },
    /// The scheduler returned an error instead of a plan.
    Scheduler { at: u64, message: String },
}

impl fmt::Display for SimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SimError::Livelock { at } => write!(f, "livelock at t={at}"),
            SimError::Stuck {
                at,
                queued,
                tracked,
                out,
            } => write!(
                f,
                "stuck at t={at}: {queued} queued, {tracked} batches tracked, {out} out of the queue"
            ),
            SimError::Violation { at, message } => write!(f, "violation at t={at}: {message}"),
            SimError::Scheduler { at, message } => {
                write!(f, "scheduler failed at t={at}: {message}")
            }
        }
    }
}

impl Error for SimError {}

/// Runs the scheduler `scheduler` describes for replication `k` of `seed`
/// in `world` under `admission`, until the world empties, it stalls or it
/// reaches its deadline, appending its steps and events to
/// `trace` when given, failed replication or not. `world` must have passed
/// [`WorldConfig::validate`] and `scheduler` [`SchedulerConfig::validate`].
pub fn run(
    world: &WorldConfig,
    admission: Admission,
    scheduler: &SchedulerConfig,
    seed: u64,
    k: u64,
    trace: Option<&mut Vec<TraceEvent>>,
) -> Result<Metrics, SimError> {
    let (world, outcome) = replicate(world, admission, scheduler, seed, k, trace);
    outcome.map(|end| world.metrics(end))
}

/// One replication as [`run`] runs it, returning the world as it was left
/// with the outcome.
fn replicate(
    config: &WorldConfig,
    admission: Admission,
    scheduler: &SchedulerConfig,
    seed: u64,
    k: u64,
    trace: Option<&mut Vec<TraceEvent>>,
) -> (World, Result<End, SimError>) {
    let deadline = (config.hours + config.drain_hours) * HOUR;
    let stall = config.stall_hours * HOUR;
    let mut world = World::new(config.clone(), admission, Streams::for_replication(seed, k));
    if trace.is_some() {
        world = world.traced();
    }
    let mut scheduler = scheduler.build();
    let outcome = drive(&mut world, scheduler.as_mut(), deadline, stall, trace);
    debug_assert_eq!(world.check_invariants(), Ok(()));
    (world, outcome)
}

/// Runs every policy of `config` at every replication. A failed
/// replication is that policy's result, not an error. With `trace`, the
/// first replication of each policy is traced.
pub fn simulate(config: &SimConfig, trace: bool) -> Report {
    let policies = config
        .policy
        .iter()
        .map(|policy| {
            let mut events = Vec::new();
            let replications = (0..config.replications)
                .map(|k| {
                    let events = (trace && k == 0).then_some(&mut events);
                    let admission = policy.admission;
                    run(
                        &config.world,
                        admission,
                        &policy.scheduler,
                        config.seed,
                        k,
                        events,
                    )
                })
                .collect();
            PolicyReport {
                name: policy.name.clone(),
                replications,
                trace: events,
            }
        })
        .collect();
    Report { policies }
}

/// The loop: observe, step, execute in rule 7's order, then advance to the
/// next event once a plan is empty. The replication runs until the world is
/// empty with no event left, and ends at the instant the world last
/// emptied; or it is cut off with the clock at `deadline`, or at `stall`
/// seconds after the last progress — a landing, an ejection, or an empty
/// world — while a pull request is present.
fn drive(
    world: &mut World,
    scheduler: &mut dyn Scheduler,
    deadline: u64,
    stall: u64,
    mut trace: Option<&mut Vec<TraceEvent>>,
) -> Result<End, SimError> {
    let mut same_instant = 0;
    let mut empty_since = world.now();
    let mut emptied_at = None;
    loop {
        let at = world.now();
        // The world is this crate's own deterministic forge: an observation
        // it cannot produce is a simulator bug, not the policy's outcome.
        let observation = world
            .observe()
            .unwrap_or_else(|e| panic!("the simulated world failed to observe at t={at}: {e}"));
        if let Some(events) = trace.as_deref_mut() {
            events.extend(world.take_trace());
        }
        let empty =
            observation.prs.is_empty() && observation.batches.is_empty() && world.out() == 0;
        // A departed pull request's own CI run still runs, and is billed,
        // after the world empties; the replication ended when it emptied.
        if !empty {
            emptied_at = None;
        } else if emptied_at.is_none() {
            emptied_at = Some(at);
        }
        let plan = scheduler
            .step(&observation)
            .map_err(|e| SimError::Scheduler {
                at,
                message: e.to_string(),
            })?;
        if let Some(events) = trace.as_deref_mut() {
            events.push(step_event(at, &observation, &plan));
        }
        execute(world, &plan).map_err(|e| failure(at, e))?;
        if !plan.is_empty() {
            same_instant += 1;
            if same_instant > SAME_INSTANT_LIMIT {
                return Err(SimError::Livelock { at });
            }
            continue;
        }
        same_instant = 0;
        let stalled_at = if empty {
            u64::MAX
        } else {
            world.last_progress().max(empty_since).saturating_add(stall)
        };
        match (world.next_event(), emptied_at) {
            (None, Some(emptied)) => return Ok(End::drained(emptied)),
            (None, None) => {
                return Err(SimError::Stuck {
                    at,
                    queued: observation.prs.len(),
                    tracked: observation.batches.len(),
                    out: world.out(),
                });
            }
            (Some(t), emptied) if t > deadline.min(stalled_at) => {
                let at = deadline.min(stalled_at);
                world.advance_to(at);
                return Ok(emptied.map_or(
                    End {
                        at,
                        cut_off: true,
                        stalled: stalled_at < deadline,
                    },
                    End::drained,
                ));
            }
            (Some(t), _) => {
                world.advance_to(t);
                if empty {
                    empty_since = t;
                }
            }
        }
    }
}

/// Executes `plan` in rule 7's order, stopping at the first error.
fn execute(forge: &mut dyn Forge, plan: &Plan) -> Result<(), ForgeError> {
    for &b in &plan.land {
        forge.land(b)?;
    }
    for n in &plan.notify {
        forge.notify(n.pr, &n.reason)?;
    }
    for s in &plan.start {
        forge.start_batch(s)?;
    }
    for e in &plan.eject {
        forge.eject(e.pr, &e.reason)?;
    }
    for &b in &plan.retire {
        forge.retire(b)?;
    }
    Ok(())
}

fn failure(at: u64, error: ForgeError) -> SimError {
    let message = match error {
        ForgeError::Transient(e) | ForgeError::Permanent(e) => e.to_string(),
    };
    SimError::Violation { at, message }
}

/// The trace event for a step.
fn step_event(t: u64, o: &Observation, plan: &Plan) -> TraceEvent {
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
    TraceEvent::Step {
        t,
        queued: o.prs.len(),
        in_flight,
        land: plan.land.iter().map(|b| b.0).collect(),
        notify: plan
            .notify
            .iter()
            .map(|n| PlannedPr {
                pr: n.pr.0,
                reason: n.reason.to_string(),
            })
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
            .map(|e| PlannedPr {
                pr: e.pr.0,
                reason: e.reason.to_string(),
            })
            .collect(),
        retire: plan.retire.iter().map(|b| b.0).collect(),
    }
}

#[cfg(test)]
mod tests;
