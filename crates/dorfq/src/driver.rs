//! The loop body: observe, step, execute.

use std::fmt;

use dorfq_core::{
    BatchState, CiStatus, ConflictWith, EjectReason, Forge, ForgeError, LandRefusal, Mergeable,
    Observation, Plan, Scheduler,
};

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
    /// `land`, `start`, `eject`, `retire`, stopping at the first error. An
    /// error means the rest of the plan was abandoned; the tick still counts.
    pub fn tick(&mut self) -> Result<TickReport, ForgeError> {
        let tick = self.tick;
        self.tick += 1;
        let observation = self.forge.observe()?;
        let plan = self.scheduler.step(&observation);
        for &batch in &plan.land {
            self.forge.land(batch)?;
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

fn mergeable(m: Mergeable) -> &'static str {
    match m {
        Mergeable::Clean => "clean",
        Mergeable::Conflicting => "conflicting",
        Mergeable::Unknown => "unknown",
    }
}

fn ci(c: CiStatus) -> &'static str {
    match c {
        CiStatus::Pending => "pending",
        CiStatus::Passed => "passed",
        CiStatus::Failed => "failed",
    }
}

/// One line for the tick and what it observed, then one line per effect in
/// execution order.
impl fmt::Display for TickReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let obs = &self.observation;
        write!(f, "tick {} trunk {}", self.tick, obs.trunk)?;
        for pr in &obs.prs {
            write!(
                f,
                " | pr {} {} {} ci {}",
                pr.number.0,
                pr.head,
                mergeable(pr.mergeable),
                ci(pr.head_ci)
            )?;
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
                BatchState::Conflict { pr, with } => match with {
                    ConflictWith::Trunk => write!(f, "conflict: pr {} with trunk", pr.0)?,
                    ConflictWith::Pr(q) => write!(f, "conflict: pr {} with pr {}", pr.0, q.0)?,
                },
                BatchState::Testing { tip, ci: c } => write!(f, "testing {tip} ci {}", ci(*c))?,
                BatchState::Landed { tip } => write!(f, "landed {tip}")?,
                BatchState::LandRefused { tip, reason } => match reason {
                    LandRefusal::TrunkMoved { trunk } => {
                        write!(f, "land refused {tip}: trunk moved to {trunk}")?
                    }
                    LandRefusal::MemberGone { pr } => {
                        write!(f, "land refused {tip}: pr {} gone", pr.0)?
                    }
                },
            }
        }
        if self.idle {
            f.write_str(" | idle")?;
        }
        let plan = &self.plan;
        for batch in &plan.land {
            write!(f, "\n  land batch {}", batch.0)?;
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
            write!(f, "\n  eject pr {}: ", ejection.pr.0)?;
            match &ejection.reason {
                EjectReason::ConflictsWithTrunk => f.write_str("conflicts with trunk")?,
                EjectReason::ConflictsWith { pr } => write!(f, "conflicts with pr {}", pr.0)?,
                EjectReason::BatchFailed { batch } => write!(f, "batch {} failed", batch.0)?,
            }
        }
        for batch in &plan.retire {
            write!(f, "\n  retire batch {}", batch.0)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
