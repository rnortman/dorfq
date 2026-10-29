//! DorfQ's forge for GitHub, over the REST API.
//!
//! The forge is an HTTP client and runs no git: GitHub's merge endpoint
//! builds a batch, a fast-forward-only ref update lands it, and every piece
//! of queue state lives in the repository itself, in the layout below.
//!
//! | Name | What it is |
//! |---|---|
//! | the configured label | intent: an open pull request against trunk, or against the head branch of another enqueued pull request, carrying it is enqueued |
//! | `refs/heads/dorfq/batch/<id>` | a tracked batch's tip; exists from the end of a successful build until `retire`; CI must run on pushes to it; with [`Settings::ci`] `Actions`, its workflow runs are cancelled when a testing batch is retired |
//! | `refs/heads/dorfq/build/<id>` | the branch a batch's merges are made on; exists only during `start_batch` |
//! | `refs/heads/dorfq/probe/<id>` | the branch conflict attribution uses; exists only during `start_batch` |
//! | `refs/dorfq/batches/<id>` | the batch's record, a commit whose message is the [`record`] text |
//! | the ruleset `dorfq/locked-heads` | refuses every update of each head branch in this repository of a pull request that is enqueued and a member of a tracked batch; exists while there is one and [`Settings::lock_heads`] is on; with it off the queue neither reads nor writes it |
//!
//! `<id>` is the decimal batch id. A batch with no members, a probe, is
//! published at an empty commit on its base, as is a batch whose every merge
//! had nothing to merge, so no tip is its own base. While the queue runs,
//! nobody pushes to, deletes or creates anything under `dorfq/` or
//! `refs/dorfq/` by hand: the forge reads a foreign or moved ref there as
//! [`ForgeError::Permanent`] and stops. Trunk moves only by fast-forward.
//!
//! # Stacks
//!
//! The enqueued set is built in rounds over the labelled open pull requests:
//! first those against trunk, then each one whose base branch is the head
//! branch of a pull request enqueued in an earlier round, based on the
//! lowest-numbered such pull request. A pull request whose head is in a fork
//! has no head branch here, so nothing stacks on it. Every pull request is
//! reported [`StackPolicy::Splittable`](dorfq_core::StackPolicy).
//!
//! A labelled pull request the rounds leave out is moved onto trunk when the
//! latest observation reported it stacked on an enqueued pull request whose
//! head branch it still targets, and that pull request has since merged into
//! trunk: the queue completed its landing since the latest observation,
//! whatever GitHub shows as its base; or it is open, targeting trunk, with its
//! head in trunk's history; or closed and merged into trunk with its recorded
//! head at the branch's tip.
//! The rounds are then run again. Any other labelled pull request left out is
//! not enqueued, and its author is told to retarget it: a base branch whose
//! pull request was merged into another branch, that moved on after its pull
//! request merged, or that was never a pull request's head, or one the queue
//! never reported this pull request stacked on, whatever its history, which
//! includes every labelled pull request after a restart, never makes the
//! queue change a pull request's base.
//!
//! A labelled member of a batch whose record reads landed is reported on
//! trunk, whatever its base, and its landing is completed when the batch is
//! retired. A stacked member of a batch being landed is moved onto trunk
//! after rule 2 is checked and before trunk is fast-forwarded, while GitHub
//! still sees commits between trunk and its head; GitHub then marks it merged
//! when trunk reaches its head, as it does a member against trunk. A move
//! GitHub refuses is a line on stderr and the member lands on its old base.
//! The `landing` record names each stacked member, the pull request it was
//! stacked on and that one's head branch before any member moves. When the
//! fast-forward is refused, or a move's response is lost, each moved member
//! is put back on its base branch at once, except one whose base landed
//! since the latest observation; when the fast-forward's own response is
//! lost, or a move back fails, or the process dies between the moves and the
//! fast-forward, the next observation puts them back from the record. A move
//! back GitHub refuses is [`ForgeError::Permanent`]: a member left on trunk
//! while its base is unlanded would land its base's commits alone.
//!
//! # Staleness
//!
//! [`Observation::trunk_position`] is 0 at the first observation this
//! process makes and counts every move of trunk after it: a batch the queue
//! landed by its members, a move by another hand by the commits it brought,
//! read by one `compare` from the trunk last counted. Every trunk counted is
//! kept with its position.
//!
//! GitHub's checks on a head do not say what trunk the run merged with, so a
//! pull request's [`PrSnapshot::behind`] is taken from its head's merge base
//! with trunk, read by one `compare` the first time the process sees the
//! head: where the merge base is a trunk this process counted, the run
//! covered that trunk's position exactly; otherwise it covered at most the
//! position less the commits trunk has that the head does not, which is
//! never fewer changes than the truth. From then on `behind` grows with the
//! position, by pull request for the queue's landings. A push to the head
//! reads it again; a check re-run on the same head changes nothing. Both
//! the position and what each head covered start over with the process,
//! and both are kept only once an observation succeeds.
//!
//! # Recovery
//!
//! [`GitHubForge`]'s first `observe` reads every batch back from its record
//! and its batch branch, and each later batch refresh (see **Refresh**)
//! reads that batch the same way, so a restarted process sees what the
//! previous one left:
//!
//! | record state | batch branch | reported as |
//! |---|---|---|
//! | `started` | any | nothing: a build that died; its branches and record are deleted |
//! | `testing <tip>` | at `<tip>` | `Testing { tip, ci }` |
//! | `conflict …` | absent | `Conflict { pr, with }` |
//! | `landing <tip>` | at `<tip>` | `Landed { tip }` if `<tip>` is in trunk's history, else `Testing { tip, ci }`, after each `moved:` member still open on trunk whose base pull request has not merged is put back on its base branch |
//! | `landed <tip>` | any | `Landed { tip }` |
//! | `refused <tip> …` | any | `LandRefused { tip, reason }` |
//!
//! Any other combination, a record that does not parse, or a ref under
//! `refs/dorfq/batches/` whose suffix is not a batch id is
//! [`ForgeError::Permanent`]: someone else is using the queue's namespace.
//!
//! # Refresh
//!
//! The forge keeps a cache of what it read and when, and `observe` reads
//! only the entities whose deadline in the [`refresh`] schedule has passed,
//! then derives a complete observation from the cache. A refresh of an
//! entity is these reads and nothing else:
//!
//! | entity | read | again after |
//! |---|---|---|
//! | the listing | the open issues carrying the label, the pull requests among them; the refs under `refs/dorfq/batches/` | `listing` |
//! | trunk | its branch's tip | `batch` |
//! | a batch | its record and branch, recovered as in **Recovery**; for a `testing` or `landing` record the tip's checks | `batch` while `Testing`; any other state is read again at the next listing |
//! | a pull request | its detail and, while it is open and labelled, its head's check runs and combined status; when its base is neither trunk nor a cached open pull request's head branch, the lowest-numbered open pull request with that head branch, and with none and the latest observation reporting it stacked on an unlanded one, the branch's tip and the heads of closed pull requests merged from it | `pr`; five seconds while GitHub reports its mergeability `unknown` |
//!
//! The intervals are [`Settings::refresh`]. Entities of one class read in
//! the same pass are spread over one to two intervals, and stay spread.
//! Everything is due at the first `observe`.
//!
//! **What is read where.** A fact keyed by a pull request, a branch or a
//! batch is read only at that entity's refresh; between refreshes it is
//! accepted stale, and every derivation across entities — the enqueued set,
//! stacks, holds, departures — runs over the cache. A fact keyed by commit
//! shas alone, one commit's ancestry of another, is read the first time it
//! is needed and kept while its later commit is trunk or a cached pull
//! request's head, the only commits it is asked of; it is the only read the
//! derivation makes, besides the commits trunk gained. An entity is made due
//! at once only when the queue itself changed it, or may have:
//!
//! - each effect makes due, before its first write, the listing and every
//!   batch and pull request it writes, and `land` trunk too, so a write
//!   whose response is lost is read back;
//! - a member put back on its base by a batch's recovery, and a pull
//!   request moved onto trunk when its base merged, have their cached base
//!   overwritten and are read in the same pass or the next;
//! - a cached pull request the listing lacks, and a listed one the cache
//!   lacks, are read in the same pass, and the pull request's own read
//!   decides whether it stays.
//!
//! A pull request found closed or unlabelled leaves the cache once the
//! observation reporting it is accepted; a batch whose record is gone
//! leaves it at its refresh. A blocked pull request's comment thread is read
//! once per blocker. A transient failure keeps every refresh that succeeded
//! before it; the failed entity stays due and no observation is made.
//!
//! A staleness this leaves is bounded by what the effects read: `land`
//! reads trunk and every member again and refuses on any change, a draft
//! and a merge state GitHub blocks included, and a member that conflicts
//! with trunk fails the batch's build before any CI runs.
//!
//! **Hints.** A [`webhook::Hint`] never changes the cache: it moves the
//! named entity's deadline earlier, to no sooner than five seconds after
//! its last read. A pull request the cache holds is hinted; one it does not
//! hold is due at once, and its read settles whether it is queued. A commit
//! hints every cached pull request whose head it is and every testing batch
//! whose tip it is, and is dropped when it is neither; a batch in any other
//! state is read only after the queue's own effects. `Trunk` hints trunk.
//! [`GitHubForge::wait`] blocks until the earlier of the next deadline and
//! the retry of a comment the last flush kept after a transient failure,
//! which is retried under its own backoff of five seconds doubling to
//! sixty, and resolves hints as they arrive; no hint shortens a rate-limit
//! pause or a backoff. `observe` posts the comments it owes before it
//! returns. When the receiver's channel disconnects, the next `observe` is
//! [`ForgeError::Permanent`].
//!
//! Each resolved hint writes one stderr line per entity it made due, with
//! the deadline it now holds (`due now`, or `due in <k> s` inside the
//! floor), or one line saying it matched nothing. When a wait is still held
//! after the hints that arrived, one line says how long the next tick is
//! away and why: a rate-limit pause, a backoff, retrying a comment post, or
//! nothing due sooner. A
//! pull request the cache did not hold whose read finds it not open and
//! labelled writes one line saying it is not queued.
//!
//! - [`api`]: the REST calls, behind the [`api::GitHubApi`] trait.
//! - [`app`]: the GitHub App credential: the private key, and the JWT it
//!   signs.
//! - [`checks`]: required check names on a commit, reduced to a CI status.
//! - [`messages`]: what authors are told, as marked issue comments.
//! - [`record`]: the batch record's text.
//! - [`refresh`]: the schedule of when each cached entity is next read.
//! - [`webhook`]: the webhook receiver, turning GitHub deliveries into
//!   refresh hints.

pub mod api;
pub mod app;
pub mod checks;
pub mod messages;
pub mod record;
pub mod refresh;
pub mod webhook;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::io::Write;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dorfq_core::ledger::Ledger;
use dorfq_core::{
    BatchId, BatchMember, BatchStart, BatchState, BatchStatus, CiStatus, ConflictWith, Departed,
    Departure, EjectReason, Forge, ForgeError, Hold, LandRefusal, NoticeReason, Observation,
    PrBase, PrNumber, PrSnapshot, PrStatus, Sha, Timestamp, Withdrawal,
};

use api::{ApiError, GitHubApi, Merge, MergeState, OpenPr, PrDetail, RefUpdate, RulesetId, RunId};
use messages::{Blocker, Event, Marker, Message};
use record::{Moved, Record, RecordState};
use refresh::{Entity, Refresh, Schedule};
use webhook::Hint;

