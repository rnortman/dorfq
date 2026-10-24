//! Per-pull-request and per-run records, and one replication's summary.

use dorfq_core::{BatchId, CiStatus, EjectReason, PrNumber, Sha};
use serde::Serialize;

const HOUR: f64 = crate::HOUR as f64;

/// How a pull request left the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Landed,
    /// Ejected; `justified` per the world's truth at the moment of ejection.
    Ejected {
        reason: EjectReason,
        justified: bool,
    },
    /// Intent cleared.
    Withdrawn,
    /// Left because its base `on` left without landing.
    Blocked {
        on: PrNumber,
    },
}

/// One pull request's life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRecord {
    pub number: PrNumber,
    pub arrived_at: u64,
    /// When and how it left; `None` while in the world.
    pub left: Option<(u64, Exit)>,
    /// Rebases pushed: after a conflict, or a landed base under `github`
    /// admission.
    pub rebases: u32,
}

/// One run that reached a runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub batch: BatchId,
    /// Own members.
    pub members: usize,
    pub started_at: u64,
    /// Seconds of runner time used: the whole duration, or the elapsed time
    /// of a cancelled run.
    pub consumed: u64,
    /// The verdict, once the run completed; `None` while running or after a
    /// cancellation.
    pub verdict: Option<CiStatus>,
    /// The tree members not on trunk when it completed: its own members
    /// and those of every ancestor batch not yet landed. Empty until then.
    pub tree: Vec<PrNumber>,
    /// True once its batch landed.
    pub landed: bool,
}

/// One pull request's own CI run that reached a runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrCiRecord {
    pub pr: PrNumber,
    /// The head it ran on.
    pub head: Sha,
    pub started_at: u64,
    /// Seconds of runner time used: the whole duration once it completed,
    /// the elapsed time once cancelled, 0 while running.
    pub consumed: u64,
    /// True when a head change cut it short.
    pub cancelled: bool,
}

/// One replication's summary; `None` where the set it summarises is empty.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Metrics {
    pub arrived: usize,
    pub landed: usize,
    pub ejected_bad: usize,
    pub ejected_good: usize,
    /// Intent cleared.
    pub withdrawn: usize,
    /// Left because a base left without landing.
    pub blocked: usize,
    pub unfinished: usize,
    /// Rebases pushed, over every pull request.
    pub rebases: u64,
    /// Batch runs that reached a runner, cancelled included.
    pub runs: usize,
    pub mean_batch_size: Option<f64>,
    /// Pull requests' own CI runs that reached a runner, cancelled included.
    pub pr_ci_runs: usize,
    /// Runner time used by batch runs and PR CI runs alike: every finished
    /// or cancelled run's consumed time, plus the elapsed time of runs
    /// still running when the replication stopped.
    pub runner_hours: f64,
    /// The part of `runner_hours` PR CI runs used.
    pub pr_ci_hours: f64,
    pub runner_hours_per_landed: Option<f64>,
    /// The share of batch runs' runner time spent on batch runs that ended
    /// without landing (failed, cancelled, refused, passed on a dead base).
    /// Runs still running at the stop have no outcome and are not counted
    /// as waste. PR CI is not in it, on either side.
    pub wasted_fraction: Option<f64>,
    pub land_p50_hours: Option<f64>,
    pub land_p95_hours: Option<f64>,
    pub reject_p50_hours: Option<f64>,
    pub reject_p95_hours: Option<f64>,
    /// `runner_hours` over the runner time available from 0 to the stop:
    /// the moment the queue drained, or the deadline.
    pub utilisation: Option<f64>,
}

/// The nearest-rank percentile of `values`, which must be sorted.
fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let rank = ((p * values.len() as f64).ceil() as usize).max(1);
    Some(values[rank - 1])
}

fn ratio(numerator: f64, denominator: f64) -> Option<f64> {
    (denominator > 0.0).then(|| numerator / denominator)
}

/// Runner-seconds used so far by the runs still running, by kind of run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunningSeconds {
    pub batches: u64,
    pub pr_ci: u64,
}

