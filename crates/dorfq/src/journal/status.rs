//! The status document: the pool as last observed, when each pull request
//! and batch entered its state, and the last 24 hours of events, kept live
//! from the event stream and rendered to Markdown for the status issue.
//!
//! A figure over the window is over the samples and counts whose end falls
//! in the trailing 24 hours of forge time: a lifetime or run that began
//! before the window and ended in it is in, one that ended before is out,
//! one still open is in no figure. The pool is the present and is not
//! windowed.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;

use dorfq_core::{BatchState, CiStatus, Observation, PrBase};
use serde::Serialize;
use serde_json::Value;

use super::events::{Event, Kind};
use super::summary::{HOUR, percentiles};
use crate::driver;

/// The window, in seconds of forge time.
pub const WINDOW: u64 = 24 * 3600;

/// The longest body rendered, in characters: under GitHub's 65,536.
pub const BODY_CEILING: usize = 60_000;

/// The first line of every rendered body.
pub const MARKER: &str = "<!-- dorfq: status -->";

// TODO(status-document): publish this document as JSON to a branch for the
// simulation page's live mode.
/// The queue's state for a reader: the pool as last observed, when each
/// pull request and batch entered its state, and the last hours. A
/// function of the pool and the window alone, so it changes only when
/// they do.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Document {
    /// Oldest arrival first.
    pub prs: Vec<QueuedPr>,
    pub batches: Vec<Batch>,
    /// The window's events, oldest first: the kinds a reader is shown.
    pub recent: Vec<Event>,
    pub figures: Figures,
    /// Forge time of the first observation the structure has seen, seeded
    /// or live: where its record begins. `None` before the first.
    pub record_since: Option<u64>,
}

/// One queued pull request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueuedPr {
    pub number: u64,
    pub base: Option<u64>,
    pub head: String,
    /// The driver's wording: `ready`, `waiting:<holds>`, `conflicting:<with>`.
    pub status: String,
    /// Forge time it entered `status`.
    pub since: u64,
    /// Forge time of its `arrive`.
    pub arrived: u64,
    /// The tracked batches holding it.
    pub batches: Vec<u64>,
}

/// One tracked batch.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Batch {
    pub id: u64,
    /// The batch it was built on, or trunk.
    pub base: Option<u64>,
    pub members: Vec<u64>,
    /// `testing`, `passed`, `failed:<checks>`, `landed`, `refused:<reason>`,
    /// `conflict:<pr> with <what>`.
    pub state: String,
    /// Forge time of its `start`.
    pub started: u64,
}

/// Over the window: counts, and nearest-rank percentiles of the samples
/// whose end falls in it. Landings and ejections are the queue's own acts:
/// a pull request that left with its base is a `blocked` event in the
/// window and in neither figure, where the journal summary counts it under
/// its base's exit.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Figures {
    /// Members of the batches landed.
    pub landed_prs: usize,
    pub landed_batches: usize,
    /// Pull requests the queue ejected.
    pub ejected: usize,
    pub refused: usize,
    pub passed_runs: usize,
    pub failed_runs: usize,
    pub cancelled_runs: usize,
    pub batch_run_p50_hours: Option<f64>,
    pub batch_run_p95_hours: Option<f64>,
    pub land_p50_hours: Option<f64>,
    pub land_p95_hours: Option<f64>,
}

#[derive(Debug, Clone)]
struct PrTimes {
    arrived: u64,
    status: String,
    since: u64,
}

#[derive(Debug, Clone, Copy)]
struct BatchTimes {
    started: u64,
    base: Option<u64>,
}

/// What a count entry counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Counted {
    LandedPrs,
    LandedBatches,
    Ejected,
    Refused,
    PassedRuns,
    FailedRuns,
    CancelledRuns,
}

/// The live, bounded structure behind the [`Document`]: per-entity state
/// bounded by the pool and the tracked batches, everything else by the
/// window.
#[derive(Debug, Clone, Default)]
pub struct Status {
    /// The last observation.
    pool: Option<Observation>,
    prs: BTreeMap<u64, PrTimes>,
    batches: BTreeMap<u64, BatchTimes>,
    /// The window's shown events, oldest first.
    recent: VecDeque<Event>,
    /// `(t, hours)` per batch run with a verdict.
    runs: VecDeque<(u64, f64)>,
    /// `(t, hours)` per pull request landed, from its arrival.
    lands: VecDeque<(u64, f64)>,
    counts: VecDeque<(u64, Counted, usize)>,
    record_since: Option<u64>,
    /// The greatest event time seen.
    now: u64,
}

