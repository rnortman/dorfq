//! The loop body: observe, step, execute.

use std::error::Error;
use std::fmt;

use dorfq_core::{
    BatchState, CiStatus, Forge, ForgeError, Observation, Plan, PrBase, PrStatus, Scheduler,
    SchedulerError,
};

/// Why a tick ended early.
#[derive(Debug)]
pub enum DriverError {
    /// The scheduler could not produce a plan; always fatal.
    Scheduler(SchedulerError),
    /// A forge call failed; fatal when `Permanent`.
    Forge(ForgeError),
}

impl From<ForgeError> for DriverError {
    fn from(error: ForgeError) -> DriverError {
        DriverError::Forge(error)
    }
}

impl fmt::Display for DriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriverError::Scheduler(e) => e.fmt(f),
            DriverError::Forge(e) => e.fmt(f),
        }
    }
}

impl Error for DriverError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            DriverError::Scheduler(e) => Some(e),
            DriverError::Forge(e) => Some(e),
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

    /// Observes, steps the scheduler, and executes the plan in the order
    /// `land`, `notify`, `start`, `eject`, `retire`, stopping at the first
    /// error. An error means the rest of the plan was abandoned, or none was
    /// made; the tick still counts.
    pub fn tick(&mut self) -> Result<TickReport, DriverError> {
        let tick = self.tick;
        self.tick += 1;
        let observation = self.forge.observe()?;
        let plan = self
            .scheduler
            .step(&observation)
            .map_err(DriverError::Scheduler)?;
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
        let idle = observation.prs.is_empty() && observation.batches.is_empty() && plan.is_empty();
        Ok(TickReport {
            tick,
            observation,
            plan,
            idle,
        })
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

fn status(s: &PrStatus) -> String {
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
