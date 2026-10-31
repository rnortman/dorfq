//! The loop body: observe, step, execute.

use std::error::Error;
use std::fmt;

use dorfq_core::{
    BatchState, CiStatus, Forge, ForgeError, Observation, Plan, PrBase, PrStatus, Scheduler,
    SchedulerError,
};
use serde::{Deserialize, Serialize};

/// Why a tick ended early.
#[derive(Debug)]
pub enum DriverError {
    /// `observe` failed; nothing was stepped.
    Observe(ForgeError),
    /// The scheduler could not produce a plan; always fatal.
    Scheduler(SchedulerError),
    /// The scheduler stepped and an effect then failed: the plan ran up to
    /// that effect (rule 7), and `report` is what the tick saw and decided.
    Effect {
        report: Box<TickReport>,
        error: ForgeError,
    },
}

impl DriverError {
    /// Whether the process must stop: a `Permanent` forge error or a
    /// scheduler error.
    pub fn is_fatal(&self) -> bool {
        match self {
            DriverError::Observe(error) | DriverError::Effect { error, .. } => {
                matches!(error, ForgeError::Permanent(_))
            }
            DriverError::Scheduler(_) => true,
        }
    }

    /// What the tick saw and decided, when it got as far as a plan.
    pub fn report(&self) -> Option<&TickReport> {
        match self {
            DriverError::Effect { report, .. } => Some(report),
            DriverError::Observe(_) | DriverError::Scheduler(_) => None,
        }
    }
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriverError::Observe(e) | DriverError::Effect { error: e, .. } => e.fmt(f),
            DriverError::Scheduler(e) => e.fmt(f),
        }
    }
}

impl Error for DriverError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            DriverError::Observe(e) | DriverError::Effect { error: e, .. } => Some(e),
            DriverError::Scheduler(e) => Some(e),
        }
    }
}

/// Runs ticks of a forge and a scheduler it borrows.
pub struct Driver<'a> {
    forge: &'a mut dyn Forge,
    scheduler: &'a mut dyn Scheduler,
    tick: u64,
}

/// What one tick saw and decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickReport {
    /// The tick's number, counting from 0.
    pub tick: u64,
    pub observation: Observation,
    pub plan: Plan,
    /// True when the observation has no pull requests and no batches and
    /// the plan is empty.
    pub idle: bool,
}

impl<'a> Driver<'a> {
    /// A driver whose first tick is tick 0.
    pub fn new(forge: &'a mut dyn Forge, scheduler: &'a mut dyn Scheduler) -> Driver<'a> {
        Driver {
            forge,
            scheduler,
            tick: 0,
        }
    }

    /// How many ticks have run, the one that failed included.
    pub fn ticks(&self) -> u64 {
        self.tick
    }

    /// Observes, steps the scheduler, and executes the plan in the order
    /// `land`, `notify`, `start`, `eject`, `retire`, stopping at the first
    /// error. An error means the rest of the plan was abandoned, or none was
    /// made; the tick still counts.
    pub fn tick(&mut self) -> Result<TickReport, DriverError> {
        let tick = self.tick;
        self.tick += 1;
        let observation = self.forge.observe().map_err(DriverError::Observe)?;
        let plan = self
            .scheduler
            .step(&observation)
            .map_err(DriverError::Scheduler)?;
        let idle = observation.prs.is_empty() && observation.batches.is_empty() && plan.is_empty();
        let report = TickReport {
            tick,
            observation,
            plan,
            idle,
        };
        match self.execute(&report.plan) {
            Ok(()) => Ok(report),
            Err(error) => Err(DriverError::Effect {
                report: Box::new(report),
                error,
            }),
        }
    }

    fn execute(&mut self, plan: &Plan) -> Result<(), ForgeError> {
        for &batch in &plan.land {
            self.forge.land(batch)?;
        }
        for notice in &plan.notify {
            self.forge.notify(notice.pr, &notice.reason)?;
        }
        for batch in &plan.start {
            self.forge.start_batch(batch)?;
        }
        for ejection in &plan.eject {
            self.forge.eject(ejection.pr, &ejection.reason)?;
        }
        for &batch in &plan.retire {
            self.forge.retire(batch)?;
        }
        Ok(())
    }
}