/// What the forge is configured with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The branch the queue lands on, by short name (`main`).
    pub trunk: String,
    /// The intent label.
    pub label: String,
    /// The check names that must pass on pull request heads.
    pub checks: Vec<String>,
    /// The check names that must pass on batch tips.
    pub batch_checks: Vec<String>,
    /// Whether a batched member's head branch is locked by the queue's
    /// ruleset while it is batched; needs Administration: read/write.
    pub lock_heads: bool,
    /// The CI system whose runs the queue cancels when it retires a batch
    /// that is still testing. Absent, the queue cancels nothing: a retired
    /// batch's run ends on its own.
    pub ci: Option<Ci>,
    /// How often each cached entity is read unprompted.
    pub refresh: Refresh,
}

/// A CI system the queue can cancel a run on.
// TODO(github-retire-cancels-buildkite-build): a `Buildkite` variant, with
// its own settings and token, cancelling the builds on a retired batch's tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ci {
    /// GitHub Actions: workflow runs on the batch branch, cancelled through
    /// the repository's Actions API; needs Actions: read/write.
    Actions,
}

impl Settings {
    /// Rejects an empty `trunk` or `label`, no `checks` or `batch_checks`,
    /// an empty check name, or an interval [`Refresh::validate`] rejects.
    /// The message begins with the field's name (`trunk is empty`,
    /// `checks[1] is empty`, `refresh.pr_secs is 0ns; …`).
    pub fn validate(&self) -> Result<(), String> {
        if self.trunk.is_empty() {
            return Err("trunk is empty".to_owned());
        }
        if self.label.is_empty() {
            return Err("label is empty".to_owned());
        }
        // GitHub's label filter splits its argument at commas.
        if self.label.contains(',') {
            return Err(format!("label {:?} contains a comma", self.label));
        }
        for (field, names) in [
            ("checks", &self.checks),
            ("batch_checks", &self.batch_checks),
        ] {
            if names.is_empty() {
                return Err(format!("{field} names no check"));
            }
            if let Some(empty) = names.iter().position(String::is_empty) {
                return Err(format!("{field}[{empty}] is empty"));
            }
        }
        self.refresh
            .validate()
            .map_err(|problem| format!("refresh.{problem}"))
    }

    /// Whether a batch holding `enqueued` locks its head branch: `lock_heads`
    /// is on and the branch is in this repository.
    fn locks(&self, enqueued: &Enqueued) -> bool {
        self.lock_heads && enqueued.head_branch.is_some()
    }
}

/// A [`dorfq_core::Forge`] over one GitHub repository.
pub struct GitHubForge {
    api: Box<dyn GitHubApi>,
    settings: Settings,
    clock: Clock,
    ledger: Ledger,
    /// Pull requests whose landing this process completed since the latest
    /// observation (label removed, author told), with the batch they landed
    /// in.
    completed: BTreeMap<PrNumber, BatchId>,
    /// Pull requests whose label removal `eject` attempted and saw fail
    /// since the latest observation, with the reason: a removal applied
    /// with its response lost makes the departure `Ejected`.
    ejecting: BTreeMap<PrNumber, EjectReason>,
    /// What the effects need about the pull requests enqueued at the latest
    /// observation.
    enqueued: BTreeMap<PrNumber, Enqueued>,
    /// What the latest read of each pull request found: every one the
    /// latest observation found open and labelled, and every one read since.
    facts: BTreeMap<PrNumber, PrFacts>,
    /// Trunk's sha as last read; `None` before the first read.
    trunk_at: Option<Sha>,
    /// Each tracked batch as its record and branch were last read.
    batches: BTreeMap<BatchId, BatchStatus>,
    /// Whether one sha is in another's history, per pair read whose
    /// descendant is trunk or a cached pull request's head.
    ancestry: BTreeMap<(Sha, Sha), bool>,
    /// The blocker each pull request blocked at the latest observation was
    /// found told of, or told of since, so its thread is not read again
    /// while the blocker stands.
    told: BTreeMap<PrNumber, Blocker>,
    /// When each cached entity, and the listing, is next read.
    schedule: Schedule,
    /// The webhook receiver's hints; `None` without a receiver.
    hints: Option<Receiver<Hint>>,
    /// Set when the receiver's channel disconnected: its thread ended.
    listener_gone: bool,
    /// The hold `wait` applied after the latest transient failure, in
    /// seconds; `None` once a tick completed.
    backoff: Option<u64>,
    /// The hold `wait` applied before retrying a comment a transient failure
    /// kept in the outbox, in seconds; `None` once a wait found it empty.
    outbox_backoff: Option<u64>,
    /// The trunk position and what each enqueued head covered, as of the
    /// latest observation.
    counted: Counted,
    /// Comments not yet posted, in order.
    outbox: VecDeque<Message>,
    /// The members of each batch this process started since the latest
    /// observation; written only by `started`, beside the ledger.
    batched: BTreeMap<BatchId, Vec<PrNumber>>,
    /// The queue's branch-lock ruleset and the branches it locks, as last
    /// read from or written to GitHub; `None` when there is none.
    locked: Option<(RulesetId, BTreeSet<String>)>,
    /// One line per message posted.
    sink: Box<dyn Write>,
}

/// What the forge keeps about a pull request it reported enqueued.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Enqueued {
    /// For merge commit messages.
    title: String,
    /// Its head branch, which a pull request stacked on it targets; `None`
    /// for a head in a fork.
    head_branch: Option<String>,
    /// The repository's verdict on its head, if the repository holds it.
    verdict: Option<Verdict>,
}

impl Enqueued {
    /// Whether the repository's verdict holds it: it is held, and a batch
    /// holding it is not landed.
    fn vetoed(&self) -> bool {
        self.verdict.is_some()
    }
}

/// The repository's verdict on a head: it may not merge for a reason of
/// the repository's, not the queue's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Draft,
    Protection,
}

impl Verdict {
    fn hold(self) -> Hold {
        match self {
            Verdict::Draft => Hold::Draft,
            Verdict::Protection => Hold::Protection,
        }
    }
}

/// The trunk position this process has counted, and what it knows of the
/// trunk each enqueued head's own CI covered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Counted {
    /// The trunk position, counted to `counted`: pull requests that reached
    /// trunk since this process's first observation.
    position: u64,
    /// The trunk sha `position` counts to; `None` before the first read.
    counted: Option<Sha>,
    /// Every trunk this process has counted, with its position: where a
    /// pull request's merge base with trunk is one of them, the trunk its
    /// own CI covered is known exactly.
    history: BTreeMap<Sha, u64>,
    /// Each enqueued pull request's head and the position its own CI
    /// covered at that head, so the merge base is read once per head.
    covered: BTreeMap<PrNumber, (Sha, i64)>,
}

impl Counted {
    /// Counts trunk up to `trunk`: the first read is position 0; after it,
    /// each batch in `batches` landed on the trunk last counted adds its
    /// members, and whatever else moved trunk adds the commits it brought.
    fn advance(
        &mut self,
        api: &mut dyn GitHubApi,
        trunk: &Sha,
        batches: &[BatchStatus],
    ) -> Result<(), ApiError> {
        let Some(mut at) = self.counted.clone() else {
            self.position = 0;
            self.counted = Some(trunk.clone());
            self.history.insert(trunk.clone(), 0);
            return Ok(());
        };
        // Each batch is taken at most once, so the walk ends whatever the
        // batches' tips are.
        let mut taken = BTreeSet::new();
        while at != *trunk {
            let landed = batches
                .iter()
                .filter(|b| b.base == at && !taken.contains(&b.id))
                .filter_map(|b| match &b.state {
                    BatchState::Landed { tip } => Some((b.id, tip, b.members.len())),
                    _ => None,
                })
                .min_by_key(|&(id, ..)| id);
            match landed {
                Some((id, tip, members)) => {
                    taken.insert(id);
                    self.position += u64::try_from(members).expect("a batch's members fit u64");
                    at = tip.clone();
                }
                None => {
                    self.position += u64::from(api.compare(&at, trunk)?.ahead_by);
                    at = trunk.clone();
                }
            }
            self.history.insert(at.clone(), self.position);
        }
        self.counted = Some(at);
        Ok(())
    }

    /// `head`'s staleness: the position less the position its own CI
    /// covered. The covered position is read once per head: that of its
    /// merge base with `trunk` where that is a trunk this process counted,
    /// else the position less the commits trunk has that the head does not.
    fn behind(
        &mut self,
        api: &mut dyn GitHubApi,
        number: PrNumber,
        head: &Sha,
        trunk: &Sha,
    ) -> Result<u32, ApiError> {
        let position = i64::try_from(self.position).expect("a trunk position fits i64");
        let covered = match self.covered.get(&number) {
            Some((seen, covered)) if seen == head => *covered,
            _ => {
                let compared = api.compare(head, trunk)?;
                let covered = match self.history.get(&compared.merge_base) {
                    Some(&at) => i64::try_from(at).expect("a trunk position fits i64"),
                    None => position - i64::from(compared.ahead_by),
                };
                self.covered.insert(number, (head.clone(), covered));
                covered
            }
        };
        Ok(u32::try_from(position - covered).expect("a head's staleness fits u32"))
    }
}

/// What the latest read of a pull request found.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PrFacts {
    detail: PrDetail,
    /// Its head's CI over [`Settings::checks`]; read only when it is open
    /// and labelled.
    ci: Option<CiStatus>,
    base: BaseFacts,
}

/// What a read of a labelled pull request found at its base branch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BaseFacts {
    /// Nothing was looked up: the base is trunk or the head branch of an
    /// open pull request read in the same pass, or the pull request is a
    /// member of a landed batch, or the latest observation reported it
    /// stacked on one this process landed since.
    Unread,
    /// The lowest-numbered open pull request whose head branch it is.
    Held(OpenPr),
    /// No open pull request's head branch. `merged` when the latest
    /// observation reported the pull request stacked and the branch's tip
    /// is the recorded head of a pull request merged into trunk; read only
    /// in that case.
    Free { merged: bool },
}

/// What one read of the repository found.
struct World {
    observation: Observation,
    enqueued: BTreeMap<PrNumber, Enqueued>,
    /// The count advanced to this read, kept once the observation is.
    counted: Counted,
    /// Every labelled open pull request that is not enqueued, with the
    /// reason.
    blocked: BTreeMap<PrNumber, Blocker>,
}

/// What a labelled pull request the enqueued set left out is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Class {
    /// The latest observation reported it stacked on a pull request that
    /// has since merged into trunk: it belongs on trunk.
    MergedBase,
    Blocked(Blocker),
}

/// What the latest observation says a labelled pull request was queued on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stacked {
    /// Not reported stacked on the pull request whose head it targets.
    No,
    /// Reported stacked on a pull request whose landing this process
    /// completed since that observation.
    OnLanded,
    /// Reported stacked on a pull request this process has not landed since.
    OnUnlanded,
}

/// Where an observation's [`Observation::now`] comes from.
pub type Clock = Box<dyn Fn() -> Result<Timestamp, String>>;

/// Wall time in whole seconds since the Unix epoch.
pub fn system_clock() -> Result<Timestamp, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| Timestamp(d.as_secs()))
        .map_err(|e| format!("the system clock is before 1970: {e}"))
}

const RECORDS: &str = "refs/dorfq/batches/";

/// A deadline every clock reading has passed: due at once.
const AT_ONCE: Timestamp = Timestamp(0);

/// The hold after the first of a run of transient failures, in seconds.
const BACKOFF_FIRST: u64 = 5;
/// The longest hold after a transient failure, in seconds.
const BACKOFF_MAX: u64 = 60;

/// The hold after one more transient failure in a run whose last hold was
/// `last` seconds: [`BACKOFF_FIRST`] to start a run, doubling up to
/// [`BACKOFF_MAX`].
fn next_backoff(last: Option<u64>) -> u64 {
    last.map_or(BACKOFF_FIRST, |last| (last * 2).min(BACKOFF_MAX))
}