impl Status {
    /// Folds one tick: its events, then its observation when it had one.
    /// The [`super::Sink`] signature, so the seed drives it as the live
    /// loop does.
    pub fn tick(&mut self, observation: Option<&Observation>, events: &[Event]) {
        for event in events {
            self.apply(event);
        }
        if let Some(o) = observation {
            self.observe(o);
        }
        self.evict();
    }

    /// The greatest forge time seen: the latest observation's `now`.
    pub fn now(&self) -> u64 {
        self.now
    }

    fn count(&mut self, t: u64, what: Counted, n: usize) {
        self.counts.push_back((t, what, n));
    }

    fn apply(&mut self, event: &Event) {
        let t = event.t;
        self.now = self.now.max(t);
        let shown = match &event.kind {
            Kind::Arrive { pr, .. } => {
                self.prs.insert(
                    *pr,
                    PrTimes {
                        arrived: t,
                        status: String::new(),
                        since: t,
                    },
                );
                false
            }
            Kind::Start { batch, base, .. } => {
                let times = BatchTimes {
                    started: t,
                    base: *base,
                };
                self.batches.insert(*batch, times);
                false
            }
            Kind::RunEnd { batch, passed, .. } => {
                let what = if *passed {
                    Counted::PassedRuns
                } else {
                    Counted::FailedRuns
                };
                self.count(t, what, 1);
                if let Some(times) = self.batches.get(batch) {
                    let hours = t.saturating_sub(times.started) as f64 / HOUR;
                    self.runs.push_back((t, hours));
                }
                !passed
            }
            Kind::Land { members, .. } => {
                self.count(t, Counted::LandedBatches, 1);
                self.count(t, Counted::LandedPrs, members.len());
                for member in members {
                    if let Some(times) = self.prs.remove(member) {
                        let hours = t.saturating_sub(times.arrived) as f64 / HOUR;
                        self.lands.push_back((t, hours));
                    }
                }
                true
            }
            Kind::Eject { pr, .. } => {
                self.count(t, Counted::Ejected, 1);
                self.prs.remove(pr);
                true
            }
            Kind::Refused { .. } => {
                self.count(t, Counted::Refused, 1);
                true
            }
            Kind::Retire { batch, cancelled } => {
                self.batches.remove(batch);
                if *cancelled {
                    self.count(t, Counted::CancelledRuns, 1);
                }
                *cancelled
            }
            Kind::Merged { pr }
            | Kind::Withdraw { pr, .. }
            | Kind::Blocked { pr, .. }
            | Kind::Lost { pr } => {
                self.prs.remove(pr);
                true
            }
            Kind::Trunk { foreign, .. } => *foreign > 0,
            Kind::Notice { .. }
            | Kind::Conflict { .. }
            | Kind::Restart { .. }
            | Kind::Error { .. } => true,
            Kind::Admit { .. }
            | Kind::Repush { .. }
            | Kind::Held { .. }
            | Kind::PrCi { .. }
            | Kind::PrCiEnd { .. }
            | Kind::Conflicting { .. }
            | Kind::Step { .. } => false,
        };
        if shown {
            self.recent.push_back(event.clone());
        }
    }

    fn observe(&mut self, o: &Observation) {
        let now = o.now.0;
        self.now = self.now.max(now);
        self.record_since.get_or_insert(now);
        self.prs
            .retain(|n, _| o.prs.binary_search_by_key(n, |s| s.number.0).is_ok());
        for snapshot in &o.prs {
            let status = driver::status(&snapshot.status);
            // The fold's `arrive` precedes a pull request's first observation.
            let times = self.prs.entry(snapshot.number.0).or_insert(PrTimes {
                arrived: now,
                status: String::new(),
                since: now,
            });
            if times.status != status {
                times.status = status;
                times.since = now;
            }
        }
        self.pool = Some(o.clone());
    }

    fn evict(&mut self) {
        let Some(cutoff) = self.now.checked_sub(WINDOW) else {
            return;
        };
        while self.recent.front().is_some_and(|e| e.t < cutoff) {
            self.recent.pop_front();
        }
        while self.runs.front().is_some_and(|s| s.0 < cutoff) {
            self.runs.pop_front();
        }
        while self.lands.front().is_some_and(|s| s.0 < cutoff) {
            self.lands.pop_front();
        }
        while self.counts.front().is_some_and(|c| c.0 < cutoff) {
            self.counts.pop_front();
        }
    }

