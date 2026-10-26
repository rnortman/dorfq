//! DorfQ's forge for GitHub, over the REST API.
//!
//! The forge is an HTTP client and runs no git: GitHub's merge endpoint
//! builds a batch, a fast-forward-only ref update lands it, and every piece
//! of queue state lives in the repository itself, in the layout below.
//!
//! | Name | What it is |
//! |---|---|
//! | the configured label | intent: an open pull request against trunk, or against the head branch of another enqueued pull request, carrying it is enqueued |
//! | `refs/heads/dorfq/batch/<id>` | a tracked batch's tip; exists from the end of a successful build until `retire`; CI must run on pushes to it |
//! | `refs/heads/dorfq/build/<id>` | the branch a batch's merges are made on; exists only during `start_batch` |
//! | `refs/heads/dorfq/probe/<id>` | the branch conflict attribution uses; exists only during `start_batch` |
//! | `refs/dorfq/batches/<id>` | the batch's record, a commit whose message is the [`record`] text |
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
//! When the batch then does not land — the fast-forward is refused, or a
//! later call fails and trunk is not at the tip — each moved member is put
//! back on its base branch; a move back that fails is a line on stderr. A
//! process that dies between the moves and the fast-forward leaves them on
//! trunk.
//!
//! # Staleness
//!
//! GitHub's checks on a head do not say what trunk the run merged with, so a
//! pull request's [`PrSnapshot::behind`] is the commits trunk has that its
//! head does not: the changes since the head's merge base with trunk, read
//! fresh at every observation. It counts commits, not pull requests, so it
//! overstates by the commits each landing brings; and it does not fall when
//! a check re-runs on the same head.
//!
//! # Recovery
//!
//! [`GitHubForge`]'s `observe` reads every tracked batch back from its record
//! and its batch branch, so a restarted process sees what the previous one
//! left:
//!
//! | record state | batch branch | reported as |
//! |---|---|---|
//! | `started` | any | nothing: a build that died; its branches and record are deleted |
//! | `testing <tip>` | at `<tip>` | `Testing { tip, ci }` |
//! | `conflict …` | absent | `Conflict { pr, with }`; a leftover build or probe branch is deleted |
//! | `landing <tip>` | at `<tip>` | `Landed { tip }` if `<tip>` is in trunk's history, else `Testing { tip, ci }` |
//! | `landed <tip>` | any | `Landed { tip }` |
//! | `refused <tip> …` | any | `LandRefused { tip, reason }` |
//!
//! Any other combination, a record that does not parse, or a ref under
//! `refs/dorfq/batches/` whose suffix is not a batch id is
//! [`ForgeError::Permanent`]: someone else is using the queue's namespace.
//!
//! - [`api`]: the REST calls, behind the [`api::GitHubApi`] trait.
//! - [`app`]: the GitHub App credential: the private key, and the JWT it
//!   signs.
//! - [`checks`]: required check names on a commit, reduced to a CI status.
//! - [`messages`]: what authors are told, as marked issue comments.
//! - [`record`]: the batch record's text.

pub mod api;
pub mod app;
pub mod checks;
pub mod messages;
pub mod record;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use dorfq_core::ledger::Ledger;
use dorfq_core::{
    BatchId, BatchMember, BatchStart, BatchState, BatchStatus, CiStatus, ConflictWith, Departed,
    Departure, EjectReason, Forge, ForgeError, Hold, LandRefusal, Observation, PrBase, PrNumber,
    PrSnapshot, PrStatus, Sha, Timestamp, Withdrawal,
};

use api::{ApiError, GitHubApi, Merge, PrSummary, RefEntry, RefUpdate};
use messages::{Blocker, Event, Marker, Message};
use record::{Record, RecordState};

/// What the forge is configured with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The branch the queue lands on, by short name (`main`).
    pub trunk: String,
    /// The intent label.
    pub label: String,
    /// The check names that must pass, on pull request heads and batch tips.
    pub checks: Vec<String>,
}