/// `time` in whole seconds since the Unix epoch, rounded up so a pause
/// ending at `time` has ended by the [`Timestamp`]; the epoch for a time
/// before it.
fn timestamp_at(time: SystemTime) -> Timestamp {
    time.duration_since(UNIX_EPOCH).map_or(AT_ONCE, |d| {
        Timestamp(d.as_secs() + u64::from(d.subsec_nanos() > 0))
    })
}

/// What resolving one hint at `now` made due, for its stderr lines.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolution {
    hint: Hint,
    now: Timestamp,
    made: Made,
}

/// The entities a hint made due, with the deadline each holds after it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Made {
    /// A pull-request hint: whether the cache held the pull request.
    Pr { cached: bool, deadline: Timestamp },
    /// A commit hint: every cached pull request and testing batch it named,
    /// none when it matched nothing.
    Commit(Vec<(Entity, Timestamp)>),
    /// A trunk hint.
    Trunk(Timestamp),
}

impl Resolution {
    /// `due now`, or `due in <k> s` with the floor named.
    fn due(&self, deadline: Timestamp) -> String {
        match deadline.0.checked_sub(self.now.0) {
            Some(wait) if wait > 0 => {
                format!(
                    "due in {wait} s (read less than {} s ago)",
                    refresh::HINT_FLOOR
                )
            }
            _ => "due now".to_owned(),
        }
    }
}

impl fmt::Display for Resolution {
    /// One line per entity made due, or one saying nothing was.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hint = &self.hint;
        match &self.made {
            Made::Pr { cached, deadline } => {
                let cached = if *cached { "cached" } else { "not cached" };
                write!(f, "dorfq: hint {hint}: {cached}; {}", self.due(*deadline))
            }
            Made::Commit(named) if named.is_empty() => write!(
                f,
                "dorfq: hint {hint}: matches no cached pull request head or testing batch tip; dropped"
            ),
            Made::Commit(named) => {
                for (index, &(entity, deadline)) in named.iter().enumerate() {
                    if index > 0 {
                        writeln!(f)?;
                    }
                    write!(f, "dorfq: hint {hint}: {entity} {}", self.due(deadline))?;
                }
                Ok(())
            }
            Made::Trunk(deadline) => write!(f, "dorfq: hint {hint}: {}", self.due(*deadline)),
        }
    }
}

/// What holds a wait past the hints that arrived during it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitHold {
    RateLimit,
    Backoff,
    /// The retry of a comment a transient failure kept is sooner than any
    /// deadline.
    Outbox,
    /// No deadline is sooner: none of the hints made anything due yet, or
    /// what they made due waits for the five-second floor.
    Floor,
}

/// What one wait holds for, fixed when it begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Holds {
    /// When the wait began.
    start: Timestamp,
    /// The end of the hold after a transient failure; `start` when none.
    held: Timestamp,
    /// When a comment the last flush kept is retried; `None` when none is.
    outbox_due: Option<Timestamp>,
}

impl Holds {
    /// The earlier of the schedule's `next_deadline` and the kept comment's
    /// retry.
    fn earliest(self, next_deadline: Timestamp) -> Timestamp {
        self.outbox_due
            .map_or(next_deadline, |due| due.min(next_deadline))
    }

    /// When the wait is over, given the schedule's `next_deadline`.
    fn wake(self, next_deadline: Timestamp) -> Timestamp {
        self.earliest(next_deadline).max(self.held)
    }
}

/// Why a wait holding for `holds` is not over at the earlier of the
/// schedule's `next_deadline` and a kept comment's retry, with a rate-limit
/// pause ending at `paused_until`.
fn hold(next_deadline: Timestamp, paused_until: Option<Timestamp>, holds: Holds) -> WaitHold {
    let earliest = holds.earliest(next_deadline);
    if holds.held > holds.start && holds.held >= earliest {
        WaitHold::Backoff
    } else if paused_until.is_some_and(|until| until >= earliest) {
        WaitHold::RateLimit
    } else if holds.outbox_due.is_some_and(|due| due < next_deadline) {
        WaitHold::Outbox
    } else {
        WaitHold::Floor
    }
}

/// The stderr line for a wait that holds `seconds` more for `hold`.
fn hold_line(seconds: u64, hold: WaitHold) -> String {
    let reason = match hold {
        WaitHold::RateLimit => "rate-limit pause",
        WaitHold::Backoff => "backing off after a transient failure",
        WaitHold::Outbox => "retrying a comment post",
        WaitHold::Floor => "nothing due sooner",
    };
    format!("dorfq: next tick in {seconds} s: {reason}")
}

/// The batch id of record ref `name`; `Permanent` for a ref under
/// [`RECORDS`] that is not one.
fn record_id(name: &str) -> Result<BatchId, ForgeError> {
    name.strip_prefix(RECORDS)
        .and_then(record::parse_decimal)
        .map(BatchId)
        .ok_or_else(|| {
            permanent(format!(
                "{name} is not a batch record: the queue owns {RECORDS}"
            ))
        })
}

/// The ancestry of sha pairs, read once per pair through `api` and kept in
/// `known`.
struct Ancestry<'a> {
    api: &'a mut dyn GitHubApi,
    known: &'a mut BTreeMap<(Sha, Sha), bool>,
}

impl Ancestry<'_> {
    /// Whether `ancestor` is in `descendant`'s history.
    fn is(&mut self, ancestor: &Sha, descendant: &Sha) -> Result<bool, ApiError> {
        let pair = (ancestor.clone(), descendant.clone());
        if let Some(&known) = self.known.get(&pair) {
            return Ok(known);
        }
        let is = self.api.is_ancestor(ancestor, descendant)?;
        self.known.insert(pair, is);
        Ok(is)
    }
}

/// The name of the repository ruleset that locks batched members' head
/// branches.
pub const LOCK_RULESET: &str = "dorfq/locked-heads";

/// The full ref name of branch `name`.
pub(crate) fn branch(name: &str) -> String {
    format!("refs/heads/{name}")
}

/// The short name of batch `id`'s published branch.
pub(crate) fn batch_branch(id: BatchId) -> String {
    format!("dorfq/batch/{}", id.0)
}

/// The short name of the branch batch `id`'s merges are made on.
fn build_branch(id: BatchId) -> String {
    format!("dorfq/build/{}", id.0)
}

/// The short name of the branch batch `id`'s conflict attribution uses.
fn probe_branch(id: BatchId) -> String {
    format!("dorfq/probe/{}", id.0)
}

fn record_ref(id: BatchId) -> String {
    format!("{RECORDS}{}", id.0)
}

fn permanent(message: String) -> ForgeError {
    ForgeError::Permanent(message.into())
}

/// The holds in `holds` its author is told about: the queue's own, which
/// GitHub does not show on the pull request. CI, a draft, branch protection
/// and mergeability are GitHub's state and are shown there.
fn told_holds(holds: &BTreeSet<Hold>) -> BTreeSet<Hold> {
    holds
        .iter()
        .filter(|hold| matches!(hold, Hold::AncestorMoved { .. }))
        .cloned()
        .collect()
}

impl GitHubForge {
    /// A forge over the repository `api` reaches, reading time from
    /// `clock`, and hints from `hints` when a webhook receiver runs. Checks
    /// that trunk and the label exist and, with `lock_heads` on, reads the
    /// queue's branch-lock ruleset; writes nothing.
    pub fn new(
        mut api: Box<dyn GitHubApi>,
        settings: Settings,
        clock: Clock,
        sink: Box<dyn Write>,
        hints: Option<Receiver<Hint>>,
    ) -> Result<GitHubForge, ApiError> {
        settings
            .validate()
            .map_err(|problem| ApiError::Permanent(format!("forge setting {problem}").into()))?;
        let trunk = branch(&settings.trunk);
        if api.get_ref(&trunk)?.is_none() {
            return Err(ApiError::Permanent(
                format!("trunk branch {trunk} does not exist").into(),
            ));
        }
        if !api.label_exists(&settings.label)? {
            return Err(ApiError::Permanent(
                format!(
                    "label {:?} does not exist in the repository",
                    settings.label
                )
                .into(),
            ));
        }
        let locked = if settings.lock_heads {
            api.find_ruleset(LOCK_RULESET)?
        } else {
            None
        };
        let schedule = Schedule::new(&settings.refresh);
        Ok(GitHubForge {
            api,
            settings,
            clock,
            ledger: Ledger::new(),
            completed: BTreeMap::new(),
            ejecting: BTreeMap::new(),
            enqueued: BTreeMap::new(),
            facts: BTreeMap::new(),
            trunk_at: None,
            batches: BTreeMap::new(),
            ancestry: BTreeMap::new(),
            told: BTreeMap::new(),
            schedule,
            hints,
            listener_gone: false,
            backoff: None,
            outbox_backoff: None,
            counted: Counted::default(),
            outbox: VecDeque::new(),
            batched: BTreeMap::new(),
            locked,
            sink,
        })
    }