impl TickReport {
    /// Whether this tick is worth no line after one that printed
    /// `previous`: its plan is empty and its observation equals `previous`
    /// in everything but [`Observation::now`].
    pub fn repeats(&self, previous: &Observation) -> bool {
        let mut previous = previous.clone();
        previous.now = self.observation.now;
        self.plan.is_empty() && previous == self.observation
    }
}

/// A pull request's status as the tick line words it: `ready`,
/// `waiting:<holds>`, `conflicting:<with>`.
pub(crate) fn status(s: &PrStatus) -> String {
    match s {
        PrStatus::Ready => "ready".to_owned(),
        PrStatus::Waiting(holds) => {
            let holds: Vec<String> = holds.iter().map(ToString::to_string).collect();
            format!("waiting:{}", holds.join(","))
        }
        PrStatus::Conflicting(with) => format!("conflicting:{with}"),
    }
}

fn ci(c: &CiStatus) -> String {
    match c {
        CiStatus::Pending => "pending".to_owned(),
        CiStatus::Passed => "passed".to_owned(),
        CiStatus::Failed(failure) if failure.checks.is_empty() => "failed".to_owned(),
        CiStatus::Failed(failure) => {
            let checks: Vec<&str> = failure.checks.iter().map(|c| c.0.as_str()).collect();
            format!("failed:{}", checks.join(","))
        }
    }
}

/// One line for the tick and what it observed, then one line per effect in
/// execution order.
impl fmt::Display for TickReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let obs = &self.observation;
        write!(
            f,
            "tick {} now {} trunk {}",
            self.tick, obs.now.0, obs.trunk
        )?;
        for pr in &obs.prs {
            write!(f, " | pr {} {}", pr.number.0, pr.head)?;
            match pr.base {
                PrBase::Trunk => {}
                PrBase::Pr(base) => write!(f, " on #{}", base.0)?,
            }
            write!(f, " {}", status(&pr.status))?;
        }
        for batch in &obs.batches {
            let members: Vec<String> = batch.members.iter().map(|m| m.pr.0.to_string()).collect();
            write!(
                f,
                " | batch {} [{}] on {} ",
                batch.id.0,
                members.join(", "),
                batch.base
            )?;
            match &batch.state {
                BatchState::Conflict { pr, with } => {
                    write!(f, "conflict: pr {} with {with}", pr.0)?
                }
                BatchState::Testing { tip, ci: c } => write!(f, "testing {tip} ci {}", ci(c))?,
                BatchState::Landed { tip } => write!(f, "landed {tip}")?,
                BatchState::LandRefused { tip, reason } => {
                    write!(f, "land refused {tip}: {reason}")?
                }
            }
        }
        for departure in &obs.departed {
            write!(
                f,
                " | departed pr {} {} {}",
                departure.pr.0, departure.head, departure.how
            )?;
        }
        if self.idle {
            f.write_str(" | idle")?;
        }
        let plan = &self.plan;
        for batch in &plan.land {
            write!(f, "\n  land batch {}", batch.0)?;
        }
        for notice in &plan.notify {
            write!(f, "\n  notify pr {}: {}", notice.pr.0, notice.reason)?;
        }
        for start in &plan.start {
            let members: Vec<String> = start.prs.iter().map(|p| p.0.to_string()).collect();
            write!(
                f,
                "\n  start batch {} [{}] on {}",
                start.id.0,
                members.join(", "),
                start.base
            )?;
        }
        for ejection in &plan.eject {
            write!(f, "\n  eject pr {}: {}", ejection.pr.0, ejection.reason)?;
        }
        for batch in &plan.retire {
            write!(f, "\n  retire batch {}", batch.0)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