impl Settings {
    /// Rejects an empty `trunk` or `label`, no `checks`, or an empty check
    /// name. The message begins with the field's name (`trunk is empty`,
    /// `checks[1] is empty`).
    pub fn validate(&self) -> Result<(), String> {
        if self.trunk.is_empty() {
            return Err("trunk is empty".to_owned());
        }
        if self.label.is_empty() {
            return Err("label is empty".to_owned());
        }
        if self.checks.is_empty() {
            return Err("checks names no check".to_owned());
        }
        if let Some(empty) = self.checks.iter().position(String::is_empty) {
            return Err(format!("checks[{empty}] is empty"));
        }
        Ok(())
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
    /// What the effects need about the pull requests enqueued at the latest
    /// observation.
    enqueued: BTreeMap<PrNumber, Enqueued>,
    /// Comments not yet posted, in order.
    outbox: VecDeque<Message>,
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
}

/// What one read of the repository found.
struct World {
    observation: Observation,
    enqueued: BTreeMap<PrNumber, Enqueued>,
    /// Every labelled open pull request in the listing that is not
    /// enqueued, with the reason.
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

fn transient(message: String) -> ForgeError {
    ForgeError::Transient(message.into())
}

/// Whether a pull request held by `holds` is told so. `MergeabilityUnknown`
/// alone is GitHub catching up after trunk moved and says nothing about the
/// pull request.
fn tells_author(holds: &BTreeSet<Hold>) -> bool {
    holds.iter().any(|hold| *hold != Hold::MergeabilityUnknown)
}

impl GitHubForge {
    /// A forge over the repository `api` reaches, reading time from
    /// `clock`. Checks that trunk and the label exist; writes nothing.
    pub fn new(
        mut api: Box<dyn GitHubApi>,
        settings: Settings,
        clock: Clock,
        sink: Box<dyn Write>,
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
        Ok(GitHubForge {
            api,
            settings,
            clock,
            ledger: Ledger::new(),
            completed: BTreeMap::new(),
            enqueued: BTreeMap::new(),
            outbox: VecDeque::new(),
            sink,
        })
    }

    /// Posts the outbox in order. A transient failure keeps the comment and
    /// everything after it for the next flush; a permanent one drops the
    /// comment with a line on stderr.
    fn flush(&mut self) -> Result<(), ForgeError> {
        while let Some(message) = self.outbox.front() {
            let body = message.body(&self.settings.trunk, &self.settings.label);
            match self.api.create_comment(message.pr, &body) {
                Ok(()) => {
                    writeln!(self.sink, "{message}")
                        .map_err(|e| ForgeError::Permanent(Box::new(e)))?;
                }
                Err(ApiError::Transient(_)) => return Ok(()),
                Err(error) => eprintln!("dorfq: dropped the comment for {message}: {error}"),
            }
            self.outbox.pop_front();
        }
        Ok(())
    }