    /// Blocks until the next observation has something to do: until the
    /// earliest deadline in the schedule, or a hint makes an entity due
    /// sooner, or a comment the last flush kept after a transient failure is
    /// due to be retried — five seconds after the wait begins, doubling at
    /// each wait in a row that finds one kept, up to sixty. After a
    /// transient failure (`transient`) it also holds for five seconds,
    /// doubling at each failure in a row up to sixty; a rate-limit pause the
    /// API client recorded holds it until the pause ends. No hint ends a
    /// hold early. Hints are resolved against a clock reading taken when
    /// they arrive, each writing its stderr lines, followed by a line naming
    /// the hold when the wait goes on past them. Returns at once when the
    /// clock cannot be read, or when the webhook receiver's channel
    /// disconnects; the next observation reports either.
    pub fn wait(&mut self, transient: bool) {
        if let Some(until) = self.api.rate_limit_pause() {
            self.schedule.pause(timestamp_at(until));
        }
        let Ok(start) = (self.clock)() else {
            return;
        };
        let held = if transient {
            let backoff = next_backoff(self.backoff);
            self.backoff = Some(backoff);
            Timestamp(start.0 + backoff)
        } else {
            self.backoff = None;
            start
        };
        self.outbox_backoff = (!self.outbox.is_empty()).then(|| next_backoff(self.outbox_backoff));
        let outbox_due = self.outbox_backoff.map(|backoff| {
            let retry = Timestamp(start.0 + backoff);
            self.schedule
                .paused_until()
                .map_or(retry, |until| retry.max(until))
        });
        let holds = Holds {
            start,
            held,
            outbox_due,
        };
        loop {
            let Ok(now) = (self.clock)() else {
                return;
            };
            let wake = holds.wake(self.next_deadline());
            if wake <= now {
                return;
            }
            let remaining = Duration::from_secs(wake.0 - now.0);
            let Some(hints) = &self.hints else {
                thread::sleep(remaining);
                continue;
            };
            match hints.recv_timeout(remaining) {
                Ok(hint) => {
                    let mut received = vec![hint];
                    received.extend(hints.try_iter());
                    let Ok(now) = (self.clock)() else {
                        return;
                    };
                    for hint in received {
                        eprintln!("{}", self.resolve(hint, now));
                    }
                    if let Some(line) = self.held_line(now, holds) {
                        eprintln!("{line}");
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.listener_gone = true;
                    return;
                }
            }
        }
    }

    /// The stderr line for a wait holding for `holds` that is not over at
    /// `now`: how long the next tick is away and why. `None` when the wait
    /// is over.
    fn held_line(&self, now: Timestamp, holds: Holds) -> Option<String> {
        let next_deadline = self.next_deadline();
        let wake = holds.wake(next_deadline);
        (wake > now).then(|| {
            let why = hold(next_deadline, self.schedule.paused_until(), holds);
            hold_line(wake.0 - now.0, why)
        })
    }

    fn next_deadline(&self) -> Timestamp {
        self.schedule
            .next_deadline()
            .expect("the listing is always tracked")
    }

    /// Makes due at `now` what `hint` names, as far as the cache knows it:
    /// a cached pull request, subject to the schedule's floor; an uncached
    /// one at once, so a read settles whether it is queued; the cached pull
    /// requests whose head and the testing batches whose tip is the commit;
    /// trunk. A commit the cache does not know changes nothing. A batch in
    /// any other state is due already or untracked: only the queue's own
    /// effects change it. Returns what was made due, with each deadline.
    fn resolve(&mut self, hint: Hint, now: Timestamp) -> Resolution {
        let made = match &hint {
            Hint::Pr(number) if self.facts.contains_key(number) => Made::Pr {
                cached: true,
                deadline: self.hint_cached(Entity::Pr(*number), now),
            },
            Hint::Pr(number) => Made::Pr {
                cached: false,
                deadline: self.schedule.due_now(Entity::Pr(*number), now),
            },
            Hint::Commit(sha) => {
                let prs = self
                    .facts
                    .iter()
                    .filter(|(_, f)| f.detail.head == *sha)
                    .map(|(&n, _)| Entity::Pr(n));
                let batches = self
                    .batches
                    .iter()
                    .filter(
                        |(_, b)| matches!(&b.state, BatchState::Testing { tip, .. } if tip == sha),
                    )
                    .map(|(&id, _)| Entity::Batch(id));
                let named: Vec<Entity> = prs.chain(batches).collect();
                Made::Commit(
                    named
                        .into_iter()
                        .map(|entity| (entity, self.hint_cached(entity, now)))
                        .collect(),
                )
            }
            Hint::Trunk => Made::Trunk(self.hint_cached(Entity::Trunk, now)),
        };
        Resolution { hint, now, made }
    }

    /// Hints `entity`, which the cache holds and the schedule therefore
    /// tracks between ticks; the deadline it holds after.
    fn hint_cached(&mut self, entity: Entity, now: Timestamp) -> Timestamp {
        self.schedule
            .hint(entity, now)
            .expect("every cached entity is tracked between ticks")
    }

    /// Posts the outbox in order. A transient failure keeps the comment and
    /// everything after it for the next flush, a permanent one drops the
    /// comment; either writes a line on stderr.
    fn flush(&mut self) -> Result<(), ForgeError> {
        while let Some(message) = self.outbox.front() {
            let body = message.body(&self.settings.trunk, &self.settings.label);
            match self.api.create_comment(message.pr, &body) {
                Ok(()) => {
                    writeln!(self.sink, "{message}")
                        .map_err(|e| ForgeError::Permanent(Box::new(e)))?;
                }
                Err(error @ ApiError::Transient(_)) => {
                    eprintln!("dorfq: kept the comment for {message}: {error}; retrying");
                    return Ok(());
                }
                Err(error) => eprintln!("dorfq: dropped the comment for {message}: {error}"),
            }
            self.outbox.pop_front();
        }
        Ok(())
    }

    /// Whether the queue's ruleset, as last read or written, locks branch
    /// `name`.
    fn locks_branch(&self, name: &str) -> bool {
        self.locked
            .as_ref()
            .is_some_and(|(_, branches)| branches.contains(name))
    }

    /// Makes `entity` due at once: the queue changed it, or may have.
    fn mark_due(&mut self, entity: Entity) {
        self.schedule.due_now(entity, AT_ONCE);
    }

    /// Makes due at once what an effect writes or may have written: the
    /// listing, `batch`, and each pull request in `prs`. Called before the
    /// effect's first write, so a write whose response is lost is read back.
    fn mark_written(&mut self, batch: Option<BatchId>, prs: &[PrNumber]) {
        self.mark_due(Entity::Listing);
        if let Some(id) = batch {
            self.mark_due(Entity::Batch(id));
        }
        for &pr in prs {
            self.mark_due(Entity::Pr(pr));
        }
    }

    /// Reads every entity due at `now` into the cache, in the schedule's
    /// order: the listing, trunk, batches, then pull requests, the last
    /// asked for after the rest so a pull request the listing or a batch's
    /// recovery made due is read in the same pass. Each refresh that
    /// succeeds keeps its facts and its new deadline whatever fails after
    /// it; the one that fails stays due.
    fn refresh_due(&mut self, now: Timestamp) -> Result<(), ForgeError> {
        let mut records = BTreeMap::new();
        if self.schedule.due(now).contains(&Entity::Listing) {
            records = self.refresh_listing(now)?;
        }
        if self.schedule.due(now).contains(&Entity::Trunk) {
            self.trunk_at = Some(self.trunk()?);
            self.schedule.refreshed(Entity::Trunk, now, 1, 0);
        }
        let due: Vec<BatchId> = self
            .schedule
            .due(now)
            .into_iter()
            .filter_map(|e| match e {
                Entity::Batch(id) => Some(id),
                Entity::Listing | Entity::Trunk | Entity::Pr(_) => None,
            })
            .collect();
        for (index, &id) in due.iter().enumerate() {
            self.refresh_batch(id, records.get(&id).cloned(), now, due.len(), index)?;
        }
        self.refresh_prs(now, &self.landed_members())
    }

    /// The members of the cached `Landed` batches: their content is in
    /// trunk, whatever their base on GitHub.
    fn landed_members(&self) -> BTreeSet<PrNumber> {
        self.batches
            .values()
            .filter(|b| matches!(b.state, BatchState::Landed { .. }))
            .flat_map(|b| b.members.iter().map(|m| m.pr))
            .collect()
    }

    /// Reads the labelled pull requests and the batch records; returns each
    /// record's sha by batch id. A listed pull request or record the cache
    /// does not hold is due at once, and so is a cached one the listing
    /// lacks: the listing pages by offset, so an item on a page boundary can
    /// be listed twice or not at all, and the entity's own read settles it.
    /// A record whose batch the schedule does not track, one read in a state
    /// with nothing more to come but the queue's own effects, is read again.
    fn refresh_listing(&mut self, now: Timestamp) -> Result<BTreeMap<BatchId, Sha>, ForgeError> {
        let listed: BTreeSet<PrNumber> = self
            .api
            .list_labelled(&self.settings.label)?
            .into_iter()
            .collect();
        let mut records = BTreeMap::new();
        for entry in self.api.list_refs(RECORDS)? {
            records.insert(record_id(&entry.name)?, entry.sha);
        }
        let prs: BTreeSet<PrNumber> = listed
            .symmetric_difference(&self.facts.keys().copied().collect())
            .copied()
            .collect();
        for number in prs {
            self.mark_due(Entity::Pr(number));
        }
        let batches: Vec<BatchId> = records
            .keys()
            .filter(|id| {
                !self.batches.contains_key(id) || !self.schedule.tracks(Entity::Batch(**id))
            })
            .chain(self.batches.keys().filter(|id| !records.contains_key(id)))
            .copied()
            .collect();
        for id in batches {
            self.mark_due(Entity::Batch(id));
        }
        self.schedule.refreshed(Entity::Listing, now, 1, 0);
        Ok(records)
    }

    /// Reads batch `id`'s record, from `listed` when the listing in this
    /// pass found it, and recovers the batch from it; the `index`th of
    /// `count` batches read in this pass. A batch whose record is gone, or
    /// whose build died, leaves the cache. One `Testing` is read again on
    /// the batch interval; any other state changes only by the queue's own
    /// effects, which make it due, and the listing reads it again.
    fn refresh_batch(
        &mut self,
        id: BatchId,
        listed: Option<Sha>,
        now: Timestamp,
        count: usize,
        index: usize,
    ) -> Result<(), ForgeError> {
        let record = match listed {
            Some(sha) => Some(sha),
            None => self.api.get_ref(&record_ref(id))?,
        };
        let status = match record {
            Some(sha) => self.recover(id, &sha)?,
            None => None,
        };
        match status {
            Some(status) => {
                if matches!(status.state, BatchState::Testing { .. }) {
                    self.schedule
                        .refreshed(Entity::Batch(id), now, count, index);
                } else {
                    self.schedule.forget(Entity::Batch(id));
                }
                self.batches.insert(id, status);
            }
            None => {
                self.batches.remove(&id);
                self.schedule.forget(Entity::Batch(id));
            }
        }
        Ok(())
    }

    /// Reads every pull request due at `now` into the cache: its detail, and
    /// its CI when it is open and labelled. One read open and labelled is
    /// due again on the pull-request interval, or shortly when GitHub had
    /// not computed its mergeability; any other stays due until an
    /// observation drops it, and one the cache did not hold writes a stderr
    /// line saying it is not queued. Then, for each cached open labelled one not in
    /// `landed` with nothing read at its base, whose base branch is neither
    /// trunk nor the head branch of another cached open one, what is at its
    /// base.
    fn refresh_prs(
        &mut self,
        now: Timestamp,
        landed: &BTreeSet<PrNumber>,
    ) -> Result<(), ForgeError> {
        let due: Vec<PrNumber> = self
            .schedule
            .due(now)
            .into_iter()
            .filter_map(|e| match e {
                Entity::Pr(number) => Some(number),
                Entity::Listing | Entity::Trunk | Entity::Batch(_) => None,
            })
            .collect();
        for (index, &number) in due.iter().enumerate() {
            let cached = self.facts.contains_key(&number);
            let detail = self.api.get_pr(number)?;
            let queued = labelled(&detail, &self.settings.label);
            if let Some(line) = not_queued_line(number, cached, queued, &self.settings.label) {
                eprintln!("{line}");
            }
            let ci = if queued {
                Some(self.head_ci(&detail.head)?)
            } else {
                None
            };
            if queued {
                let entity = Entity::Pr(number);
                if detail.merge_state == MergeState::Unknown {
                    self.schedule.soon(entity, now);
                } else {
                    self.schedule.refreshed(entity, now, due.len(), index);
                }
            }
            self.facts.insert(
                number,
                PrFacts {
                    detail,
                    ci,
                    base: BaseFacts::Unread,
                },
            );
        }
        let unread: Vec<PrNumber> = self
            .facts
            .iter()
            .filter(|(_, f)| f.base == BaseFacts::Unread)
            .map(|(&n, _)| n)
            .collect();
        for number in unread {
            let detail = &self.facts[&number].detail;
            if !labelled(detail, &self.settings.label)
                || detail.base == self.settings.trunk
                || landed.contains(&number)
                || holder_in(&self.facts, number, &detail.base).is_some()
            {
                continue;
            }
            let base = detail.base.clone();
            let stacked = self.latest_snapshot(number).map_or(Stacked::No, |p| {
                reported_stacked(p.base, &self.enqueued, &self.completed, &base)
            });
            let found = if stacked == Stacked::OnLanded {
                BaseFacts::Unread
            } else if let Some(holder) = self.api.open_pr_with_head(&base)? {
                BaseFacts::Held(holder)
            } else if stacked == Stacked::OnUnlanded {
                BaseFacts::Free {
                    merged: self.merged_into_trunk(&base)?,
                }
            } else {
                BaseFacts::Free { merged: false }
            };
            self.facts
                .get_mut(&number)
                .expect("the pull request is cached")
                .base = found;
        }
        Ok(())
    }

    /// Whether `base`'s tip is the recorded head of a closed pull request
    /// merged into trunk; `false` when the branch is gone.
    fn merged_into_trunk(&mut self, base: &str) -> Result<bool, ForgeError> {
        Ok(match self.api.get_ref(&branch(base))? {
            Some(tip) => self
                .api
                .merged_pr_heads(base, &self.settings.trunk)?
                .contains(&tip),
            None => false,
        })
    }

    /// The observation at `now` over the cache, what the effects need about
    /// the pull requests it reports, and why each labelled open pull request
    /// it does not report is not enqueued. Reads nothing keyed by a pull
    /// request, branch or record; its only reads are the ancestry of sha
    /// pairs not read before and the commits trunk gained.
    fn derive(&mut self, now: Timestamp) -> Result<World, ForgeError> {
        let trunk = self
            .trunk_at
            .clone()
            .expect("trunk is read at the first refresh");
        let batches: Vec<BatchStatus> = self.batches.values().cloned().collect();
        let landed = self.landed_members();
        // Advanced on a copy, so a read that fails later leaves the count as
        // the latest observation left it.
        let mut counted = self.counted.clone();
        counted.advance(self.api.as_mut(), &trunk, &batches)?;

        // A merged base retargeted onto trunk can carry pull requests of its
        // own, so the enqueued set is computed again after every retarget. A
        // retarget changes only the retargeted pull request's base, which
        // can bring a blocked pull request into the enqueued set but never
        // changes the class of one it leaves out: each is classified once.
        let mut blocked = BTreeMap::new();
        let bases = loop {
            let bases = enqueued_bases(&self.facts, &self.settings, &landed);
            blocked.retain(|n, _| !bases.contains_key(n));
            let left: Vec<PrNumber> = self
                .facts
                .iter()
                .filter(|(_, f)| labelled(&f.detail, &self.settings.label))
                .map(|(&n, _)| n)
                .filter(|n| !bases.contains_key(n) && !blocked.contains_key(n))
                .collect();
            let mut moved = false;
            for number in left {
                let facts = &self.facts[&number];
                let base = facts.detail.base.clone();
                let stacked = self.latest_snapshot(number).map_or(Stacked::No, |p| {
                    reported_stacked(p.base, &self.enqueued, &self.completed, &base)
                });
                let holder = holder_in(&self.facts, number, &base).or(match &facts.base {
                    BaseFacts::Held(holder) => Some(holder.clone()),
                    BaseFacts::Unread | BaseFacts::Free { .. } => None,
                });
                let merged = matches!(facts.base, BaseFacts::Free { merged: true });
                let class = classify(
                    &mut Ancestry {
                        api: self.api.as_mut(),
                        known: &mut self.ancestry,
                    },
                    &self.settings.trunk,
                    &base,
                    stacked,
                    holder.as_ref(),
                    merged,
                    &trunk,
                )?;
                match class {
                    Class::MergedBase => match self.api.set_base(number, &self.settings.trunk) {
                        Ok(()) => {
                            // GitHub recomputes mergeability after a change
                            // of base.
                            let detail = &mut self
                                .facts
                                .get_mut(&number)
                                .expect("a pull request left out is cached")
                                .detail;
                            detail.base = self.settings.trunk.clone();
                            detail.merge_state = MergeState::Unknown;
                            self.schedule.due_now(Entity::Pr(number), AT_ONCE);
                            moved = true;
                        }
                        Err(ApiError::Permanent(error)) => {
                            eprintln!(
                                "dorfq: could not retarget #{} onto {}: {error}",
                                number.0, self.settings.trunk
                            );
                            blocked.insert(number, Blocker::Branch(base));
                        }
                        Err(error) => return Err(error.into()),
                    },
                    Class::Blocked(blocker) => {
                        blocked.insert(number, blocker);
                    }
                }
            }
            if !moved {
                break bases;
            }
        };

        let in_batch: BTreeSet<PrNumber> = batches
            .iter()
            .flat_map(|b| b.members.iter().map(|m| m.pr))
            .collect();
        let mut prs = Vec::new();
        let mut enqueued = BTreeMap::new();
        for (&number, &base) in &bases {
            let facts = &self.facts[&number];
            let pr = &facts.detail;
            // A read of the pull request derives the draft verdict from its
            // `draft` flag; protection the latest read at this head derived
            // or carried is carried through a read that cannot derive it.
            let carried_protection = self
                .latest_snapshot(number)
                .filter(|p| p.head == pr.head)
                .and_then(|_| self.enqueued.get(&number)?.verdict)
                .filter(|v| *v == Verdict::Protection);
            // A batched pull request stacked on a batched base is blocked by
            // the queue's own lock on that base, and the read cannot tell
            // that block from the repository's. It was ready when it was
            // batched, so the block does not hold it. Outside a batch the
            // block is derived, whatever its cause.
            let lock_blocks = in_batch.contains(&number)
                && matches!(base, PrBase::Pr(_))
                && self.locks_branch(&pr.base);
            let (mut verdict, conflicting, unknown, blocked) = match (pr.draft, pr.merge_state) {
                (true, state) => (
                    Some(Verdict::Draft),
                    state == MergeState::Dirty,
                    state == MergeState::Unknown,
                    false,
                ),
                (false, MergeState::Dirty) => (None, true, false, false),
                (false, MergeState::Unknown) => (carried_protection, false, true, false),
                (false, MergeState::Blocked) => (None, false, false, true),
                (
                    false,
                    MergeState::Clean
                    | MergeState::Unstable
                    | MergeState::HasHooks
                    | MergeState::Behind
                    | MergeState::Draft,
                ) => (None, false, false, false),
            };
            let status = if conflicting {
                PrStatus::Conflicting(match base {
                    PrBase::Trunk => ConflictWith::Trunk,
                    PrBase::Pr(a) => ConflictWith::Pr(a),
                })
            } else {
                let mut holds = BTreeSet::new();
                if unknown {
                    holds.insert(Hold::MergeabilityUnknown);
                }
                let mut ancestry = Ancestry {
                    api: self.api.as_mut(),
                    known: &mut self.ancestry,
                };
                if let PrBase::Pr(a) = base
                    && !ancestry.is(&self.facts[&a].detail.head, &pr.head)?
                {
                    holds.insert(Hold::AncestorMoved { pr: a });
                }
                let ci = facts
                    .ci
                    .clone()
                    .expect("an open labelled pull request's CI is read with it");
                match ci {
                    CiStatus::Passed => {}
                    CiStatus::Pending => {
                        holds.insert(Hold::OwnCiPending);
                    }
                    CiStatus::Failed(_) => {
                        holds.insert(Hold::OwnCiFailed);
                    }
                }
                // `blocked` beside a pending or failed required check is
                // explained by the check, and for a batched member on a
                // locked base by the lock; neither can say whether the rule
                // behind an earlier verdict is now met.
                if blocked {
                    verdict = match ci {
                        CiStatus::Passed if !lock_blocks => Some(Verdict::Protection),
                        CiStatus::Passed | CiStatus::Pending | CiStatus::Failed(_) => {
                            carried_protection
                        }
                    };
                }
                if let Some(verdict) = verdict {
                    holds.insert(verdict.hold());
                }
                if holds.is_empty() {
                    PrStatus::Ready
                } else {
                    PrStatus::Waiting(holds)
                }
            };
            enqueued.insert(
                number,
                Enqueued {
                    title: pr.title.clone(),
                    head_branch: pr.head_branch.clone(),
                    verdict,
                },
            );
            let behind = counted.behind(self.api.as_mut(), number, &pr.head, &trunk)?;
            prs.push(PrSnapshot::new(
                number,
                pr.head.clone(),
                base,
                status,
                behind,
            ));
        }

        counted.covered.retain(|n, _| bases.contains_key(n));
        prs.sort_by_key(|pr| pr.number);
        let departed = self.ledger.departures(&prs, &batches, |gone| {
            if self.ejecting.contains_key(&gone.number) {
                return Ok::<_, ForgeError>(Departed::Ejected);
            }
            if let Some(&batch) = self.completed.get(&gone.number) {
                return Ok(Departed::Landed { batch });
            }
            // Every pull request an observation reports stays cached until
            // one reports it gone.
            let pr = &self
                .facts
                .get(&gone.number)
                .expect("a pull request the latest observation reported is cached")
                .detail;
            let trunk_name = &self.settings.trunk;
            Ok(if labelled(pr, &self.settings.label) {
                match (blocked.get(&gone.number), gone.base) {
                    (Some(Blocker::Pr(a)), PrBase::Pr(b)) if *a == b => {
                        Departed::Withdrawn(Withdrawal::Blocked { on: b })
                    }
                    (Some(_), _) => Departed::Withdrawn(Withdrawal::Retargeted),
                    (None, _) => Departed::Withdrawn(Withdrawal::IntentCleared),
                }
            } else if pr.merged && pr.base == *trunk_name {
                Departed::Merged
            } else if pr.merged {
                // Merged into another branch leaves its content out of trunk.
                Departed::Withdrawn(Withdrawal::Retargeted)
            } else if !pr.open {
                Departed::Withdrawn(Withdrawal::Closed)
            } else {
                Departed::Withdrawn(Withdrawal::IntentCleared)
            })
        })?;
        Ok(World {
            observation: Observation::new(now, trunk, counted.position, prs, batches, departed),
            enqueued,
            counted,
            blocked,
        })
    }

    /// The batch whose record ref is at `record_sha`, per the recovery table,
    /// against the cached trunk; `None` for a build that died, which this
    /// call cleans up.
    fn recover(
        &mut self,
        id: BatchId,
        record_sha: &Sha,
    ) -> Result<Option<BatchStatus>, ForgeError> {
        let name = record_ref(id);
        let trunk = self
            .trunk_at
            .clone()
            .expect("trunk is read before any batch");
        let commit = self.api.get_commit(record_sha)?;
        let record = Record::parse(&name, id, &commit.message)
            .map_err(|e| ForgeError::Permanent(Box::new(e)))?;
        let published_ref = branch(&batch_branch(id));
        let published = self.api.get_ref(&published_ref)?;

        let state = match record.state {
            RecordState::Started => {
                self.api.delete_ref(&branch(&build_branch(id)))?;
                self.api.delete_ref(&branch(&probe_branch(id)))?;
                if published.is_some() {
                    self.api.delete_ref(&published_ref)?;
                }
                self.api.delete_ref(&name)?;
                return Ok(None);
            }
            RecordState::Testing { tip } => {
                expect_published(&published_ref, published.as_ref(), &tip)?;
                let ci = self.batch_ci(&tip)?;
                BatchState::Testing { tip, ci }
            }
            RecordState::Conflict { pr, with } => {
                if let Some(sha) = published {
                    return Err(permanent(format!(
                        "{published_ref} is at {sha}, but batch {} conflicted while \
                         building and was never published",
                        id.0
                    )));
                }
                BatchState::Conflict { pr, with }
            }
            RecordState::Landing { tip } => {
                expect_published(&published_ref, published.as_ref(), &tip)?;
                if self.api.is_ancestor(&tip, &trunk)? {
                    BatchState::Landed { tip }
                } else {
                    for m in &record.moved {
                        self.put_back(m)?;
                    }
                    let ci = self.batch_ci(&tip)?;
                    BatchState::Testing { tip, ci }
                }
            }
            RecordState::Landed { tip } => BatchState::Landed { tip },
            RecordState::Refused { tip, reason } => BatchState::LandRefused { tip, reason },
        };
        Ok(Some(BatchStatus::new(
            id,
            record.base,
            record.members,
            state,
        )))
    }

    /// A pull request head's CI, over `checks`.
    fn head_ci(&mut self, sha: &Sha) -> Result<CiStatus, ForgeError> {
        let results = self.api.checks(sha)?;
        Ok(checks::evaluate(&self.settings.checks, &results))
    }

    /// A batch tip's CI, over `batch_checks`.
    fn batch_ci(&mut self, sha: &Sha) -> Result<CiStatus, ForgeError> {
        let results = self.api.checks(sha)?;
        Ok(checks::evaluate(&self.settings.batch_checks, &results))
    }

    /// The rule-9 messages for `observation`. A departure is told it landed
    /// when it landed with a batch and this process has not completed that
    /// landing, and the reason when it was ejected with the ejection's own
    /// response lost; any other departure is told nothing.
    /// Every member still enqueued of a batch reported `Testing` that the
    /// latest observation did not report and this process did not start
    /// since is told it is being tested: its start applied with the
    /// response lost.
    /// After a restart, with no latest observation, nothing is told.
    /// A departure in `blocked` is told nothing here: every pull request in
    /// `blocked` is told why it is not enqueued unless the latest observation
    /// found it blocked and told of the same blocker, or its comment thread,
    /// read only otherwise, already ends with that. A pull request in
    /// `entered` that the latest observation did not report is greeted; one
    /// it did report, a landed or ejected member still labelled, and every
    /// one at the first observation of a process, only if its comment
    /// thread does not show it was greeted already. One held by
    /// [`told_holds`] that was not at the latest observation, or was at
    /// another head, is told them, unless its thread was read and the last
    /// transition it records is that.
    fn transitions(
        &mut self,
        observation: &Observation,
        entered: &BTreeSet<PrNumber>,
        blocked: &BTreeMap<PrNumber, Blocker>,
        enqueued: &BTreeMap<PrNumber, Enqueued>,
    ) -> Result<Vec<Message>, ForgeError> {
        let mut messages = Vec::new();
        for Departure { pr, how, .. } in &observation.departed {
            if blocked.contains_key(pr) {
                continue;
            }
            let event = match how {
                Departed::Landed { batch } if !self.completed.contains_key(pr) => {
                    let tip = observation
                        .batches
                        .iter()
                        .find(|b| b.id == *batch)
                        .and_then(|b| match &b.state {
                            BatchState::Landed { tip } => Some(tip.clone()),
                            BatchState::Conflict { .. }
                            | BatchState::Testing { .. }
                            | BatchState::LandRefused { .. } => None,
                        })
                        .expect("a landed departure names a batch the observation reports landed");
                    Some(Event::Landed { batch: *batch, tip })
                }
                Departed::Ejected => self.ejecting.get(pr).map(|reason| Event::Ejected {
                    reason: reason.clone(),
                    base: self
                        .latest_snapshot(*pr)
                        .expect("a departure was reported by the latest observation")
                        .base,
                }),
                Departed::Landed { .. } | Departed::Merged | Departed::Withdrawn(_) => None,
            };
            if let Some(event) = event {
                messages.push(Message { pr: *pr, event });
            }
        }
        if let Some(latest) = self.ledger.latest() {
            for batch in &observation.batches {
                let BatchState::Testing { tip, .. } = &batch.state else {
                    continue;
                };
                if latest.batches.iter().any(|b| b.id == batch.id)
                    || self.batched.contains_key(&batch.id)
                {
                    continue;
                }
                let members = batch.members.iter().filter(|m| {
                    observation
                        .prs
                        .binary_search_by_key(&m.pr, |p| p.number)
                        .is_ok()
                });
                let members: Vec<PrNumber> = members.map(|m| m.pr).collect();
                for pr in members {
                    messages.push(Message {
                        pr,
                        event: Event::Testing {
                            batch: batch.id,
                            tip: tip.clone(),
                            locked: enqueued.get(&pr).is_some_and(|e| self.settings.locks(e)),
                        },
                    });
                }
            }
        }
        for (&pr, blocker) in blocked {
            if self.told.get(&pr) == Some(blocker) {
                continue;
            }
            let thread = self.api.list_comments(pr)?;
            if !messages::told_blocked(&thread, blocker) {
                messages.push(Message {
                    pr,
                    event: Event::Blocked {
                        on: blocker.clone(),
                    },
                });
            }
        }
        let first = self.ledger.latest().is_none();
        for snapshot in &observation.prs {
            let pr = snapshot.number;
            let told_held = if !entered.contains(&pr) {
                self.held_at_latest(snapshot)
            } else if !first && self.latest_snapshot(pr).is_none() {
                // Not enqueued at the latest observation: it entered.
                messages.push(Message {
                    pr,
                    event: Event::Queued,
                });
                false
            } else {
                let thread = self.api.list_comments(pr)?;
                if messages::needs_greeting(&thread) {
                    messages.push(Message {
                        pr,
                        event: Event::Queued,
                    });
                }
                messages::last_transition_marker(&thread) == Some(Marker::Held)
            };
            let holds = match &snapshot.status {
                PrStatus::Waiting(holds) => told_holds(holds),
                PrStatus::Ready | PrStatus::Conflicting(_) => BTreeSet::new(),
            };
            if !told_held && !holds.is_empty() {
                messages.push(Message {
                    pr,
                    event: Event::Held { holds },
                });
            }
        }
        Ok(messages)
    }

    /// The latest observation's snapshot of `pr`, if it reported it.
    fn latest_snapshot(&self, pr: PrNumber) -> Option<&PrSnapshot> {
        let latest = self.ledger.latest()?;
        let i = latest.prs.binary_search_by_key(&pr, |p| p.number).ok()?;
        Some(&latest.prs[i])
    }

    /// Whether the latest observation reported `snapshot`'s pull request at
    /// the same head, `Waiting` on holds its author is told about. A push
    /// that leaves it held is told again.
    fn held_at_latest(&self, snapshot: &PrSnapshot) -> bool {
        self.latest_snapshot(snapshot.number)
            .is_some_and(|p| match &p.status {
                PrStatus::Waiting(holds) => {
                    p.head == snapshot.head && !told_holds(holds).is_empty()
                }
                PrStatus::Ready | PrStatus::Conflicting(_) => false,
            })
    }

    fn tree_of(&mut self, commit: &Sha) -> Result<Sha, ForgeError> {
        Ok(self.api.get_commit(commit)?.tree)
    }

    /// Points `name` at `sha` whatever it held.
    fn force_ref(&mut self, name: &str, sha: &Sha) -> Result<(), ForgeError> {
        match self.api.update_ref(name, sha, true)? {
            RefUpdate::Updated => Ok(()),
            RefUpdate::NotFastForward => Err(permanent(format!(
                "a forced update of {name} to {sha} was refused as not a fast-forward"
            ))),
        }
    }

    /// Replaces a batch's record with a new commit holding `record`, whose
    /// tree is `tree` (the base's) and whose parent is the base.
    fn write_record(&mut self, record: &Record, tree: &Sha) -> Result<(), ForgeError> {
        let text = record.render().map_err(|e| permanent(e.to_string()))?;
        let sha = self
            .api
            .create_commit(&text, tree, std::slice::from_ref(&record.base))?;
        self.force_ref(&record_ref(record.id), &sha)
    }

    /// The merge commit message for `pr` in batch `id`.
    fn merge_message(&self, id: BatchId, pr: PrNumber) -> String {
        let title = &self
            .enqueued
            .get(&pr)
            .expect("a batch member is enqueued at the latest observation")
            .title;
        format!("Merge #{}: {title}\n\nDorfq-Batch: {}", pr.0, id.0)
    }

    /// What member `k` of a batch conflicted with when merged onto the
    /// earlier members: trunk when it is the first member; otherwise
    /// whatever the probe branch reproduces.
    fn attribute(
        &mut self,
        id: BatchId,
        base: &Sha,
        members: &[BatchMember],
        k: usize,
    ) -> Result<ConflictWith, ForgeError> {
        if k == 0 {
            return Ok(ConflictWith::Trunk);
        }
        let probe = branch(&probe_branch(id));
        self.api.create_ref(&probe, base)?;
        let outcome = self.probe(id, base, members, k);
        let deleted = self.api.delete_ref(&probe);
        let with = outcome?;
        deleted?;
        Ok(with)
    }

    /// On the probe branch: member `k` alone on the base, then each earlier
    /// member followed by `k`. The first conflict names the culprit; when no
    /// pair reproduces it, the last earlier member is named.
    fn probe(
        &mut self,
        id: BatchId,
        base: &Sha,
        members: &[BatchMember],
        k: usize,
    ) -> Result<ConflictWith, ForgeError> {
        let probe = probe_branch(id);
        let probe_ref = branch(&probe);
        let culprit = &members[k];
        let culprit_message = self.merge_message(id, culprit.pr);
        if self.api.merge(&probe, &culprit.head, &culprit_message)? == Merge::Conflict {
            return Ok(ConflictWith::Trunk);
        }
        for earlier in &members[..k] {
            self.force_ref(&probe_ref, base)?;
            let message = self.merge_message(id, earlier.pr);
            if self.api.merge(&probe, &earlier.head, &message)? == Merge::Conflict {
                continue;
            }
            if self.api.merge(&probe, &culprit.head, &culprit_message)? == Merge::Conflict {
                return Ok(ConflictWith::Pr(earlier.pr));
            }
        }
        Ok(ConflictWith::Pr(members[k - 1].pr))
    }

    /// The members of `status` the latest observation reported stacked,
    /// each with the pull request it was stacked on and that one's head
    /// branch: those `land` moves onto trunk.
    fn stacked_members(&self, status: &BatchStatus) -> Vec<Moved> {
        status
            .members
            .iter()
            .filter_map(|member| {
                let PrBase::Pr(base) = self.latest_snapshot(member.pr)?.base else {
                    return None;
                };
                let branch = self.enqueued.get(&base)?.head_branch.clone()?;
                Some(Moved {
                    pr: member.pr,
                    base,
                    branch,
                })
            })
            .collect()
    }

    /// Rule 2 against the world as it is now, the move onto trunk of each
    /// member in `moves`, then the fast-forward; the refusal, or `None` when
    /// trunk is now `tip`.
    fn fast_forward(
        &mut self,
        status: &BatchStatus,
        tip: &Sha,
        moves: &[Moved],
    ) -> Result<Option<LandRefusal>, ForgeError> {
        let trunk = self.trunk()?;
        if trunk != status.base {
            return Ok(Some(LandRefusal::TrunkMoved { trunk }));
        }
        let mut stacked = Vec::new();
        for member in &status.members {
            // Landing a member the repository's rules hold would bypass them.
            if self.enqueued.get(&member.pr).is_some_and(Enqueued::vetoed) {
                return Ok(Some(LandRefusal::MemberHeld { pr: member.pr }));
            }
            let expected = self.latest_snapshot(member.pr).and_then(|p| match p.base {
                PrBase::Trunk => Some(self.settings.trunk.clone()),
                PrBase::Pr(_) => moves
                    .iter()
                    .find(|m| m.pr == member.pr)
                    .map(|m| m.branch.clone()),
            });
            let pr = self.api.get_pr(member.pr)?;
            // A member the latest observation does not report departed while
            // its batch was in flight.
            let enqueued = labelled(&pr, &self.settings.label)
                && expected.as_ref() == Some(&pr.base)
                && pr.head == member.head;
            if !enqueued {
                return Ok(Some(LandRefusal::MemberGone { pr: member.pr }));
            }
            // The verdict at the landing, by the observation's rule with the
            // member's CI passed, as it was at this head when the batch
            // started: a draft, or `blocked` where the queue's own lock on a
            // stacked member's base does not explain it.
            let lock_blocks =
                expected.as_ref() != Some(&self.settings.trunk) && self.locks_branch(&pr.base);
            if pr.draft || (pr.merge_state == MergeState::Blocked && !lock_blocks) {
                return Ok(Some(LandRefusal::MemberHeld { pr: member.pr }));
            }
            if let Some(m) = moves.iter().find(|m| m.pr == member.pr) {
                stacked.push(m);
            }
        }
        // GitHub refuses a base whose branch already contains the head, so a
        // stacked member moves onto trunk before trunk reaches its head. A
        // refused move does not stop the landing: the member's content
        // reaches trunk whatever its base reads. A batch that does not land
        // puts each moved member back on its base, here or, from the
        // `landing` record, at the next observation, so it is not reported on
        // trunk while its base is unlanded.
        let mut moved = Vec::new();
        for m in stacked {
            match self.api.set_base(m.pr, &self.settings.trunk) {
                Ok(()) => moved.push(m),
                Err(ApiError::Permanent(error)) => eprintln!(
                    "dorfq: could not retarget #{} onto {} before landing batch {}: {error}",
                    m.pr.0, self.settings.trunk, status.id.0
                ),
                Err(error) => {
                    // The move may have applied with its response lost.
                    moved.push(m);
                    return Err(match self.move_back(&moved) {
                        Err(back @ ApiError::Permanent(_)) => back.into(),
                        Ok(()) | Err(ApiError::Transient(_)) => error.into(),
                    });
                }
            }
        }
        let trunk_ref = branch(&self.settings.trunk);
        match self.api.update_ref(&trunk_ref, tip, false)? {
            RefUpdate::Updated => Ok(None),
            RefUpdate::NotFastForward => {
                // A member whose base landed in this plan has its base's
                // content in trunk, and trunk is its honest base.
                let back: Vec<&Moved> = moved
                    .into_iter()
                    .filter(|m| !self.completed.contains_key(&m.base))
                    .collect();
                self.move_back(&back)?;
                Ok(Some(LandRefusal::TrunkMoved {
                    trunk: self.trunk()?,
                }))
            }
        }
    }

    /// Puts each of `moved` back on its base branch, after its batch did not
    /// land; every move is attempted, and the first error is returned.
    fn move_back(&mut self, moved: &[&Moved]) -> Result<(), ApiError> {
        let mut first = Ok(());
        for m in moved {
            if let Err(error) = self.api.set_base(m.pr, &m.branch)
                && first.is_ok()
            {
                first = Err(error);
            }
        }
        first
    }

    /// Puts `m`, moved onto trunk by a landing that did not complete, back on
    /// its base branch, unless it is no longer open on trunk or its base has
    /// merged into trunk.
    /// One put back is due, and its cached base is the branch written.
    fn put_back(&mut self, m: &Moved) -> Result<(), ForgeError> {
        let pr = self.api.get_pr(m.pr)?;
        if !pr.open || pr.base != self.settings.trunk {
            return Ok(());
        }
        let base = self.api.get_pr(m.base)?;
        if base.merged && base.base == self.settings.trunk {
            return Ok(());
        }
        self.mark_due(Entity::Pr(m.pr));
        self.api.set_base(m.pr, &m.branch)?;
        if let Some(facts) = self.facts.get_mut(&m.pr) {
            facts.detail.base = m.branch.clone();
        }
        Ok(())
    }

    /// The members of every batch the queue tracks now: those the latest
    /// observation reported and no `retire` removed, and those started
    /// since; less every pull request an effect since took out of
    /// knownness.
    fn batched_now(&self) -> impl Iterator<Item = PrNumber> + '_ {
        let observed = self
            .ledger
            .latest()
            .into_iter()
            .flat_map(|o| &o.batches)
            .filter(|b| self.ledger.batch_known(b.id))
            .flat_map(|b| b.members.iter().map(|m| m.pr));
        observed
            .chain(self.batched.values().flatten().copied())
            .filter(|&pr| self.ledger.pr_known(pr))
    }

