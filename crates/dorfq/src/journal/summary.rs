//! The summary: what an event stream adds up to.
//!
//! A name is the simulator's only where its definition is the same, given
//! the journal's tick resolution; a quantity only a real queue has, or one
//! defined differently, has its own name. A departure ends a lifetime and a
//! number that comes back is a new one. A `blocked` departure counts under
//! the first exit down its chain of bases that is not a block, and under
//! `left_with_base` as well.

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;

use super::events::{Event, Kind};

pub(crate) const HOUR: f64 = 3600.0;

/// What a journal's events add up to; `None` where the set it summarises is
/// empty.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    /// The last event's time less the first's.
    pub window_hours: f64,
    pub ticks: usize,
    pub restarts: usize,
    pub errors: usize,
    /// Lifetimes begun.
    pub arrived: usize,
    pub landed: usize,
    /// Departures by a hand other than the queue's.
    pub merged_outside: usize,
    pub ejected: usize,
    pub withdrawn: usize,
    pub left_with_base: usize,
    /// Departures no observation explained.
    pub lost: usize,
    /// Lifetimes still going at the end of the input.
    pub unfinished: usize,
    pub repushes: usize,
    /// Pull requests becoming conflicting.
    pub conflicts: usize,
    /// Batches started that ran CI: a batch whose build conflicted never
    /// ran, and counts in no run figure.
    pub runs: usize,
    pub cancelled_runs: usize,
    pub passed_runs: usize,
    pub failed_runs: usize,
    pub probes: usize,
    pub refused_landings: usize,
    pub mean_batch_size: Option<f64>,
    /// Batch run time: to the verdict, or to the cancelling retirement; a
    /// run still pending at the end counts up to the end.
    pub batch_runner_hours: f64,
    pub batch_runner_hours_per_landed: Option<f64>,
    /// The share of batch run time spent on batches that never landed; runs
    /// still pending at the end count toward the whole and not the waste.
    pub wasted_fraction: Option<f64>,
    pub batch_run_p50_hours: Option<f64>,
    pub batch_run_p95_hours: Option<f64>,
    pub land_p50_hours: Option<f64>,
    pub land_p95_hours: Option<f64>,
    pub eject_p50_hours: Option<f64>,
    pub eject_p95_hours: Option<f64>,
    pub own_ci_p50_hours: Option<f64>,
    pub own_ci_p95_hours: Option<f64>,
    pub notices: usize,
    /// Per check name, how many failed runs named it.
    pub failed_checks: BTreeMap<String, usize>,
}

/// How a lifetime ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Landed,
    Ejected,
    Withdrawn,
    Blocked { on: u64 },
    Merged,
    Lost,
}

struct Lifetime {
    pr: u64,
    arrived: u64,
    left: Option<(u64, Exit)>,
}

struct Run {
    started: u64,
    members: usize,
    /// Its build conflicted, so it never ran.
    conflicted: bool,
    ended: Option<u64>,
    /// Ended with a verdict rather than a retirement.
    verdict: bool,
    landed: bool,
}

