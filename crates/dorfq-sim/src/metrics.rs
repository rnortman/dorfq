//! Per-pull-request and per-run records, and one replication's summary.

use std::collections::BTreeMap;

use dorfq_core::{BatchId, CiStatus, EjectReason, PrNumber, Sha};
use serde::Serialize;

const HOUR: f64 = crate::HOUR as f64;

/// How a pull request left the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// Landed; `bad` when it was bad against trunk at that instant — broken
    /// by an earlier landing, on a bad base, or beside a pull request of its
    /// batch it conflicts with — through a run that missed the break.
    Landed { bad: bool },
    /// Ejected; `bad_since` is when it became bad against trunk, when it
    /// was bad at the moment of ejection (the ejection was justified), and
    /// `None` when it was not.
    Ejected {
        reason: EjectReason,
        bad_since: Option<u64>,
    },
    /// Intent cleared.
    Withdrawn,
    /// Left because its base `on` left without landing; `bad_since` is when
    /// it became bad against trunk, its base's badness included, when it was
    /// bad at that instant, and `None` when it was not.
    Blocked {
        on: PrNumber,
        bad_since: Option<u64>,
    },
}

/// How a replication ended: drained, or cut off before the world emptied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct End {
    /// When it ended: the instant its world last emptied, or the cut-off.
    pub at: u64,
    /// The replication ended with the world not empty: at its deadline, or
    /// stalled.
    pub cut_off: bool,
    /// No landing or ejection for `stall_hours` while a pull request was
    /// present ended it. Implies `cut_off`.
    pub stalled: bool,
}

impl End {
    /// A replication whose world emptied at `at`.
    pub fn drained(at: u64) -> End {
        End {
            at,
            cut_off: false,
            stalled: false,
        }
    }
}

/// A span trunk was broken: from a bad landing at `at` to its repair at
/// `until`, `None` while unrepaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Break {
    pub at: u64,
    pub until: Option<u64>,
}

/// One pull request's life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRecord {
    pub number: PrNumber,
    pub arrived_at: u64,
    /// When and how it left; `None` while in the world.
    pub left: Option<(u64, Exit)>,
    /// Rebases pushed: after a conflict, after a landed base under `github`
    /// admission, or as the fix for a failed own-CI run.
    pub rebases: u32,
    /// It was bad against trunk at some point: broken itself, or stacked on
    /// a bad base.
    pub ever_bad: bool,
    /// The first notice the queue ever sent it; `None` while it has had
    /// none. Later notices, at this head or another, do not replace it.
    pub notice: Option<FirstNotice>,
}

/// The first notice a pull request received, judged against the world's
/// truth when it was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstNotice {
    pub t: u64,
    pub justified: bool,
    /// For a justified `Broken` notice, the seconds from the later of its
    /// arrival and `bad_since` to the notice: how long the author waited
    /// to hear. `None` for an `InteractsWith` notice or an unjustified one.
    pub waited: Option<u64>,
}

/// One batch run.
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
    /// True when its batch was retired while the run was running.
    pub cancelled: bool,
    /// Its tree was broken and the run missed it, fixed at its start: it
    /// passes unless it flakes.
    pub missed: bool,
}

/// One pull request's own CI run.
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
    /// The queue caused it by landing the pull request's direct base
    /// without it; its time counts in `runner_hours`.
    pub billed: bool,
    /// Its verdict, fixed at its start: the head was broken against trunk,
    /// or trunk itself was broken, and the run did not miss it.
    pub failed: bool,
}