    /// Records that `start_batch` created `batch`, in the ledger and with
    /// its members.
    fn started(&mut self, batch: &BatchStart) {
        self.ledger.started(batch.id);
        self.batched.insert(batch.id, batch.prs.clone());
    }

    /// Cancels, in id order, every workflow run on `tip` that is still going
    /// on `batch`'s branch; a run of the same commit on another branch is
    /// left alone. The first error is returned with its own kind.
    fn cancel_runs(&mut self, batch: BatchId, tip: &Sha) -> Result<(), ApiError> {
        let branch = batch_branch(batch);
        let mut runs: Vec<RunId> = self
            .api
            .list_runs(tip)?
            .into_iter()
            .filter(|run| !run.completed && run.head_branch.as_deref() == Some(branch.as_str()))
            .map(|run| run.id)
            .collect();
        runs.sort();
        for id in runs {
            self.api.cancel_run(id)?;
        }
        Ok(())
    }

    /// Makes the queue's ruleset lock exactly `want`: created for the first
    /// branch, updated on a change, deleted with the last. A failed write
    /// leaves `locked` as it was, so the next call tries again; a ruleset
    /// deleted by hand is created again. With `lock_heads` off it writes
    /// nothing.
    fn sync_locks(&mut self, want: BTreeSet<String>) -> Result<(), ForgeError> {
        if !self.settings.lock_heads {
            return Ok(());
        }
        match self.locked.clone() {
            None if want.is_empty() => {}
            None => {
                let id = self.lock_anew(&want)?;
                self.locked = Some((id, want));
            }
            Some((id, _)) if want.is_empty() => {
                self.api.delete_ruleset(id)?;
                self.locked = None;
            }
            Some((_, have)) if have == want => {}
            Some((id, _)) => {
                if self.api.update_ruleset(id, &want)? {
                    self.locked = Some((id, want));
                } else {
                    // Deleted outside the queue: made again.
                    self.locked = None;
                    let id = self.lock_anew(&want)?;
                    self.locked = Some((id, want));
                }
            }
        }
        Ok(())
    }