    /// The document of the structure as it stands.
    pub fn document(&self) -> Document {
        let mut prs = Vec::new();
        let mut batches = Vec::new();
        if let Some(pool) = &self.pool {
            for snapshot in &pool.prs {
                let number = snapshot.number.0;
                // `observe` gives every pooled pull request an entry.
                let times = &self.prs[&number];
                prs.push(QueuedPr {
                    number,
                    base: match snapshot.base {
                        PrBase::Trunk => None,
                        PrBase::Pr(n) => Some(n.0),
                    },
                    head: snapshot.head.to_string(),
                    status: times.status.clone(),
                    since: times.since,
                    arrived: times.arrived,
                    batches: pool
                        .batches
                        .iter()
                        .filter(|b| b.members.iter().any(|m| m.pr.0 == number))
                        .map(|b| b.id.0)
                        .collect(),
                });
            }
            prs.sort_by_key(|p| (p.arrived, p.number));
            for batch in &pool.batches {
                let times = self.batches.get(&batch.id.0);
                batches.push(Batch {
                    id: batch.id.0,
                    base: times.and_then(|t| t.base),
                    members: batch.members.iter().map(|m| m.pr.0).collect(),
                    state: state(&batch.state),
                    started: times.map_or(pool.now.0, |t| t.started),
                });
            }
        }
        let mut figures = Figures::default();
        for &(_, what, n) in &self.counts {
            *match what {
                Counted::LandedPrs => &mut figures.landed_prs,
                Counted::LandedBatches => &mut figures.landed_batches,
                Counted::Ejected => &mut figures.ejected,
                Counted::Refused => &mut figures.refused,
                Counted::PassedRuns => &mut figures.passed_runs,
                Counted::FailedRuns => &mut figures.failed_runs,
                Counted::CancelledRuns => &mut figures.cancelled_runs,
            } += n;
        }
        let hours = |samples: &VecDeque<(u64, f64)>| samples.iter().map(|s| s.1).collect();
        (figures.batch_run_p50_hours, figures.batch_run_p95_hours) = percentiles(hours(&self.runs));
        (figures.land_p50_hours, figures.land_p95_hours) = percentiles(hours(&self.lands));
        Document {
            prs,
            batches,
            recent: self.recent.iter().cloned().collect(),
            figures,
            record_since: self.record_since,
        }
    }
}

/// A batch's state as the document words it.
fn state(state: &BatchState) -> String {
    match state {
        BatchState::Testing {
            ci: CiStatus::Pending,
            ..
        } => "testing".to_owned(),
        BatchState::Testing {
            ci: CiStatus::Passed,
            ..
        } => "passed".to_owned(),
        BatchState::Testing {
            ci: CiStatus::Failed(failure),
            ..
        } => {
            let checks: Vec<&str> = failure.checks.iter().map(|c| c.0.as_str()).collect();
            format!("failed:{}", checks.join(","))
        }
        BatchState::Landed { .. } => "landed".to_owned(),
        BatchState::LandRefused { reason, .. } => format!("refused:{reason}"),
        BatchState::Conflict { pr, with } => format!("conflict:#{} with {with}", pr.0),
    }
}

/// The status structure and the last document handed over: the live loop's
/// side of the status issue.
#[derive(Debug, Default)]
pub struct Live {
    status: Status,
    last: Option<Document>,
    /// Whether a live tick has carried an observation: until one has, the
    /// structure holds only the seeded past, or nothing.
    observed: bool,
}

impl Live {
    /// An empty structure; nothing handed over yet.
    pub fn new() -> Live {
        Live::default()
    }

    /// Folds one tick replayed from the journal into the structure; see
    /// [`Status::tick`].
    pub fn seed(&mut self, observation: Option<&Observation>, events: &[Event]) {
        self.status.tick(observation, events);
    }

    /// Folds one live tick into the structure; see [`Status::tick`].
    pub fn tick(&mut self, observation: Option<&Observation>, events: &[Event]) {
        self.observed |= observation.is_some();
        self.status.tick(observation, events);
    }