impl Metrics {
    /// Summarises the records of a replication that ended at `elapsed`
    /// seconds on `runners` runners, with the runs still running having
    /// used `running`.
    pub fn summarise(
        prs: &[PrRecord],
        runs: &[RunRecord],
        pr_ci: &[PrCiRecord],
        running: RunningSeconds,
        runners: usize,
        elapsed: u64,
    ) -> Metrics {
        let mut landed = 0;
        let mut ejected_bad = 0;
        let mut ejected_good = 0;
        let mut withdrawn = 0;
        let mut blocked = 0;
        let mut unfinished = 0;
        let mut land_hours = Vec::new();
        let mut reject_hours = Vec::new();
        let rebases = prs.iter().map(|p| u64::from(p.rebases)).sum();
        for pr in prs {
            let Some((left_at, exit)) = &pr.left else {
                unfinished += 1;
                continue;
            };
            let hours = (left_at - pr.arrived_at) as f64 / HOUR;
            match exit {
                Exit::Landed => {
                    landed += 1;
                    land_hours.push(hours);
                }
                Exit::Ejected {
                    justified: true, ..
                } => {
                    ejected_bad += 1;
                    reject_hours.push(hours);
                }
                Exit::Ejected {
                    justified: false, ..
                } => ejected_good += 1,
                Exit::Withdrawn => withdrawn += 1,
                Exit::Blocked { .. } => blocked += 1,
            }
        }
        land_hours.sort_by(f64::total_cmp);
        reject_hours.sort_by(f64::total_cmp);
        let consumed: u64 = runs.iter().map(|r| r.consumed).sum::<u64>() + running.batches;
        let wasted: u64 = runs.iter().filter(|r| !r.landed).map(|r| r.consumed).sum();
        let members: usize = runs.iter().map(|r| r.members).sum();
        let pr_ci_consumed = pr_ci.iter().map(|r| r.consumed).sum::<u64>() + running.pr_ci;
        let runner_hours = (consumed + pr_ci_consumed) as f64 / HOUR;
        Metrics {
            arrived: prs.len(),
            landed,
            ejected_bad,
            ejected_good,
            withdrawn,
            blocked,
            unfinished,
            rebases,
            runs: runs.len(),
            mean_batch_size: ratio(members as f64, runs.len() as f64),
            pr_ci_runs: pr_ci.len(),
            runner_hours,
            pr_ci_hours: pr_ci_consumed as f64 / HOUR,
            runner_hours_per_landed: ratio(runner_hours, landed as f64),
            wasted_fraction: ratio(wasted as f64, consumed as f64),
            land_p50_hours: percentile(&land_hours, 0.5),
            land_p95_hours: percentile(&land_hours, 0.95),
            reject_p50_hours: percentile(&reject_hours, 0.5),
            reject_p95_hours: percentile(&reject_hours, 0.95),
            utilisation: ratio(runner_hours, runners as f64 * elapsed as f64 / HOUR),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, arrived_at: u64, left: Option<(u64, Exit)>) -> PrRecord {
        PrRecord {
            number: PrNumber(number),
            arrived_at,
            left,
            rebases: 0,
        }
    }

    fn run(consumed: u64, members: usize, landed: bool) -> RunRecord {
        RunRecord {
            batch: BatchId(1),
            members,
            started_at: 0,
            consumed,
            verdict: None,
            tree: Vec::new(),
            landed,
        }
    }

    fn pr_ci(consumed: u64, cancelled: bool) -> PrCiRecord {
        PrCiRecord {
            pr: PrNumber(1),
            head: Sha::parse(&format!("{:040x}", 2)).expect("hex"),
            started_at: 0,
            consumed,
            cancelled,
        }
    }

    fn ejected(justified: bool) -> Exit {
        Exit::Ejected {
            reason: EjectReason::Conflicts {
                with: dorfq_core::ConflictWith::Trunk,
            },
            justified,
        }
    }

    #[test]
    fn summarises_counts_costs_and_latencies() {
        let prs = [
            pr(1, 0, Some((3600, Exit::Landed))),
            pr(2, 0, Some((7200, Exit::Landed))),
            pr(3, 0, Some((1800, ejected(true)))),
            pr(4, 0, Some((1800, ejected(false)))),
            pr(5, 0, Some((60, Exit::Withdrawn))),
            pr(6, 0, None),
            pr(7, 0, Some((60, Exit::Blocked { on: PrNumber(5) }))),
        ];
        let runs = [run(3600, 2, true), run(3600, 4, false)];
        let mut prs = prs;
        prs[0].rebases = 2;
        prs[5].rebases = 1;
        let m = Metrics::summarise(&prs, &runs, &[], RunningSeconds::default(), 2, 7200);
        assert_eq!(m.rebases, 3);
        assert_eq!(
            (
                m.arrived,
                m.landed,
                m.ejected_bad,
                m.ejected_good,
                m.withdrawn,
                m.blocked,
                m.unfinished
            ),
            (7, 2, 1, 1, 1, 1, 1)
        );
        assert_eq!(m.runs, 2);
        assert_eq!(m.mean_batch_size, Some(3.0));
        assert_eq!(m.runner_hours, 2.0);
        assert_eq!(m.runner_hours_per_landed, Some(1.0));
        assert_eq!(m.wasted_fraction, Some(0.5));
        assert_eq!(m.land_p50_hours, Some(1.0));
        assert_eq!(m.land_p95_hours, Some(2.0));
        assert_eq!(m.reject_p50_hours, Some(0.5));
        assert_eq!(m.utilisation, Some(0.5));
    }

    #[test]
    fn empty_sets_summarise_to_none() {
        let m = Metrics::summarise(&[], &[], &[], RunningSeconds::default(), 2, 0);
        assert_eq!(m.arrived, 0);
        assert_eq!(m.runner_hours, 0.0);
        assert_eq!((m.pr_ci_runs, m.pr_ci_hours), (0, 0.0));
        assert_eq!(m.mean_batch_size, None);
        assert_eq!(m.runner_hours_per_landed, None);
        assert_eq!(m.wasted_fraction, None);
        assert_eq!(m.land_p50_hours, None);
        assert_eq!(m.reject_p95_hours, None);
        assert_eq!(m.utilisation, None);
    }

    #[test]
    fn runs_still_running_cost_runner_time_but_are_not_waste() {
        let runs = [run(3600, 2, true), run(0, 2, false)];
        let m = Metrics::summarise(
            &[],
            &runs,
            &[],
            RunningSeconds {
                batches: 3600,
                pr_ci: 0,
            },
            2,
            3600,
        );
        assert_eq!(m.runner_hours, 2.0);
        assert_eq!(m.wasted_fraction, Some(0.0));
        assert_eq!(m.utilisation, Some(1.0));
    }

    #[test]
    fn pr_ci_costs_runner_time_but_is_not_batch_waste() {
        let prs = [pr(1, 0, Some((7200, Exit::Landed)))];
        let runs = [run(1800, 1, true), run(1800, 1, false)];
        let pr_ci = [pr_ci(1800, false), pr_ci(900, true)];
        let m = Metrics::summarise(
            &prs,
            &runs,
            &pr_ci,
            RunningSeconds {
                batches: 0,
                pr_ci: 900,
            },
            2,
            7200,
        );
        assert_eq!((m.runs, m.pr_ci_runs), (2, 2));
        assert_eq!(m.pr_ci_hours, 1.0);
        assert_eq!(m.runner_hours, 2.0);
        assert_eq!(m.runner_hours_per_landed, Some(2.0));
        assert_eq!(m.wasted_fraction, Some(0.5));
        assert_eq!(m.utilisation, Some(0.5));
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let values: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(percentile(&values, 0.5), Some(10.0));
        assert_eq!(percentile(&values, 0.95), Some(19.0));
        assert_eq!(percentile(&[4.0], 0.95), Some(4.0));
    }
}