    /// Makes a ruleset the forge does not hold lock `want`: one of the
    /// queue's name is read first, since a creation whose response was lost
    /// may have applied, and updated if found; otherwise one is created.
    fn lock_anew(&mut self, want: &BTreeSet<String>) -> Result<RulesetId, ForgeError> {
        if let Some((id, _)) = self.api.find_ruleset(LOCK_RULESET)?
            && self.api.update_ruleset(id, want)?
        {
            return Ok(id);
        }
        Ok(self.api.create_ruleset(LOCK_RULESET, want)?)
    }

    fn trunk(&mut self) -> Result<Sha, ForgeError> {
        let trunk_ref = branch(&self.settings.trunk);
        self.api
            .get_ref(&trunk_ref)?
            .ok_or_else(|| permanent(format!("trunk branch {trunk_ref} does not exist")))
    }

    /// A landed member leaves the queue: its label is removed, its landing is
    /// completed, and it is told it landed.
    fn complete_landing(
        &mut self,
        pr: PrNumber,
        batch: BatchId,
        tip: &Sha,
    ) -> Result<(), ForgeError> {
        self.mark_due(Entity::Pr(pr));
        self.api.remove_label(pr, &self.settings.label)?;
        self.completed.insert(pr, batch);
        self.outbox.push_back(Message {
            pr,
            event: Event::Landed {
                batch,
                tip: tip.clone(),
            },
        });
        Ok(())
    }