// TODO(journal-fit): `dorfq journal fit`, a simulator `[world]` table from
// these rates and durations.
/// Summarises `events`, in the order derived.
pub fn summarise(events: &[Event]) -> Summary {
    let mut s = Summary {
        window_hours: match (events.first(), events.last()) {
            (Some(first), Some(last)) => last.t.saturating_sub(first.t) as f64 / HOUR,
            _ => 0.0,
        },
        ticks: 0,
        restarts: 0,
        errors: 0,
        arrived: 0,
        landed: 0,
        merged_outside: 0,
        ejected: 0,
        withdrawn: 0,
        left_with_base: 0,
        lost: 0,
        unfinished: 0,
        repushes: 0,
        conflicts: 0,
        runs: 0,
        cancelled_runs: 0,
        passed_runs: 0,
        failed_runs: 0,
        probes: 0,
        refused_landings: 0,
        mean_batch_size: None,
        batch_runner_hours: 0.0,
        batch_runner_hours_per_landed: None,
        wasted_fraction: None,
        batch_run_p50_hours: None,
        batch_run_p95_hours: None,
        land_p50_hours: None,
        land_p95_hours: None,
        eject_p50_hours: None,
        eject_p95_hours: None,
        own_ci_p50_hours: None,
        own_ci_p95_hours: None,
        notices: 0,
        failed_checks: BTreeMap::new(),
    };
    let end = events.last().map_or(0, |e| e.t);
    let mut lives: Vec<Lifetime> = Vec::new();
    // The open lifetime of each pull request, by index into `lives`.
    let mut open: BTreeMap<u64, usize> = BTreeMap::new();
    let mut runs: Vec<Run> = Vec::new();
    // The current run of each batch id, by index into `runs`.
    let mut current: BTreeMap<u64, usize> = BTreeMap::new();
    let mut own_ci: BTreeMap<u64, u64> = BTreeMap::new();
    let mut own_ci_hours = Vec::new();
    for event in events {
        let t = event.t;
        match &event.kind {
            Kind::Arrive { pr, .. } => {
                // A lifetime still open ended out of sight.
                if open.contains_key(pr) {
                    s.lost += 1;
                    leave(&mut lives, &mut open, t, *pr, Exit::Lost);
                }
                open.insert(*pr, lives.len());
                lives.push(Lifetime {
                    pr: *pr,
                    arrived: t,
                    left: None,
                });
            }
            Kind::Repush { .. } => s.repushes += 1,
            Kind::Conflicting { .. } => s.conflicts += 1,
            Kind::PrCi { pr } => {
                own_ci.insert(*pr, t);
            }
            Kind::PrCiEnd { pr, .. } => {
                if let Some(started) = own_ci.remove(pr) {
                    own_ci_hours.push(t.saturating_sub(started) as f64 / HOUR);
                }
            }
            Kind::Withdraw { pr, .. } => leave(&mut lives, &mut open, t, *pr, Exit::Withdrawn),
            Kind::Blocked { pr, on, .. } => {
                leave(&mut lives, &mut open, t, *pr, Exit::Blocked { on: *on })
            }
            Kind::Merged { pr } => {
                s.merged_outside += 1;
                leave(&mut lives, &mut open, t, *pr, Exit::Merged);
            }
            Kind::Eject { pr, .. } => leave(&mut lives, &mut open, t, *pr, Exit::Ejected),
            Kind::Lost { pr } => {
                s.lost += 1;
                leave(&mut lives, &mut open, t, *pr, Exit::Lost);
            }
            Kind::Start {
                batch,
                members: m,
                probe,
                ..
            } => {
                s.probes += usize::from(*probe);
                current.insert(*batch, runs.len());
                runs.push(Run {
                    started: t,
                    members: m.len(),
                    conflicted: false,
                    ended: None,
                    verdict: false,
                    landed: false,
                });
            }
            Kind::RunEnd {
                batch,
                passed,
                checks,
            } => {
                if *passed {
                    s.passed_runs += 1;
                } else {
                    s.failed_runs += 1;
                    for check in checks {
                        *s.failed_checks.entry(check.clone()).or_default() += 1;
                    }
                }
                if let Some(run) = current.get(batch).map(|&i| &mut runs[i])
                    && run.ended.is_none()
                {
                    run.ended = Some(t);
                    run.verdict = true;
                }
            }
            Kind::Land { batch, members } => {
                if let Some(&i) = current.get(batch) {
                    runs[i].landed = true;
                }
                for pr in members {
                    leave(&mut lives, &mut open, t, *pr, Exit::Landed);
                }
            }
            Kind::Refused { .. } => s.refused_landings += 1,
            Kind::Conflict { batch, .. } => {
                if let Some(&i) = current.get(batch) {
                    runs[i].conflicted = true;
                }
            }
            Kind::Retire { batch, cancelled } => {
                s.cancelled_runs += usize::from(*cancelled);
                if let Some(i) = current.remove(batch)
                    && runs[i].ended.is_none()
                {
                    runs[i].ended = Some(t);
                }
            }
            Kind::Notice { .. } => s.notices += 1,
            Kind::Step { .. } => s.ticks += 1,
            Kind::Error { .. } => s.errors += 1,
            Kind::Restart { .. } => s.restarts += 1,
            Kind::Admit { .. } | Kind::Held { .. } | Kind::Trunk { .. } => {}
        }
    }

    s.arrived = lives.len();
    s.unfinished = open.len();
    let mut land_hours = Vec::new();
    let mut eject_hours = Vec::new();
    for life in &lives {
        let Some((left, exit)) = life.left else {
            continue;
        };
        let hours = left.saturating_sub(life.arrived) as f64 / HOUR;
        if let Exit::Blocked { .. } = exit {
            s.left_with_base += 1;
        }
        match root(&lives, life) {
            Some(Exit::Landed) => {
                s.landed += 1;
                land_hours.push(hours);
            }
            Some(Exit::Ejected) => {
                s.ejected += 1;
                eject_hours.push(hours);
            }
            Some(Exit::Withdrawn) => s.withdrawn += 1,
            // A block rooted in a departure the queue did not make, or in
            // one outside the input, counts under `left_with_base` alone.
            Some(Exit::Merged | Exit::Lost | Exit::Blocked { .. }) | None => {}
        }
    }

    let mut consumed = 0;
    let mut wasted = 0;
    let mut run_hours = Vec::new();
    let runs: Vec<&Run> = runs.iter().filter(|run| !run.conflicted).collect();
    let members: usize = runs.iter().map(|run| run.members).sum();
    for run in &runs {
        let seconds = run.ended.unwrap_or(end).saturating_sub(run.started);
        consumed += seconds;
        if run.ended.is_some() && !run.landed {
            wasted += seconds;
        }
        if run.verdict {
            run_hours.push(seconds as f64 / HOUR);
        }
    }
    s.runs = runs.len();
    s.mean_batch_size = ratio(members as f64, runs.len() as f64);
    s.batch_runner_hours = consumed as f64 / HOUR;
    s.batch_runner_hours_per_landed = ratio(s.batch_runner_hours, s.landed as f64);
    s.wasted_fraction = ratio(wasted as f64, consumed as f64);
    (s.batch_run_p50_hours, s.batch_run_p95_hours) = percentiles(run_hours);
    (s.land_p50_hours, s.land_p95_hours) = percentiles(land_hours);
    (s.eject_p50_hours, s.eject_p95_hours) = percentiles(eject_hours);
    (s.own_ci_p50_hours, s.own_ci_p95_hours) = percentiles(own_ci_hours);
    s
}