    /// The rendered body when the document differs from the last one handed
    /// over, which it then records; `None` when it does not, and before the
    /// first live observation, when the structure says nothing about the
    /// repository as this process sees it.
    pub fn changed(&mut self, version: &str, started: u64) -> Option<String> {
        if !self.observed {
            return None;
        }
        let document = self.status.document();
        if self.last.as_ref() == Some(&document) {
            return None;
        }
        let body = render(&document, self.status.now(), version, started);
        self.last = Some(document);
        Some(body)
    }
}

/// The days-from-epoch `days` as a civil date: year, month, day.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

/// `t`, Unix seconds, as `YYYY-MM-DD HH:MM UTC`, the date dropped when it is
/// `now`'s.
fn time(t: u64, now: u64) -> String {
    let days = (t / 86_400) as i64;
    let of_day = t % 86_400;
    let clock = format!("{:02}:{:02} UTC", of_day / 3600, of_day % 3600 / 60);
    if t / 86_400 == now / 86_400 {
        return clock;
    }
    let (year, month, day) = civil(days);
    format!("{year:04}-{month:02}-{day:02} {clock}")
}

/// `seconds` as `3d 4h`, `2h 15m` or `15m`.
fn duration(seconds: u64) -> String {
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
    );
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// `text` safe inside a table cell: one line, no column breaks.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn prs(prs: &[u64]) -> String {
    let links: Vec<String> = prs.iter().map(|n| format!("#{n}")).collect();
    links.join(" ")
}