/// One replication's summary; `None` where the set it summarises is empty.
/// The simulation page's glossary (`ROWS` in `site/app.js`) restates these
/// field comments and changes with them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Metrics {
    pub arrived: usize,
    pub landed: usize,
    /// Final ejections, direct or with an ejected base, of pull requests
    /// bad against trunk at that instant.
    pub ejected_bad: usize,
    /// Final ejections, direct or with an ejected base, of pull requests
    /// not bad at that instant.
    pub ejected_good: usize,
    /// Intent cleared, directly or with a withdrawn base.
    pub withdrawn: usize,
    pub unfinished: usize,
    /// Left because a base left without landing; counted under that base's
    /// exit as well.
    pub left_with_base: usize,
    /// Pull requests that were bad against trunk at some point.
    pub ever_bad: usize,
    /// Pull requests landed while bad against trunk — broken by an earlier
    /// landing, on a bad base, or beside a pull request they conflict
    /// with — through a run that missed the break.
    pub landed_bad: usize,
    /// Hours trunk spent broken: from a bad landing until its repair,
    /// overlapping breaks counted once, an unrepaired break clipped at the
    /// replication's end.
    pub trunk_broken_hours: f64,
    /// Rebases pushed, over every pull request.
    pub rebases: u64,
    /// Batch runs started, cancelled included.
    pub runs: usize,
    /// Batch runs stopped by their batch's retirement.
    pub cancelled_runs: usize,
    /// Members per batch run, a probe counting as a run of none.
    pub mean_batch_size: Option<f64>,
    /// Billed PR CI runs, cancelled included.
    pub billed_pr_ci_runs: usize,
    /// Runner time used by batch runs and billed PR CI runs: every finished
    /// or cancelled run's consumed time, plus the elapsed time of runs
    /// still running when the replication stopped. Unbilled PR CI is not
    /// in it.
    pub runner_hours: f64,
    /// The part of `runner_hours` billed PR CI runs used.
    pub billed_pr_ci_hours: f64,
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
    /// Hours from becoming bad, or from arriving when it arrived bad, to a
    /// justified ejection, direct or with an ejected base.
    pub detect_p50_hours: Option<f64>,
    pub detect_p95_hours: Option<f64>,
    /// Hours from becoming bad, or from arriving when it arrived bad, to
    /// the queue's first notice that it is probably broken, over pull
    /// requests whose first notice said so truly.
    pub notice_p50_hours: Option<f64>,
    pub notice_p95_hours: Option<f64>,
    /// Pull requests whose first notice was wrong: told it is probably
    /// broken while not bad, or that it fails together with a pull request
    /// it does not.
    pub notices_good: usize,
    /// When the replication ended: its world last emptied, or it was cut
    /// off.
    pub ended_hours: f64,
    /// The replication ended before the world emptied.
    pub cut_off: bool,
    /// The stall detector ended it.
    pub stalled: bool,
}

/// The nearest-rank percentile of `values`, which must be sorted.
fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let rank = ((p * values.len() as f64).ceil() as usize).max(1);
    Some(values[rank - 1])
}

/// The exit a departure counts under: `exit` itself, or for one that left
/// with its base, the first exit down its chain of bases that is not a
/// block.
fn root<'a>(exits: &BTreeMap<PrNumber, &'a Exit>, mut exit: &'a Exit) -> &'a Exit {
    while let Exit::Blocked { on, .. } = exit {
        exit = exits[on];
    }
    exit
}

/// Hours covered by the union of `breaks`, each clipped at `end`.
fn broken_hours(breaks: &[Break], end: u64) -> f64 {
    let mut spans: Vec<(u64, u64)> = breaks
        .iter()
        .map(|b| (b.at.min(end), b.until.unwrap_or(end).min(end)))
        .collect();
    spans.sort_unstable();
    let mut total = 0;
    let mut open: Option<(u64, u64)> = None;
    for (from, until) in spans {
        open = match open {
            Some((a, b)) if from <= b => Some((a, b.max(until))),
            Some((a, b)) => {
                total += b - a;
                Some((from, until))
            }
            None => Some((from, until)),
        };
    }
    if let Some((a, b)) = open {
        total += b - a;
    }
    total as f64 / HOUR
}

fn ratio(numerator: f64, denominator: f64) -> Option<f64> {
    (denominator > 0.0).then(|| numerator / denominator)
}

/// Runner-seconds used so far by the runs still running, by kind of run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunningSeconds {
    pub batches: u64,
    pub billed_pr_ci: u64,
}