/// Ends `pr`'s open lifetime, if it has one, at `t` by `exit`.
fn leave(lives: &mut [Lifetime], open: &mut BTreeMap<u64, usize>, t: u64, pr: u64, exit: Exit) {
    if let Some(i) = open.remove(&pr) {
        lives[i].left = Some((t, exit));
    }
}

/// The exit `life` counts under: its own, or for a block, the first exit
/// down its chain of bases that is not a block. `None` when a base's exit
/// is not in the input.
fn root(lives: &[Lifetime], life: &Lifetime) -> Option<Exit> {
    let (mut at, mut exit) = life.left?;
    // A chain is no longer than the lifetimes; a longer one is a cycle.
    for _ in 0..lives.len() {
        let Exit::Blocked { on } = exit else {
            return Some(exit);
        };
        // The base's lifetime that ended last, at or before the block.
        (at, exit) = lives
            .iter()
            .filter(|l| l.pr == on)
            .filter_map(|l| l.left)
            .filter(|(left, _)| *left <= at)
            .max_by_key(|(left, _)| *left)?;
    }
    None
}

/// The nearest-rank 50th and 95th percentiles of `values`.
pub(super) fn percentiles(mut values: Vec<f64>) -> (Option<f64>, Option<f64>) {
    values.sort_by(f64::total_cmp);
    let at = |p: f64| {
        let rank = ((p * values.len() as f64).ceil() as usize).max(1);
        values.get(rank - 1).copied()
    };
    (at(0.5), at(0.95))
}

fn ratio(numerator: f64, denominator: f64) -> Option<f64> {
    (denominator > 0.0).then(|| numerator / denominator)
}

impl fmt::Display for Summary {
    /// One row per name, in the order defined; `failed_checks` one row per
    /// check, `-` where a value is undefined.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = |n: usize| n.to_string();
        let hours = |h: f64| format!("{h:.2}");
        let maybe = |v: Option<f64>, decimals: usize| {
            v.map_or_else(|| "-".to_owned(), |v| format!("{v:.decimals$}"))
        };
        let rows: [(&str, String); 33] = [
            ("window_hours", hours(self.window_hours)),
            ("ticks", count(self.ticks)),
            ("restarts", count(self.restarts)),
            ("errors", count(self.errors)),
            ("arrived", count(self.arrived)),
            ("landed", count(self.landed)),
            ("merged_outside", count(self.merged_outside)),
            ("ejected", count(self.ejected)),
            ("withdrawn", count(self.withdrawn)),
            ("left_with_base", count(self.left_with_base)),
            ("lost", count(self.lost)),
            ("unfinished", count(self.unfinished)),
            ("repushes", count(self.repushes)),
            ("conflicts", count(self.conflicts)),
            ("runs", count(self.runs)),
            ("cancelled_runs", count(self.cancelled_runs)),
            ("passed_runs", count(self.passed_runs)),
            ("failed_runs", count(self.failed_runs)),
            ("probes", count(self.probes)),
            ("refused_landings", count(self.refused_landings)),
            ("mean_batch_size", maybe(self.mean_batch_size, 2)),
            ("batch_runner_hours", hours(self.batch_runner_hours)),
            (
                "batch_runner_hours_per_landed",
                maybe(self.batch_runner_hours_per_landed, 2),
            ),
            ("wasted_fraction", maybe(self.wasted_fraction, 3)),
            ("batch_run_p50_hours", maybe(self.batch_run_p50_hours, 2)),
            ("batch_run_p95_hours", maybe(self.batch_run_p95_hours, 2)),
            ("land_p50_hours", maybe(self.land_p50_hours, 2)),
            ("land_p95_hours", maybe(self.land_p95_hours, 2)),
            ("eject_p50_hours", maybe(self.eject_p50_hours, 2)),
            ("eject_p95_hours", maybe(self.eject_p95_hours, 2)),
            ("own_ci_p50_hours", maybe(self.own_ci_p50_hours, 2)),
            ("own_ci_p95_hours", maybe(self.own_ci_p95_hours, 2)),
            ("notices", count(self.notices)),
        ];
        let width = rows
            .iter()
            .map(|(name, _)| name.len())
            .chain(
                self.failed_checks
                    .keys()
                    .map(|c| "failed_checks ".len() + c.len()),
            )
            .max()
            .unwrap_or(0);
        for (name, value) in &rows {
            writeln!(f, "{name:<width$}  {value:>8}")?;
        }
        for (check, n) in &self.failed_checks {
            let name = format!("failed_checks {check}");
            writeln!(f, "{name:<width$}  {n:>8}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests;
