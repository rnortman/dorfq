//! DorfQ's simulator: any [`Scheduler`] run against a seeded, event-driven
//! world of pull requests with hidden truths.
//!
//! The [`World`] is a [`Forge`]: it checks rules 5, 6 and 8 of the
//! [`dorfq_core`] contract through its [`Ledger`](dorfq_core::ledger::Ledger)
//! and answers a breach with [`ForgeError::Permanent`]. Behind it each pull request is bad or good,
//! conflicts with trunk or not, and carries a key from which the truth of
//! every pair is a function. Runs take a uniform time on a fixed number of
//! runners and fail when their tree is broken or the flake draw fires.
//! Time is the simulator's: the scheduler never sees it.
//!
//! An arrival may be cut from a recent arrival's branch; it is stacked on
//! that pull request while it is queued, and may be `Atomic`. A base that
//! leaves without landing takes everything stacked on it with it
//! (`Blocked`), and a re-push restacks every descendant.
//!
//! Every draw comes from SplitMix64 streams split by purpose (arrivals,
//! truth, CI, churn, stacks), so two policies at one seed see the same
//! arrivals, the same truths and the same cut-from targets. Whether a cut
//! arrival stacks is the policy's doing: one that has already landed or
//! ejected the target sees it arrive on trunk.
//!
//! [`simulate`] runs every policy of a [`SimConfig`] at every replication
//! and returns a [`Report`]: runner-hours per landed pull request, time to
//! land and to reject, wasted runner time and false ejections, one column
//! per policy. Traced, the first replication of each policy is recorded as
//! [`TraceEvent`]s: every step and its plan, and everything the world did.
//! [`Report::to_json`] writes the report, its configuration and those
//! traces as one JSON document, the input of the report site.

mod config;
mod metrics;
mod report;
mod rng;
mod trace;
mod world;

use std::error::Error;
use std::fmt;

use dorfq_core::{BatchState, CiStatus, Forge, ForgeError, Observation, Plan, Scheduler};
use dorfq_scheduler::SchedulerConfig;

pub use config::{CiMinutes, Policy, SimConfig, SimConfigError, WorldConfig};
pub use metrics::{Exit, Metrics, PrRecord, RunRecord};
pub use report::{PolicyReport, Report, SCHEMA};
pub use rng::{SplitMix64, Streams};
pub use trace::{PlannedEject, PlannedStart, TraceEvent, Truth};
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
    /// Nothing is scheduled to happen, yet something is queued or tracked.
    Stuck {
        at: u64,
        queued: usize,
        tracked: usize,
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
            } => write!(
                f,
                "stuck at t={at}: {queued} queued, {tracked} batches tracked"
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
/// in `world`, to drain or deadline, appending its steps and events to
/// `trace` when given, failed replication or not. `world` must have passed
/// [`WorldConfig::validate`] and `scheduler` [`SchedulerConfig::validate`].
pub fn run(
    world: &WorldConfig,
    scheduler: &SchedulerConfig,
    seed: u64,
    k: u64,
    trace: Option<&mut Vec<TraceEvent>>,
) -> Result<Metrics, SimError> {
    let (world, outcome) = replicate(world, scheduler, seed, k, trace);
    outcome.map(|()| world.metrics())
}

/// One replication as [`run`] runs it, returning the world as it was left
/// with the outcome.
fn replicate(
    config: &WorldConfig,
    scheduler: &SchedulerConfig,
    seed: u64,
    k: u64,
    trace: Option<&mut Vec<TraceEvent>>,
) -> (World, Result<(), SimError>) {
    let deadline = (config.hours + config.drain_hours) * HOUR;
    let mut world = World::new(config.clone(), Streams::for_replication(seed, k));
    if trace.is_some() {
        world = world.traced();
    }
    let mut scheduler = scheduler.build();
    let outcome = drive(&mut world, scheduler.as_mut(), deadline, trace);
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
                    run(&config.world, &policy.scheduler, config.seed, k, events)
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
/// next event once a plan is empty. A replication cut off at `deadline`
/// ends with the clock at the deadline.
fn drive(
    world: &mut World,
    scheduler: &mut dyn Scheduler,
    deadline: u64,
    mut trace: Option<&mut Vec<TraceEvent>>,
) -> Result<(), SimError> {
    let mut same_instant = 0;
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
        match world.next_event() {
            None if observation.prs.is_empty() && observation.batches.is_empty() => return Ok(()),
            None => {
                return Err(SimError::Stuck {
                    at,
                    queued: observation.prs.len(),
                    tracked: observation.batches.len(),
                });
            }
            Some(t) if t > deadline => {
                world.advance_to(deadline);
                return Ok(());
            }
            Some(t) => world.advance_to(t),
        }
    }
}

/// Executes `plan` in rule 7's order, stopping at the first error.
fn execute(forge: &mut dyn Forge, plan: &Plan) -> Result<(), ForgeError> {
    for &b in &plan.land {
        forge.land(b)?;
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
            .map(|e| PlannedEject {
                pr: e.pr.0,
                reason: e.reason.to_string(),
            })
            .collect(),
        retire: plan.retire.iter().map(|b| b.0).collect(),
    }
}

#[cfg(test)]
mod tests;