impl Metrics {
    /// Summarises the records of a replication that ended as `end`, with
    /// the runs still running having used `running` and trunk broken over
    /// `breaks`.
    pub fn summarise(
        prs: &[PrRecord],
        runs: &[RunRecord],
        pr_ci: &[PrCiRecord],
        breaks: &[Break],
        running: RunningSeconds,
        end: End,
    ) -> Metrics {
        let mut landed = 0;
        let mut landed_bad = 0;
        let mut ejected_bad = 0;
        let mut ejected_good = 0;
        let mut withdrawn = 0;
        let mut unfinished = 0;
        let mut left_with_base = 0;
        let mut land_hours = Vec::new();
        let mut reject_hours = Vec::new();
        let mut detect_hours = Vec::new();
        let rebases = prs.iter().map(|p| u64::from(p.rebases)).sum();
        let exits: BTreeMap<PrNumber, &Exit> = prs
            .iter()
            .filter_map(|p| p.left.as_ref().map(|(_, exit)| (p.number, exit)))
            .collect();
        for pr in prs {
            let Some((left_at, exit)) = &pr.left else {
                unfinished += 1;
                continue;
            };
            let hours = (left_at - pr.arrived_at) as f64 / HOUR;
            let bad_since = match exit {
                Exit::Ejected { bad_since, .. } | Exit::Blocked { bad_since, .. } => *bad_since,
                Exit::Landed { .. } | Exit::Withdrawn => None,
            };
            if matches!(exit, Exit::Blocked { .. }) {
                left_with_base += 1;
            }
            match root(&exits, exit) {
                Exit::Landed { bad } => {
                    landed += 1;
                    landed_bad += usize::from(*bad);
                    land_hours.push(hours);
                }
                Exit::Ejected { .. } => match bad_since {
                    Some(since) => {
                        ejected_bad += 1;
                        reject_hours.push(hours);
                        let found_from = since.max(pr.arrived_at);
                        detect_hours.push((left_at - found_from) as f64 / HOUR);
                    }
                    None => ejected_good += 1,
                },
                Exit::Withdrawn => withdrawn += 1,
                Exit::Blocked { .. } => unreachable!("a root exit is not a block"),
            }
        }
        let mut notice_hours: Vec<f64> = prs
            .iter()
            .filter_map(|p| p.notice?.waited)
            .map(|s| s as f64 / HOUR)
            .collect();
        notice_hours.sort_by(f64::total_cmp);
        let notices_good = prs
            .iter()
            .filter(|p| p.notice.is_some_and(|n| !n.justified))
            .count();
        land_hours.sort_by(f64::total_cmp);
        reject_hours.sort_by(f64::total_cmp);
        detect_hours.sort_by(f64::total_cmp);
        let consumed: u64 = runs.iter().map(|r| r.consumed).sum::<u64>() + running.batches;
        let wasted: u64 = runs.iter().filter(|r| !r.landed).map(|r| r.consumed).sum();
        let members: usize = runs.iter().map(|r| r.members).sum();
        let billed = pr_ci.iter().filter(|r| r.billed);
        let billed_runs = billed.clone().count();
        let pr_ci_consumed = billed.map(|r| r.consumed).sum::<u64>() + running.billed_pr_ci;
        let runner_hours = (consumed + pr_ci_consumed) as f64 / HOUR;
        Metrics {
            arrived: prs.len(),
            landed,
            ejected_bad,
            ejected_good,
            withdrawn,
            unfinished,
            left_with_base,
            ever_bad: prs.iter().filter(|p| p.ever_bad).count(),
            landed_bad,
            trunk_broken_hours: broken_hours(breaks, end.at),
            rebases,
            runs: runs.len(),
            cancelled_runs: runs.iter().filter(|r| r.cancelled).count(),
            mean_batch_size: ratio(members as f64, runs.len() as f64),
            billed_pr_ci_runs: billed_runs,
            runner_hours,
            billed_pr_ci_hours: pr_ci_consumed as f64 / HOUR,
            runner_hours_per_landed: ratio(runner_hours, landed as f64),
            wasted_fraction: ratio(wasted as f64, consumed as f64),
            land_p50_hours: percentile(&land_hours, 0.5),
            land_p95_hours: percentile(&land_hours, 0.95),
            reject_p50_hours: percentile(&reject_hours, 0.5),
            reject_p95_hours: percentile(&reject_hours, 0.95),
            detect_p50_hours: percentile(&detect_hours, 0.5),
            detect_p95_hours: percentile(&detect_hours, 0.95),
            notice_p50_hours: percentile(&notice_hours, 0.5),
            notice_p95_hours: percentile(&notice_hours, 0.95),
            notices_good,
            ended_hours: end.at as f64 / HOUR,
            cut_off: end.cut_off,
            stalled: end.stalled,
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
            ever_bad: false,
            notice: None,
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
            cancelled: false,
            missed: false,
        }
    }

    fn pr_ci(consumed: u64, cancelled: bool, billed: bool) -> PrCiRecord {
        PrCiRecord {
            pr: PrNumber(1),
            head: Sha::parse(&format!("{:040x}", 2)).expect("hex"),
            started_at: 0,
            consumed,
            cancelled,
            billed,
            failed: false,
        }
    }

    fn ejected(bad_since: Option<u64>) -> Exit {
        Exit::Ejected {
            reason: EjectReason::Conflicts {
                with: dorfq_core::ConflictWith::Trunk,
            },
            bad_since,
        }
    }