    /// Reads and derives the repository's queue state; once the ledger has
    /// accepted it, appends to the outbox the transitions since the previous
    /// observation that [`messages`] names. A failure appends nothing.
    fn observe_unposted(&mut self) -> Result<Observation, ForgeError> {
        if self.listener_gone {
            return Err(permanent("webhook listener thread exited".to_owned()));
        }
        let now = (self.clock)().map_err(|e| permanent(format!("reading the clock: {e}")))?;
        self.refresh_due(now)?;
        let World {
            observation,
            enqueued,
            counted,
            blocked,
        } = self.derive(now)?;
        let entered: BTreeSet<PrNumber> = observation
            .prs
            .iter()
            .map(|pr| pr.number)
            .filter(|&pr| !self.ledger.pr_known(pr))
            .collect();
        // Every read that can fail comes before the ledger moves on, so a
        // failed observation is retried against the same latest one.
        let messages = self.transitions(&observation, &entered, &blocked, &enqueued)?;
        let members = observation
            .batches
            .iter()
            .flat_map(|b| b.members.iter().map(|m| m.pr));
        self.sync_locks(locked_heads(members, &enqueued, &self.settings))?;
        self.ledger.observed(&observation)?;
        // A pull request this observation found closed or unlabelled was
        // reported departed by it, or was never reported.
        let gone: Vec<PrNumber> = self
            .facts
            .iter()
            .filter(|(_, f)| !labelled(&f.detail, &self.settings.label))
            .map(|(&n, _)| n)
            .collect();
        for number in gone {
            self.facts.remove(&number);
            self.schedule.forget(Entity::Pr(number));
        }
        // The derivation asks ancestry only of trunk and of cached heads.
        let heads: BTreeSet<&Sha> = self.facts.values().map(|f| &f.detail.head).collect();
        let trunk = self.trunk_at.as_ref();
        self.ancestry
            .retain(|(_, descendant), _| Some(descendant) == trunk || heads.contains(descendant));
        self.told = blocked;
        self.completed.clear();
        self.ejecting.clear();
        self.batched.clear();
        self.enqueued = enqueued;
        self.counted = counted;
        self.outbox.extend(messages);
        Ok(observation)
    }
}

impl Forge for GitHubForge {
    /// Reports the repository's queue state (rules 1 and 8); once the ledger
    /// has accepted it, tells authors the transitions since the previous
    /// observation that [`messages`] names (rule 9). Posts them, and anything
    /// an earlier flush kept, before returning; a failed observation still
    /// posts what earlier ones kept.
    fn observe(&mut self) -> Result<Observation, ForgeError> {
        let observed = self.observe_unposted();
        self.flush()?;
        observed
    }

    /// With `lock_heads` on, locks its members' head branches; records the
    /// batch as `started`, merges its members one by one onto its build
    /// branch, and publishes the batch branch at the tip; on a conflict,
    /// records who conflicted with what and publishes nothing. A probe's tip
    /// is an empty commit on its base.
    fn start_batch(&mut self, batch: &BatchStart) -> Result<(), ForgeError> {
        self.ledger.check_start(batch)?;
        self.mark_written(Some(batch.id), &batch.prs);
        let mut members = Vec::new();
        for &pr in &batch.prs {
            let snapshot = self
                .latest_snapshot(pr)
                .expect("check_start accepts only enqueued members");
            members.push(BatchMember::new(pr, snapshot.head.clone()));
        }
        let id = batch.id;
        let want = locked_heads(
            self.batched_now().chain(batch.prs.iter().copied()),
            &self.enqueued,
            &self.settings,
        );
        self.sync_locks(want)?;
        let tree = self.tree_of(&batch.base)?;
        let mut record = Record {
            id,
            base: batch.base.clone(),
            members,
            moved: Vec::new(),
            state: RecordState::Started,
        };
        let text = record.render().map_err(|e| permanent(e.to_string()))?;
        let started = self
            .api
            .create_commit(&text, &tree, std::slice::from_ref(&batch.base))?;
        self.api.create_ref(&record_ref(id), &started)?;

        let build = build_branch(id);
        let build_ref = branch(&build);
        self.api.create_ref(&build_ref, &batch.base)?;
        let mut tip = batch.base.clone();
        let mut conflict = None;
        for (k, member) in record.members.iter().enumerate() {
            let message = self.merge_message(id, member.pr);
            match self.api.merge(&build, &member.head, &message)? {
                Merge::Merged(sha) => tip = sha,
                Merge::NothingToMerge => {}
                Merge::Conflict => {
                    conflict = Some(k);
                    break;
                }
            }
        }

        if let Some(k) = conflict {
            let with = self.attribute(id, &batch.base, &record.members, k)?;
            self.api.delete_ref(&build_ref)?;
            record.state = RecordState::Conflict {
                pr: record.members[k].pr,
                with,
            };
            self.write_record(&record, &tree)?;
            self.started(batch);
            return self.flush();
        }

        if tip == batch.base {
            // A probe, or a batch whose every member's head is already in
            // the base. The tip is an empty commit of its own, so no batch's
            // tip is its base.
            let kind = if record.members.is_empty() {
                "probe"
            } else {
                "batch"
            };
            let message = format!("dorfq {kind} {}", id.0);
            tip = self
                .api
                .create_commit(&message, &tree, std::slice::from_ref(&batch.base))?;
        }
        self.api.create_ref(&branch(&batch_branch(id)), &tip)?;
        self.api.delete_ref(&build_ref)?;
        record.state = RecordState::Testing { tip: tip.clone() };
        self.write_record(&record, &tree)?;
        self.started(batch);
        for member in &record.members {
            let locked = self
                .enqueued
                .get(&member.pr)
                .is_some_and(|e| self.settings.locks(e));
            self.outbox.push_back(Message {
                pr: member.pr,
                event: Event::Testing {
                    batch: id,
                    tip: tip.clone(),
                    locked,
                },
            });
        }
        self.flush()
    }