/// A conflict's other side: `trunk` or a pull request's link.
fn with(with: &Value) -> String {
    match with {
        Value::Number(n) => format!("#{n}"),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// One sentence for a shown event.
fn sentence(kind: &Kind) -> String {
    match kind {
        Kind::Land { batch, members } => format!("landed batch {batch}: {}", prs(members)),
        Kind::Eject { pr, reason, .. } => format!(
            "ejected #{pr}: {}",
            reason.as_deref().unwrap_or("reason unknown")
        ),
        Kind::RunEnd { batch, checks, .. } if checks.is_empty() => {
            format!("batch {batch} failed")
        }
        Kind::RunEnd { batch, checks, .. } => {
            format!("batch {batch} failed: {}", checks.join(", "))
        }
        Kind::Refused { batch, reason } => format!("batch {batch}'s landing refused: {reason}"),
        Kind::Notice { pr, reason } => format!("notice to #{pr}: {reason}"),
        Kind::Merged { pr } => format!("#{pr} merged outside the queue"),
        Kind::Withdraw { pr, how } => format!("#{pr} withdrawn: {how}"),
        Kind::Blocked { pr, on, .. } => format!("#{pr} left: its base #{on} left"),
        Kind::Lost { pr } => format!("#{pr} lost"),
        Kind::Conflict { batch, pr, with: w } => format!(
            "batch {batch} could not be built: #{pr} conflicts with {}",
            with(w)
        ),
        Kind::Retire { batch, .. } => format!("batch {batch} cancelled"),
        Kind::Trunk { foreign: 1, .. } => "trunk moved by another hand: 1 change".to_owned(),
        Kind::Trunk { foreign, .. } => format!("trunk moved by another hand: {foreign} changes"),
        Kind::Restart { dorfq, .. } => format!("restarted, dorfq {dorfq}"),
        Kind::Error {
            message,
            fatal: true,
            ..
        } => format!("fatal error: {message}"),
        Kind::Error { message, .. } => format!("error: {message}"),
        other => unreachable!("{other:?} is not a shown kind"),
    }
}

fn hours(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_owned(), |h| format!("{h:.1} h"))
}

/// `document` as the status issue's Markdown body, as of `now` (the
/// observation that changed it), for the process of `version` that started
/// at `started`; at most [`BODY_CEILING`] characters. Over it, the window
/// table's rows go first, oldest first, then the table itself; then the
/// queue table's rows from the newest arrival. The batches are never cut.
pub fn render(document: &Document, now: u64, version: &str, started: u64) -> String {
    let mut top = format!(
        "{MARKER}\ndorfq {version} · started {} · last change {} · {} queued · {} in flight\n\n## Queue\n\n",
        time(started, now),
        time(now, now),
        document.prs.len(),
        document.batches.len(),
    );
    let mut queue_rows = Vec::new();
    if document.prs.is_empty() {
        top.push_str("nothing queued\n");
    } else {
        top.push_str(
            "| pull request | status | in state | queued | batch |\n|---|---|---|---|---|\n",
        );
        for pr in &document.prs {
            let on = pr.base.map_or_else(String::new, |b| format!(" on #{b}"));
            let batch = if pr.batches.is_empty() {
                "—".to_owned()
            } else {
                let ids: Vec<String> = pr.batches.iter().map(u64::to_string).collect();
                format!("batch {}", ids.join(", "))
            };
            queue_rows.push(format!(
                "| #{}{on} | {} | since {} ({}) | queued {} (since {}) | {batch} |\n",
                pr.number,
                cell(&pr.status),
                time(pr.since, now),
                duration(now.saturating_sub(pr.since)),
                duration(now.saturating_sub(pr.arrived)),
                time(pr.arrived, now),
            ));
        }
    }

    let mut middle = String::from("\n## Batches in flight\n\n");
    if document.batches.is_empty() {
        middle.push_str("nothing in flight\n");
    } else {
        middle.push_str("| batch | base | members | state | running |\n|---|---|---|---|---|\n");
        for batch in &document.batches {
            let base = batch
                .base
                .map_or_else(|| "on trunk".to_owned(), |b| format!("on batch {b}"));
            let _ = writeln!(
                middle,
                "| {} | {base} | {} | {} | running {} (since {}) |",
                batch.id,
                prs(&batch.members),
                cell(&batch.state),
                duration(now.saturating_sub(batch.started)),
                time(batch.started, now),
            );
        }
    }
    middle.push_str("\n## Last 24 hours\n\n");
    if let Some(since) = document.record_since
        && since > now.saturating_sub(WINDOW)
    {
        let _ = writeln!(
            middle,
            "the record begins {}; earlier events are in the closed journal segments\n",
            time(since, now)
        );
    }
    let f = &document.figures;
    let _ = writeln!(
        middle,
        "landed {} pull requests in {} batches · ejected {} · refused {} · runs {} passed, {} failed, {} cancelled\n",
        f.landed_prs,
        f.landed_batches,
        f.ejected,
        f.refused,
        f.passed_runs,
        f.failed_runs,
        f.cancelled_runs,
    );
    let _ = writeln!(
        middle,
        "batch run p50 {}, p95 {} · time to land p50 {}, p95 {}",
        hours(f.batch_run_p50_hours),
        hours(f.batch_run_p95_hours),
        hours(f.land_p50_hours),
        hours(f.land_p95_hours),
    );
    let window_head = "\n| time | event |\n|---|---|\n";
    let window_rows: Vec<String> = document
        .recent
        .iter()
        .rev()
        .map(|e| format!("| {} | {} |\n", time(e.t, now), cell(&sentence(&e.kind))))
        .collect();

    let chars = |s: &str| s.chars().count();
    let fixed = chars(&top) + chars(&middle);
    let queue: usize = queue_rows.iter().map(|r| chars(r)).sum();
    let mut window: usize = window_rows.iter().map(|r| chars(r)).sum();
    let mut kept_window = window_rows.len();
    while kept_window > 0 && fixed + queue + chars(window_head) + window > BODY_CEILING {
        kept_window -= 1;
        window -= chars(&window_rows[kept_window]);
    }
    let window_len = if kept_window > 0 {
        chars(window_head) + window
    } else {
        0
    };
    let mut kept_queue = queue_rows.len();
    let mut queue = queue;
    let more = |n: usize| format!("\n… and {n} more\n");
    let cut = |kept: usize| {
        if kept < queue_rows.len() {
            chars(&more(queue_rows.len() - kept))
        } else {
            0
        }
    };
    while kept_queue > 0 && fixed + queue + cut(kept_queue) + window_len > BODY_CEILING {
        kept_queue -= 1;
        queue -= chars(&queue_rows[kept_queue]);
    }

    let mut body = top;
    for row in &queue_rows[..kept_queue] {
        body.push_str(row);
    }
    if kept_queue < queue_rows.len() {
        body.push_str(&more(queue_rows.len() - kept_queue));
    }
    body.push_str(&middle);
    if kept_window > 0 {
        body.push_str(window_head);
        for row in &window_rows[..kept_window] {
            body.push_str(row);
        }
    } else if window_rows.is_empty() {
        body.push_str("\nnothing in the window\n");
    }
    body
}

#[cfg(test)]
mod tests;