    #[test]
    fn summarises_counts_costs_and_latencies() {
        let prs = [
            pr(1, 0, Some((3600, Exit::Landed { bad: false }))),
            pr(2, 0, Some((7200, Exit::Landed { bad: true }))),
            pr(3, 0, Some((1800, ejected(Some(0))))),
            pr(4, 0, Some((1800, ejected(None)))),
            pr(5, 0, Some((60, Exit::Withdrawn))),
            pr(6, 0, None),
            pr(
                7,
                0,
                Some((
                    60,
                    Exit::Blocked {
                        on: PrNumber(5),
                        bad_since: None,
                    },
                )),
            ),
        ];
        let mut runs = [run(3600, 2, true), run(3600, 4, false)];
        runs[1].cancelled = true;
        let mut prs = prs;
        prs[0].rebases = 2;
        prs[5].rebases = 1;
        let m = Metrics::summarise(
            &prs,
            &runs,
            &[],
            &[],
            RunningSeconds::default(),
            End::drained(0),
        );
        assert_eq!(m.rebases, 3);
        assert_eq!((m.landed_bad, m.trunk_broken_hours), (1, 0.0));
        assert_eq!(
            (
                m.arrived,
                m.landed,
                m.ejected_bad,
                m.ejected_good,
                m.withdrawn,
                m.unfinished,
                m.left_with_base
            ),
            (7, 2, 1, 1, 2, 1, 1)
        );
        assert_eq!((m.runs, m.cancelled_runs), (2, 1));
        assert_eq!(m.mean_batch_size, Some(3.0));
        assert_eq!(m.runner_hours, 2.0);
        assert_eq!(m.runner_hours_per_landed, Some(1.0));
        assert_eq!(m.wasted_fraction, Some(0.5));
        assert_eq!(m.land_p50_hours, Some(1.0));
        assert_eq!(m.land_p95_hours, Some(2.0));
        assert_eq!(m.reject_p50_hours, Some(0.5));
    }

    fn blocked(on: u64, bad_since: Option<u64>) -> Exit {
        Exit::Blocked {
            on: PrNumber(on),
            bad_since,
        }
    }

    #[test]
    fn a_pull_request_that_left_with_its_base_counts_under_the_root_exit() {
        let mut prs = [
            // An ejected bad base and a chain of two below it, one bad.
            pr(1, 0, Some((7200, ejected(Some(3600))))),
            pr(2, 0, Some((7200, blocked(1, Some(1800))))),
            pr(3, 0, Some((7200, blocked(2, None)))),
            // A withdrawn base and its bad child.
            pr(4, 0, Some((3600, Exit::Withdrawn))),
            pr(5, 0, Some((3600, blocked(4, Some(0))))),
            // An unjustified ejection taking a bad child down.
            pr(6, 0, Some((3600, ejected(None)))),
            pr(7, 0, Some((3600, blocked(6, Some(0))))),
            // A child arriving at 2h on a base bad since 0.
            pr(8, 0, Some((10800, ejected(Some(0))))),
            pr(9, 7200, Some((10800, blocked(8, Some(0))))),
        ];
        for p in &mut prs[..3] {
            p.ever_bad = true;
        }
        let m = Metrics::summarise(
            &prs,
            &[],
            &[],
            &[],
            RunningSeconds::default(),
            End::drained(7200),
        );
        assert_eq!(
            (
                m.ejected_bad,
                m.ejected_good,
                m.withdrawn,
                m.left_with_base,
                m.ever_bad
            ),
            (5, 2, 2, 5, 3)
        );
        // Arrival to ejection: 2, 2, 1, 3 and 1 hours.
        assert_eq!(m.reject_p50_hours, Some(2.0));
        // Bad to ejection: 1 hour for 1, 1.5 for 2, 1 for 7, 3 for 8, and
        // 1 for 9, counted from its arrival rather than its base's breakage.
        assert_eq!(
            (m.detect_p50_hours, m.detect_p95_hours),
            (Some(1.0), Some(3.0))
        );
    }

    fn told(justified: bool, waited: Option<u64>) -> Option<FirstNotice> {
        Some(FirstNotice {
            t: 0,
            justified,
            waited,
        })
    }

    #[test]
    fn notices_summarise_the_first_notice_of_each_pull_request() {
        let mut prs = [
            // Justified `Broken`: waited 1, 2 and 4 hours.
            pr(1, 0, Some((7200, ejected(Some(0))))),
            pr(2, 0, Some((7200, Exit::Landed { bad: false }))),
            pr(3, 0, None),
            // A justified `InteractsWith`: in neither measure.
            pr(4, 0, None),
            // Wrong, of either reason.
            pr(5, 0, Some((60, Exit::Landed { bad: false }))),
            pr(6, 0, None),
            // Never told.
            pr(7, 0, None),
        ];
        prs[0].notice = told(true, Some(3600));
        prs[1].notice = told(true, Some(7200));
        prs[2].notice = told(true, Some(14400));
        prs[3].notice = told(true, None);
        prs[4].notice = told(false, None);
        prs[5].notice = told(false, None);
        let m = Metrics::summarise(
            &prs,
            &[],
            &[],
            &[],
            RunningSeconds::default(),
            End::drained(0),
        );
        assert_eq!(
            (m.notice_p50_hours, m.notice_p95_hours, m.notices_good),
            (Some(2.0), Some(4.0), 2)
        );
    }