    /// Records the landing, checks rule 2 against the world as it is now,
    /// moves each stacked member onto trunk, and fast-forwards trunk to the
    /// tip; a refusal is recorded and returns `Ok`. After the fast-forward
    /// each member's label is removed and it is told it landed.
    fn land(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        let landable = self.ledger.check_land(batch)?;
        let (tip, status) = (landable.tip, landable.status.clone());
        let members: Vec<PrNumber> = status.members.iter().map(|m| m.pr).collect();
        self.mark_written(Some(batch), &members);
        self.mark_due(Entity::Trunk);
        self.ledger.landing(batch);
        let tree = self.tree_of(&status.base)?;
        let moves = self.stacked_members(&status);
        let mut record = Record {
            id: batch,
            base: status.base.clone(),
            members: status.members.clone(),
            moved: moves.clone(),
            state: RecordState::Landing { tip: tip.clone() },
        };
        self.write_record(&record, &tree)?;

        if let Some(reason) = self.fast_forward(&status, &tip, &moves)? {
            record.state = RecordState::Refused { tip, reason };
            self.write_record(&record, &tree)?;
            return self.flush();
        }

        self.ledger.landed(batch);
        record.state = RecordState::Landed { tip: tip.clone() };
        self.write_record(&record, &tree)?;
        for member in &status.members {
            self.complete_landing(member.pr, batch, &tip)?;
        }
        self.flush()
    }

    /// Deletes the record and then the batch branch. A batch reported `Landed`
    /// first completes the landing for every member still known. With
    /// [`Settings::ci`] `Actions`, a batch reported `Testing` has the
    /// workflow runs still going on its branch cancelled once no observation
    /// can report the batch, so a cancelled run is never read as its
    /// verdict; a failed cancel is returned after the batch is gone. The
    /// cancel precedes the lock sync: nothing retries a cancel, while a
    /// lock sync skipped by a failure here is made by the next observation.
    fn retire(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        self.ledger.check_retire(batch)?;
        let reported = self
            .ledger
            .latest()
            .and_then(|o| o.batches.iter().find(|b| b.id == batch));
        let landed = reported.and_then(|b| match &b.state {
            BatchState::Landed { tip } => Some((b.members.clone(), tip.clone())),
            BatchState::Conflict { .. }
            | BatchState::Testing { .. }
            | BatchState::LandRefused { .. } => None,
        });
        let testing_tip = reported.and_then(|b| match &b.state {
            BatchState::Testing { tip, .. } => Some(tip.clone()),
            BatchState::Conflict { .. }
            | BatchState::Landed { .. }
            | BatchState::LandRefused { .. } => None,
        });
        self.mark_written(Some(batch), &[]);
        if let Some((members, tip)) = landed {
            for member in members {
                if self.ledger.pr_known(member.pr) {
                    self.complete_landing(member.pr, batch, &tip)?;
                }
            }
        }
        // The record goes first: a record whose batch branch is gone reads
        // back as `Permanent`, a batch branch with no record is never read.
        self.api.delete_ref(&record_ref(batch))?;
        self.api.delete_ref(&branch(&batch_branch(batch)))?;
        self.ledger.retired(batch);
        if let (Some(Ci::Actions), Some(tip)) = (self.settings.ci, testing_tip) {
            self.cancel_runs(batch, &tip)?;
        }
        let want = locked_heads(self.batched_now(), &self.enqueued, &self.settings);
        self.sync_locks(want)?;
        self.flush()
    }

    /// Removes the intent label and tells the author why. A removal that
    /// fails is remembered until the next observation, which tells the
    /// author why if the pull request has left.
    fn eject(&mut self, pr: PrNumber, reason: &EjectReason) -> Result<(), ForgeError> {
        self.ledger.check_eject(pr)?;
        self.mark_written(None, &[pr]);
        let base = self
            .latest_snapshot(pr)
            .expect("check_eject accepts only enqueued pull requests")
            .base;
        self.ejecting.insert(pr, reason.clone());
        self.api.remove_label(pr, &self.settings.label)?;
        self.ejecting.remove(&pr);
        self.ledger.ejected(pr);
        self.outbox.push_back(Message {
            pr,
            event: Event::Ejected {
                reason: reason.clone(),
                base,
            },
        });
        let want = locked_heads(self.batched_now(), &self.enqueued, &self.settings);
        self.sync_locks(want)?;
        self.flush()
    }

    /// Tells the author what the scheduler believes; removes nothing. The
    /// comment goes through the outbox, so a transient failure to post it
    /// keeps it for the next flush.
    fn notify(&mut self, pr: PrNumber, reason: &NoticeReason) -> Result<(), ForgeError> {
        self.ledger.check_notify(pr)?;
        self.ledger.notified(pr);
        self.outbox.push_back(Message {
            pr,
            event: Event::Notice {
                reason: reason.clone(),
            },
        });
        self.flush()
    }
}

/// Whether `pr` is open and carries `label`: queued, or meant to be.
fn labelled(pr: &PrDetail, label: &str) -> bool {
    pr.open && pr.labels.iter().any(|l| l == label)
}

/// The stderr line for a read of pull request `number`, which the cache
/// held or not and which is `queued` or not under `label`: `Some` for a
/// first read that finds it not queued.
fn not_queued_line(number: PrNumber, cached: bool, queued: bool, label: &str) -> Option<String> {
    (!cached && !queued).then(|| {
        format!(
            "dorfq: pull request #{}: read; not open and labelled {label:?}; not queued",
            number.0
        )
    })
}

/// The enqueued set over the cached pull requests, with each member's base:
/// open labelled pull requests against trunk or in `landed`, on trunk, then,
/// round by round, open labelled pull requests whose base branch is the head
/// branch of one enqueued in an earlier round, based on the lowest-numbered of
/// those. A base always enters in an earlier round than its child, so the
/// result is acyclic and closed under `base`.
fn enqueued_bases(
    facts: &BTreeMap<PrNumber, PrFacts>,
    settings: &Settings,
    landed: &BTreeSet<PrNumber>,
) -> BTreeMap<PrNumber, PrBase> {
    let labelled: Vec<(PrNumber, &PrDetail)> = facts
        .iter()
        .filter(|(_, f)| labelled(&f.detail, &settings.label))
        .map(|(&n, f)| (n, &f.detail))
        .collect();
    let mut bases = BTreeMap::new();
    let mut by_branch: BTreeMap<&str, PrNumber> = BTreeMap::new();
    loop {
        let round: Vec<(PrNumber, &PrDetail, PrBase)> = labelled
            .iter()
            .filter(|(n, _)| !bases.contains_key(n))
            .filter_map(|&(n, pr)| {
                let base = if pr.base == settings.trunk || landed.contains(&n) {
                    PrBase::Trunk
                } else {
                    PrBase::Pr(*by_branch.get(pr.base.as_str())?)
                };
                Some((n, pr, base))
            })
            .collect();
        if round.is_empty() {
            return bases;
        }
        for (number, pr, base) in round {
            bases.insert(number, base);
            if let Some(head_branch) = &pr.head_branch {
                by_branch
                    .entry(head_branch.as_str())
                    .and_modify(|n| *n = (*n).min(number))
                    .or_insert(number);
            }
        }
    }
}

/// The head branches of `members` that `enqueued` holds and `settings`
/// locks: what the queue locks. Empty with `lock_heads` off.
fn locked_heads(
    members: impl Iterator<Item = PrNumber>,
    enqueued: &BTreeMap<PrNumber, Enqueued>,
    settings: &Settings,
) -> BTreeSet<String> {
    members
        .filter_map(|pr| enqueued.get(&pr))
        .filter(|e| settings.locks(e))
        .filter_map(|e| e.head_branch.clone())
        .collect()
}

/// The lowest-numbered open pull request in `facts` other than `pr` whose
/// head branch is `branch`.
fn holder_in(facts: &BTreeMap<PrNumber, PrFacts>, pr: PrNumber, branch: &str) -> Option<OpenPr> {
    facts
        .iter()
        .find(|(n, f)| {
            **n != pr && f.detail.open && f.detail.head_branch.as_deref() == Some(branch)
        })
        .map(|(&number, f)| OpenPr {
            number,
            head: f.detail.head.clone(),
            base: f.detail.base.clone(),
        })
}

/// Whether a pull request the latest observation reported with base
/// `reported` was stacked on a pull request whose recorded head branch is
/// `base`, and whether `completed`, the pull requests whose landing this
/// process completed since then, holds that one.
fn reported_stacked(
    reported: PrBase,
    enqueued: &BTreeMap<PrNumber, Enqueued>,
    completed: &BTreeMap<PrNumber, BatchId>,
    base: &str,
) -> Stacked {
    match reported {
        PrBase::Pr(a) if enqueued.get(&a).and_then(|e| e.head_branch.as_deref()) == Some(base) => {
            if completed.contains_key(&a) {
                Stacked::OnLanded
            } else {
                Stacked::OnUnlanded
            }
        }
        _ => Stacked::No,
    }
}

/// What a labelled pull request whose base branch `base` is neither trunk
/// (`trunk_name`, at `trunk`) nor an enqueued pull request's head is.
/// `stacked` is what the latest observation says it was queued on, `holder`
/// the lowest-numbered other open pull request whose head branch is `base`,
/// and `merged` whether, with no holder, `base`'s tip is the recorded head of
/// a closed pull request merged into trunk.
///
/// - Stacked on one this process landed since: a merged base.
/// - Otherwise, with a holder: when stacked, a merged base if the holder
///   targets trunk with its head in trunk, else blocked on it; when not
///   stacked, blocked on the branch if its head is in trunk, else blocked on
///   it.
/// - With none: when stacked and `merged`, a merged base; blocked on the
///   branch otherwise.
fn classify(
    ancestry: &mut Ancestry<'_>,
    trunk_name: &str,
    base: &str,
    stacked: Stacked,
    holder: Option<&OpenPr>,
    merged: bool,
    trunk: &Sha,
) -> Result<Class, ForgeError> {
    if stacked == Stacked::OnLanded {
        return Ok(Class::MergedBase);
    }
    let on_branch = || Class::Blocked(Blocker::Branch(base.to_owned()));
    if let Some(a) = holder {
        let in_trunk = ancestry.is(&a.head, trunk)?;
        return Ok(match stacked {
            Stacked::OnUnlanded if in_trunk && a.base == trunk_name => Class::MergedBase,
            Stacked::No if in_trunk => on_branch(),
            _ => Class::Blocked(Blocker::Pr(a.number)),
        });
    }
    Ok(if stacked == Stacked::OnUnlanded && merged {
        Class::MergedBase
    } else {
        on_branch()
    })
}

/// A batch branch must be where the record says the batch was published.
fn expect_published(name: &str, published: Option<&Sha>, tip: &Sha) -> Result<(), ForgeError> {
    match published {
        Some(sha) if sha == tip => Ok(()),
        Some(sha) => Err(permanent(format!(
            "{name} is at {sha}, not at the batch's tip {tip}: it was pushed outside the queue"
        ))),
        None => Err(permanent(format!(
            "{name} is missing: the batch branch was deleted outside the queue"
        ))),
    }
}