    /// The observation, what the effects need about the pull requests it
    /// reports, and why each labelled open pull request it does not report
    /// is not enqueued.
    fn fetch_world(&mut self) -> Result<World, ForgeError> {
        let now = (self.clock)().map_err(|e| permanent(format!("reading the clock: {e}")))?;
        let trunk = self.trunk()?;

        // The listing pages by offset, so a pull request opened between two
        // page reads lists the item on the boundary twice. Kept once at the
        // same head; listed at two heads, the listing is read again next tick.
        let mut listed: BTreeMap<PrNumber, PrSummary> = BTreeMap::new();
        for summary in self.api.list_open_prs()? {
            match listed.get(&summary.number) {
                Some(seen) if seen.head == summary.head => {}
                Some(seen) => {
                    return Err(transient(format!(
                        "the listing of open pull requests reported #{} at {} and at {}",
                        summary.number.0, seen.head, summary.head
                    )));
                }
                None => {
                    listed.insert(summary.number, summary);
                }
            }
        }

        let mut batches = Vec::new();
        for entry in self.api.list_refs(RECORDS)? {
            if let Some(status) = self.recover(&entry, &trunk)? {
                batches.push(status);
            }
        }
        // A landed member's content is in trunk, whatever its base on GitHub.
        let landed: BTreeSet<PrNumber> = batches
            .iter()
            .filter(|b| matches!(b.state, BatchState::Landed { .. }))
            .flat_map(|b| b.members.iter().map(|m| m.pr))
            .collect();

        // A merged base retargeted onto trunk can carry pull requests of its
        // own, so the enqueued set is computed again after every retarget. A
        // retarget changes only the retargeted pull request's base, which
        // can bring a blocked pull request into the enqueued set but never
        // changes the class of one it leaves out: each is classified once.
        let mut retargeted = BTreeSet::new();
        let mut blocked = BTreeMap::new();
        let bases = loop {
            let bases = enqueued_bases(&listed, &self.settings, &landed);
            blocked.retain(|n, _| !bases.contains_key(n));
            let left: Vec<PrNumber> = listed
                .values()
                .filter(|s| s.labels.contains(&self.settings.label))
                .map(|s| s.number)
                .filter(|n| !bases.contains_key(n) && !blocked.contains_key(n))
                .collect();
            let mut moved = false;
            for number in left {
                let base = listed[&number].base.clone();
                let stacked = self.latest_snapshot(number).map_or(Stacked::No, |p| {
                    reported_stacked(p.base, &self.enqueued, &self.completed, &base)
                });
                let class = classify(
                    self.api.as_mut(),
                    &self.settings.trunk,
                    number,
                    &base,
                    stacked,
                    &listed,
                    &trunk,
                )?;
                match class {
                    Class::MergedBase => match self.api.set_base(number, &self.settings.trunk) {
                        Ok(()) => {
                            if let Some(summary) = listed.get_mut(&number) {
                                summary.base = self.settings.trunk.clone();
                            }
                            retargeted.insert(number);
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

        let mut prs = Vec::new();
        let mut enqueued = BTreeMap::new();
        for (&number, &base) in &bases {
            let summary = &listed[&number];
            // GitHub recomputes mergeability after a change of base.
            let mergeable = if retargeted.contains(&number) {
                None
            } else {
                self.api.get_pr(number)?.mergeable
            };
            let status = match mergeable {
                Some(false) => PrStatus::Conflicting(match base {
                    PrBase::Trunk => ConflictWith::Trunk,
                    PrBase::Pr(a) => ConflictWith::Pr(a),
                }),
                mergeable => {
                    let mut holds = BTreeSet::new();
                    if mergeable.is_none() {
                        holds.insert(Hold::MergeabilityUnknown);
                    }
                    if let PrBase::Pr(a) = base
                        && !self.api.is_ancestor(&listed[&a].head, &summary.head)?
                    {
                        holds.insert(Hold::AncestorMoved { pr: a });
                    }
                    match self.ci(&summary.head)? {
                        CiStatus::Passed => {}
                        CiStatus::Pending => {
                            holds.insert(Hold::OwnCiPending);
                        }
                        CiStatus::Failed(_) => {
                            holds.insert(Hold::OwnCiFailed);
                        }
                    }
                    if holds.is_empty() {
                        PrStatus::Ready
                    } else {
                        PrStatus::Waiting(holds)
                    }
                }
            };
            enqueued.insert(
                number,
                Enqueued {
                    title: summary.title.clone(),
                    head_branch: summary.head_branch.clone(),
                },
            );
            // TODO(github-behind-every-tick): asks GitHub every tick for
            // what the queue's own landings already determine.
            // TODO(github-behind-counts-commits): counts commits, not
            // merge commits.
            // TODO(github-behind-tested-trunk): always the merge base; the
            // estimate of the trunk own CI tested should be configurable.
            let behind = self.api.compare(&summary.head, &trunk)?.ahead_by;
            prs.push(PrSnapshot::new(
                number,
                summary.head.clone(),
                base,
                status,
                behind,
            ));
        }

        prs.sort_by_key(|pr| pr.number);
        batches.sort_by_key(|batch| batch.id);
        let departed = self.ledger.departures(&prs, &batches, |gone| {
            if let Some(&batch) = self.completed.get(&gone.number) {
                return Ok(Departed::Landed { batch });
            }
            let blocked_on = |blocker: &Blocker| match (blocker, gone.base) {
                (Blocker::Pr(a), PrBase::Pr(b)) if *a == b => {
                    Departed::Withdrawn(Withdrawal::Blocked { on: b })
                }
                _ => Departed::Withdrawn(Withdrawal::Retargeted),
            };
            // The listing holds it: this tick already decided what it is.
            if listed.contains_key(&gone.number) {
                return Ok(match blocked.get(&gone.number) {
                    Some(blocker) => blocked_on(blocker),
                    None => Departed::Withdrawn(Withdrawal::IntentCleared),
                });
            }
            let pr = self.api.get_pr(gone.number)?;
            let trunk_name = &self.settings.trunk;
            if pr.open && pr.labels.contains(&self.settings.label) {
                let left_out = || {
                    // A pull request closed between two page reads moved it
                    // across a page boundary, or its label came back after
                    // the listing.
                    Err(transient(format!(
                        "#{} is enqueued but the listing of open pull requests left it out",
                        gone.number.0
                    )))
                };
                if pr.base == *trunk_name || has_enqueued_holder(&listed, &bases, &pr.base) {
                    return left_out();
                }
                let stacked =
                    reported_stacked(gone.base, &self.enqueued, &self.completed, &pr.base);
                return match classify(
                    self.api.as_mut(),
                    trunk_name,
                    gone.number,
                    &pr.base,
                    stacked,
                    &listed,
                    &trunk,
                )? {
                    Class::MergedBase => left_out(),
                    Class::Blocked(blocker) => Ok(blocked_on(&blocker)),
                };
            }
            Ok(if pr.merged && pr.base == *trunk_name {
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
            observation: Observation::new(now, trunk, prs, batches, departed),
            enqueued,
            blocked,
        })
    }

    /// The batch a record ref describes, per the recovery table; `None` for
    /// a build that died, which this call cleans up.
    fn recover(
        &mut self,
        entry: &RefEntry,
        trunk: &Sha,
    ) -> Result<Option<BatchStatus>, ForgeError> {
        let id = entry
            .name
            .strip_prefix(RECORDS)
            .and_then(record::parse_decimal)
            .map(BatchId)
            .ok_or_else(|| {
                permanent(format!(
                    "{} is not a batch record: the queue owns {RECORDS}",
                    entry.name
                ))
            })?;
        let commit = self.api.get_commit(&entry.sha)?;
        let record = Record::parse(&entry.name, id, &commit.message)
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
                self.api.delete_ref(&entry.name)?;
                return Ok(None);
            }
            RecordState::Testing { tip } => {
                expect_published(&published_ref, published.as_ref(), &tip)?;
                let ci = self.ci(&tip)?;
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
                // TODO(efficiency-conflict-recovery-reads-for-unreachable-leftovers)
                for leftover in [branch(&build_branch(id)), branch(&probe_branch(id))] {
                    if self.api.get_ref(&leftover)?.is_some() {
                        self.api.delete_ref(&leftover)?;
                    }
                }
                BatchState::Conflict { pr, with }
            }
            RecordState::Landing { tip } => {
                expect_published(&published_ref, published.as_ref(), &tip)?;
                if self.api.is_ancestor(&tip, trunk)? {
                    BatchState::Landed { tip }
                } else {
                    let ci = self.ci(&tip)?;
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

    fn ci(&mut self, sha: &Sha) -> Result<CiStatus, ForgeError> {
        let results = self.api.checks(sha)?;
        Ok(checks::evaluate(&self.settings.checks, &results))
    }

    /// The rule-9 messages for `observation`. A departure is told it landed
    /// when it landed with a batch and this process has not completed that
    /// landing, nothing when it was ejected, and that it withdrew otherwise.
    /// A departure in `blocked` is told nothing here: every pull request in
    /// `blocked` is told why it is not enqueued unless its comment thread
    /// already ends with that, naming the same blocker. A pull request in
    /// `entered` is greeted unless its comment thread shows it was greeted
    /// already. One that is `Waiting` and was not at the latest observation
    /// is told what holds it, unless it entered and its thread already ends
    /// with that. Holds that are only
    /// `MergeabilityUnknown` are not told: GitHub recomputes mergeability
    /// after every move of trunk, and the author has nothing to do.
    fn transitions(
        &mut self,
        observation: &Observation,
        entered: &BTreeSet<PrNumber>,
        blocked: &BTreeMap<PrNumber, Blocker>,
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
                Departed::Landed { .. } | Departed::Ejected => None,
                Departed::Merged | Departed::Withdrawn(_) => Some(Event::Withdrawn),
            };
            if let Some(event) = event {
                messages.push(Message { pr: *pr, event });
            }
        }
        for (&pr, blocker) in blocked {
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
        for snapshot in &observation.prs {
            let pr = snapshot.number;
            let told_held = if entered.contains(&pr) {
                let thread = self.api.list_comments(pr)?;
                if messages::needs_greeting(&thread) {
                    messages.push(Message {
                        pr,
                        event: Event::Queued,
                    });
                }
                messages::last_marker(&thread) == Some(Marker::Held)
            } else {
                self.held_at_latest(pr)
            };
            match &snapshot.status {
                PrStatus::Waiting(holds) if !told_held && tells_author(holds) => {
                    messages.push(Message {
                        pr,
                        event: Event::Held {
                            holds: holds.clone(),
                        },
                    })
                }
                PrStatus::Ready | PrStatus::Waiting(_) | PrStatus::Conflicting(_) => {}
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

    /// Whether the latest observation reported `pr` `Waiting` on holds its
    /// author is told about.
    fn held_at_latest(&self, pr: PrNumber) -> bool {
        self.latest_snapshot(pr).is_some_and(|p| match &p.status {
            PrStatus::Waiting(holds) => tells_author(holds),
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

    /// Rule 2 against the world as it is now, each stacked member's move onto
    /// trunk, then the fast-forward; the refusal, or `None` when trunk is now
    /// `tip`.
    fn fast_forward(
        &mut self,
        status: &BatchStatus,
        tip: &Sha,
    ) -> Result<Option<LandRefusal>, ForgeError> {
        let trunk = self.trunk()?;
        if trunk != status.base {
            return Ok(Some(LandRefusal::TrunkMoved { trunk }));
        }
        let mut bases = BTreeMap::new();
        for member in &status.members {
            let base = self.latest_snapshot(member.pr).and_then(|p| match p.base {
                PrBase::Trunk => Some(self.settings.trunk.clone()),
                PrBase::Pr(a) => self.enqueued.get(&a)?.head_branch.clone(),
            });
            bases.insert(member.pr, base);
        }
        let mut stacked = Vec::new();
        for member in &status.members {
            let pr = self.api.get_pr(member.pr)?;
            // A member the latest observation does not report departed while
            // its batch was in flight.
            let enqueued = pr.open
                && bases[&member.pr].as_ref() == Some(&pr.base)
                && pr.labels.contains(&self.settings.label)
                && pr.head == member.head;
            if !enqueued {
                return Ok(Some(LandRefusal::MemberGone { pr: member.pr }));
            }
            if pr.base != self.settings.trunk {
                stacked.push(member.pr);
            }
        }
        // GitHub refuses a base whose branch already contains the head, so a
        // stacked member moves onto trunk before trunk reaches its head. A
        // refused move does not stop the landing: the member's content
        // reaches trunk whatever its base reads. A batch that does not land
        // puts each moved member back on its base, so it is not reported on
        // trunk while its base is unlanded.
        // TODO(github-unlanded-moves-lost-on-crash)
        let mut moved = Vec::new();
        for pr in stacked {
            match self.api.set_base(pr, &self.settings.trunk) {
                Ok(()) => moved.push(pr),
                Err(ApiError::Permanent(error)) => eprintln!(
                    "dorfq: could not retarget #{} onto {} before landing batch {}: {error}",
                    pr.0, self.settings.trunk, status.id.0
                ),
                Err(error) => {
                    // The move may have applied with its response lost.
                    moved.push(pr);
                    self.move_back(&moved, &bases, status.id);
                    return Err(error.into());
                }
            }
        }
        let trunk_ref = branch(&self.settings.trunk);
        match self.api.update_ref(&trunk_ref, tip, false) {
            Ok(RefUpdate::Updated) => Ok(None),
            Ok(RefUpdate::NotFastForward) => {
                self.move_back(&moved, &bases, status.id);
                Ok(Some(LandRefusal::TrunkMoved {
                    trunk: self.trunk()?,
                }))
            }
            Err(error) => {
                // The update may have applied with its response lost.
                if !moved.is_empty() {
                    match self.api.get_ref(&trunk_ref) {
                        Ok(now) if now.as_ref() == Some(tip) => {}
                        Ok(_) => self.move_back(&moved, &bases, status.id),
                        Err(read) => {
                            for pr in &moved {
                                eprintln!(
                                    "dorfq: could not tell whether batch {} landed, so #{} stays on {}: {read}",
                                    status.id.0, pr.0, self.settings.trunk
                                );
                            }
                        }
                    }
                }
                Err(error.into())
            }
        }
    }

    /// Puts each of `moved` back on its base in `bases`, after its batch did
    /// not land; a move that fails is a line on stderr.
    fn move_back(
        &mut self,
        moved: &[PrNumber],
        bases: &BTreeMap<PrNumber, Option<String>>,
        batch: BatchId,
    ) {
        for &pr in moved {
            let base = bases[&pr]
                .as_deref()
                .expect("a moved member's base matched its live base");
            if let Err(error) = self.api.set_base(pr, base) {
                eprintln!(
                    "dorfq: could not move #{} back onto {base} after batch {} did not land: {error}",
                    pr.0, batch.0
                );
            }
        }
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
}

impl Forge for GitHubForge {
    /// Reports the repository's queue state (rules 1 and 8), after posting
    /// what the outbox holds; once the ledger has accepted it, tells authors
    /// who entered or left the queue since the previous observation (rule 9).
    fn observe(&mut self) -> Result<Observation, ForgeError> {
        self.flush()?;
        let World {
            observation,
            enqueued,
            blocked,
        } = self.fetch_world()?;
        let entered: BTreeSet<PrNumber> = observation
            .prs
            .iter()
            .map(|pr| pr.number)
            .filter(|&pr| !self.ledger.pr_known(pr))
            .collect();
        // Every read that can fail comes before the ledger moves on, so a
        // failed observation is retried against the same latest one.
        let messages = self.transitions(&observation, &entered, &blocked)?;
        self.ledger.observed(&observation)?;
        self.completed.clear();
        self.enqueued = enqueued;
        self.outbox.extend(messages);
        Ok(observation)
    }

    /// Records the batch as `started`, merges its members one by one onto
    /// its build branch, and publishes the batch branch at the tip; on a
    /// conflict, records who conflicted with what and publishes nothing. A
    /// probe's tip is an empty commit on its base.
    fn start_batch(&mut self, batch: &BatchStart) -> Result<(), ForgeError> {
        self.ledger.check_start(batch)?;
        let mut members = Vec::new();
        for &pr in &batch.prs {
            let snapshot = self
                .latest_snapshot(pr)
                .expect("check_start accepts only enqueued members");
            members.push(BatchMember::new(pr, snapshot.head.clone()));
        }
        let id = batch.id;
        let tree = self.tree_of(&batch.base)?;
        let mut record = Record {
            id,
            base: batch.base.clone(),
            members,
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
            self.ledger.started(id);
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
        // TODO(errhandling-write-applied-but-reported-transient-tells-authors-wrong-thing)
        self.write_record(&record, &tree)?;
        self.ledger.started(id);
        for member in &record.members {
            self.outbox.push_back(Message {
                pr: member.pr,
                event: Event::Testing {
                    batch: id,
                    tip: tip.clone(),
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
        self.ledger.landing(batch);
        let tree = self.tree_of(&status.base)?;
        let mut record = Record {
            id: batch,
            base: status.base.clone(),
            members: status.members.clone(),
            state: RecordState::Landing { tip: tip.clone() },
        };
        self.write_record(&record, &tree)?;

        if let Some(reason) = self.fast_forward(&status, &tip)? {
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
    /// first completes the landing for every member still known.
    fn retire(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        self.ledger.check_retire(batch)?;
        let landed = self.ledger.latest().and_then(|o| {
            o.batches
                .iter()
                .find(|b| b.id == batch)
                .and_then(|b| match &b.state {
                    BatchState::Landed { tip } => Some((b.members.clone(), tip.clone())),
                    BatchState::Conflict { .. }
                    | BatchState::Testing { .. }
                    | BatchState::LandRefused { .. } => None,
                })
        });
        if let Some((members, tip)) = landed {
            for member in members {
                if self.ledger.pr_known(member.pr) {
                    self.complete_landing(member.pr, batch, &tip)?;
                }
            }
        }
        // TODO(github-retire-cancels-run): a pending batch's CI run keeps
        // running after its refs are deleted.
        // The record goes first: a record whose batch branch is gone reads
        // back as `Permanent`, a batch branch with no record is never read.
        self.api.delete_ref(&record_ref(batch))?;
        self.api.delete_ref(&branch(&batch_branch(batch)))?;
        self.ledger.retired(batch);
        self.flush()
    }

    /// Removes the intent label and tells the author why.
    fn eject(&mut self, pr: PrNumber, reason: &EjectReason) -> Result<(), ForgeError> {
        self.ledger.check_eject(pr)?;
        let base = self
            .latest_snapshot(pr)
            .expect("check_eject accepts only enqueued pull requests")
            .base;
        // TODO(errhandling-write-applied-but-reported-transient-tells-authors-wrong-thing)
        self.api.remove_label(pr, &self.settings.label)?;
        self.ledger.ejected(pr);
        self.outbox.push_back(Message {
            pr,
            event: Event::Ejected {
                reason: reason.clone(),
                base,
            },
        });
        self.flush()
    }
}

/// The enqueued set over the deduplicated listing, with each member's base:
/// labelled pull requests against trunk or in `landed`, on trunk, then, round
/// by round, labelled pull requests whose base branch is the head branch of
/// one enqueued in an earlier round, based on the lowest-numbered of those. A
/// base always enters in an earlier round than its child, so the result is
/// acyclic and closed under `base`.
fn enqueued_bases(
    listed: &BTreeMap<PrNumber, PrSummary>,
    settings: &Settings,
    landed: &BTreeSet<PrNumber>,
) -> BTreeMap<PrNumber, PrBase> {
    // TODO(security-label-grants-unreviewed-landing-on-any-later-head)
    let labelled: Vec<&PrSummary> = listed
        .values()
        .filter(|s| s.labels.contains(&settings.label))
        .collect();
    let mut bases = BTreeMap::new();
    let mut by_branch: BTreeMap<&str, PrNumber> = BTreeMap::new();
    loop {
        let round: Vec<(&PrSummary, PrBase)> = labelled
            .iter()
            .filter(|s| !bases.contains_key(&s.number))
            .filter_map(|s| {
                let base = if s.base == settings.trunk || landed.contains(&s.number) {
                    PrBase::Trunk
                } else {
                    PrBase::Pr(*by_branch.get(s.base.as_str())?)
                };
                Some((*s, base))
            })
            .collect();
        if round.is_empty() {
            return bases;
        }
        for (summary, base) in round {
            bases.insert(summary.number, base);
            if let Some(head_branch) = &summary.head_branch {
                by_branch
                    .entry(head_branch.as_str())
                    .and_modify(|n| *n = (*n).min(summary.number))
                    .or_insert(summary.number);
            }
        }
    }
}

/// Whether a pull request in `bases` has head branch `branch`: one whose
/// base is `branch` would join the enqueued set.
fn has_enqueued_holder(
    listed: &BTreeMap<PrNumber, PrSummary>,
    bases: &BTreeMap<PrNumber, PrBase>,
    branch: &str,
) -> bool {
    listed
        .values()
        .any(|s| bases.contains_key(&s.number) && s.head_branch.as_deref() == Some(branch))
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

/// What labelled pull request `pr`, whose base branch `base` is neither
/// trunk (`trunk_name`, at `trunk`) nor an enqueued pull request's head, is.
/// `stacked` is what the latest observation says `pr` was queued on.
///
/// - Stacked on one this process landed since: a merged base, with no read.
/// - Otherwise, with an open pull request (other than `pr`, the
///   lowest-numbered) whose head `base` carries: when stacked, a merged
///   base if that one targets trunk with its head in trunk, else blocked on
///   it; when not stacked, blocked on the branch if its head is in trunk,
///   else blocked on it.
/// - With none: when stacked, a merged base if a closed pull request merged
///   into trunk has `base`'s tip as its recorded head; blocked on the branch
///   otherwise.
fn classify(
    api: &mut dyn GitHubApi,
    trunk_name: &str,
    pr: PrNumber,
    base: &str,
    stacked: Stacked,
    listed: &BTreeMap<PrNumber, PrSummary>,
    trunk: &Sha,
) -> Result<Class, ForgeError> {
    if stacked == Stacked::OnLanded {
        return Ok(Class::MergedBase);
    }
    let on_branch = || Class::Blocked(Blocker::Branch(base.to_owned()));
    let holder = listed
        .values()
        .find(|s| s.number != pr && s.head_branch.as_deref() == Some(base));
    if let Some(a) = holder {
        let in_trunk = api.is_ancestor(&a.head, trunk)?;
        return Ok(match stacked {
            Stacked::OnUnlanded if in_trunk && a.base == trunk_name => Class::MergedBase,
            Stacked::No if in_trunk => on_branch(),
            _ => Class::Blocked(Blocker::Pr(a.number)),
        });
    }
    if stacked == Stacked::No {
        return Ok(on_branch());
    }
    let merged = match api.get_ref(&branch(base))? {
        Some(tip) => api.merged_pr_heads(base, trunk_name)?.contains(&tip),
        None => false,
    };
    Ok(if merged {
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