    #[test]
    fn empty_sets_summarise_to_none() {
        let m = Metrics::summarise(
            &[],
            &[],
            &[],
            &[],
            RunningSeconds::default(),
            End::drained(0),
        );
        assert_eq!(m.arrived, 0);
        assert_eq!(m.runner_hours, 0.0);
        assert_eq!((m.billed_pr_ci_runs, m.billed_pr_ci_hours), (0, 0.0));
        assert_eq!(m.mean_batch_size, None);
        assert_eq!(m.runner_hours_per_landed, None);
        assert_eq!(m.wasted_fraction, None);
        assert_eq!(m.land_p50_hours, None);
        assert_eq!(m.reject_p95_hours, None);
        assert_eq!(m.detect_p50_hours, None);
        assert_eq!((m.notice_p50_hours, m.notices_good), (None, 0));
    }

    #[test]
    fn runs_still_running_cost_runner_time_but_are_not_waste() {
        let runs = [run(3600, 2, true), run(0, 2, false)];
        let m = Metrics::summarise(
            &[],
            &runs,
            &[],
            &[],
            RunningSeconds {
                batches: 3600,
                billed_pr_ci: 0,
            },
            End::drained(0),
        );
        assert_eq!(m.runner_hours, 2.0);
        assert_eq!(m.wasted_fraction, Some(0.0));
    }

    #[test]
    fn billed_pr_ci_costs_runner_time_but_is_not_batch_waste() {
        let prs = [pr(1, 0, Some((7200, Exit::Landed { bad: false })))];
        let runs = [run(1800, 1, true), run(1800, 1, false)];
        let pr_ci = [
            pr_ci(1800, false, true),
            pr_ci(900, true, true),
            pr_ci(3600, false, false),
            pr_ci(600, true, false),
        ];
        let m = Metrics::summarise(
            &prs,
            &runs,
            &pr_ci,
            &[],
            RunningSeconds {
                batches: 0,
                billed_pr_ci: 900,
            },
            End::drained(0),
        );
        assert_eq!((m.runs, m.billed_pr_ci_runs), (2, 2));
        assert_eq!(m.billed_pr_ci_hours, 1.0);
        assert_eq!(m.runner_hours, 2.0);
        assert_eq!(m.runner_hours_per_landed, Some(2.0));
        assert_eq!(m.wasted_fraction, Some(0.5));
    }

    #[test]
    fn the_end_and_the_clock_are_carried() {
        let drained = Metrics::summarise(
            &[],
            &[],
            &[],
            &[],
            RunningSeconds::default(),
            End::drained(5400),
        );
        assert_eq!(
            (drained.ended_hours, drained.cut_off, drained.stalled),
            (1.5, false, false)
        );
        let end = End {
            at: 7200,
            cut_off: true,
            stalled: true,
        };
        let stalled = Metrics::summarise(&[], &[], &[], &[], RunningSeconds::default(), end);
        assert_eq!(
            (stalled.ended_hours, stalled.cut_off, stalled.stalled),
            (2.0, true, true)
        );
    }

    #[test]
    fn trunk_broken_hours_counts_overlapping_breaks_once_and_clips_an_open_one() {
        let at = |h: u64| h * 3600;
        let breaks = [
            Break {
                at: at(2),
                until: Some(at(4)),
            },
            Break {
                at: at(6),
                until: None,
            },
            Break {
                at: at(1),
                until: Some(at(3)),
            },
        ];
        let m = Metrics::summarise(
            &[],
            &[],
            &[],
            &breaks,
            RunningSeconds::default(),
            End::drained(at(7)),
        );
        assert_eq!(m.trunk_broken_hours, 4.0);
        // A repair scheduled past the end counts up to the end alone.
        let late = [Break {
            at: at(1),
            until: Some(at(9)),
        }];
        let m = Metrics::summarise(
            &[],
            &[],
            &[],
            &late,
            RunningSeconds::default(),
            End::drained(at(7)),
        );
        assert_eq!(m.trunk_broken_hours, 6.0);
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let values: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(percentile(&values, 0.5), Some(10.0));
        assert_eq!(percentile(&values, 0.95), Some(19.0));
        assert_eq!(percentile(&[4.0], 0.95), Some(4.0));
    }
}
