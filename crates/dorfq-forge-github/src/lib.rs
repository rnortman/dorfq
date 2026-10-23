//! DorfQ's forge for GitHub, over the REST API.
//!
//! The forge is an HTTP client and runs no git: GitHub's merge endpoint
//! builds a batch, a fast-forward-only ref update lands it, and every piece
//! of queue state lives in the repository itself, in the layout below.
//!
//! | Name | What it is |
//! |---|---|
//! | the configured label | intent: an open pull request against trunk carrying it is enqueued |
//! | `refs/heads/dorfq/batch/<id>` | a tracked batch's tip; exists from the end of a successful build until `retire`; CI must run on pushes to it |
//! | `refs/heads/dorfq/build/<id>` | the branch a batch's merges are made on; exists only during `start_batch` |
//! | `refs/heads/dorfq/probe/<id>` | the branch conflict attribution uses; exists only during `start_batch` |
//! | `refs/dorfq/batches/<id>` | the batch's record, a commit whose message is the [`record`] text |
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
//! - [`checks`]: required check names on a commit, reduced to a CI status.
//! - [`messages`]: what authors are told, as marked issue comments.
//! - [`record`]: the batch record's text.

pub mod api;
pub mod checks;
pub mod messages;
pub mod record;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write;

use dorfq_core::ledger::Ledger;
use dorfq_core::{
    BatchId, BatchMember, BatchStart, BatchState, BatchStatus, CiStatus, ConflictWith, EjectReason,
    Forge, ForgeError, LandRefusal, Mergeable, Observation, PrNumber, PrSnapshot, Sha,
};

use api::{ApiError, GitHubApi, Merge, RefEntry, RefUpdate};
use messages::{Event, Message};
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
    ledger: Ledger,
    /// Enqueued at the latest observation and not landed or ejected since.
    known: BTreeSet<PrNumber>,
    /// The titles of the pull requests enqueued at the latest observation,
    /// for merge commit messages.
    titles: BTreeMap<PrNumber, String>,
    /// Comments not yet posted, in order.
    outbox: VecDeque<Message>,
    /// One line per message posted.
    sink: Box<dyn Write>,
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

impl GitHubForge {
    /// A forge over the repository `api` reaches. Checks that trunk and the
    /// label exist; writes nothing.
    pub fn new(
        mut api: Box<dyn GitHubApi>,
        settings: Settings,
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
            ledger: Ledger::new(),
            known: BTreeSet::new(),
            titles: BTreeMap::new(),
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

    /// The observation, and the titles of the pull requests it reports.
    fn fetch_world(&mut self) -> Result<(Observation, BTreeMap<PrNumber, String>), ForgeError> {
        let trunk = self.trunk()?;

        let mut prs = Vec::new();
        let mut titles = BTreeMap::new();
        for summary in self.api.list_open_prs(&self.settings.trunk)? {
            // TODO(security-label-grants-unreviewed-landing-on-any-later-head)
            if !summary.labels.contains(&self.settings.label) {
                continue;
            }
            let detail = self.api.get_pr(summary.number)?;
            let mergeable = match detail.mergeable {
                Some(true) => Mergeable::Clean,
                Some(false) => Mergeable::Conflicting,
                None => Mergeable::Unknown,
            };
            let head_ci = self.ci(&summary.head)?;
            titles.insert(summary.number, summary.title);
            prs.push(PrSnapshot {
                number: summary.number,
                head: summary.head,
                mergeable,
                head_ci,
            });
        }

        let mut batches = Vec::new();
        for entry in self.api.list_refs(RECORDS)? {
            if let Some(status) = self.recover(&entry, &trunk)? {
                batches.push(status);
            }
        }

        prs.sort_by_key(|pr| pr.number);
        batches.sort_by_key(|batch| batch.id);
        let observation = Observation {
            trunk,
            prs,
            batches,
        };
        Ok((observation, titles))
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
        Ok(Some(BatchStatus {
            id,
            base: record.base,
            members: record.members,
            state,
        }))
    }

    fn ci(&mut self, sha: &Sha) -> Result<CiStatus, ForgeError> {
        let results = self.api.checks(sha)?;
        Ok(checks::evaluate(&self.settings.checks, &results))
    }

    /// The rule-9 messages for the move from `known` to `observation`: a
    /// pull request that left is told it landed when its batch is reported
    /// `Landed`, and that it withdrew otherwise; one that entered is greeted
    /// unless its comment thread shows it was greeted already.
    fn transitions(&mut self, observation: &Observation) -> Result<Vec<Message>, ForgeError> {
        let enqueued: BTreeSet<PrNumber> = observation.prs.iter().map(|pr| pr.number).collect();
        let mut messages = Vec::new();
        for &pr in self.known.difference(&enqueued) {
            let landed = observation
                .batches
                .iter()
                .find_map(|batch| match &batch.state {
                    BatchState::Landed { tip } if batch.members.iter().any(|m| m.pr == pr) => {
                        Some(Event::Landed {
                            batch: batch.id,
                            tip: tip.clone(),
                        })
                    }
                    _ => None,
                });
            messages.push(Message {
                pr,
                event: landed.unwrap_or(Event::Withdrawn),
            });
        }
        for &pr in enqueued.difference(&self.known) {
            let thread = self.api.list_comments(pr)?;
            if messages::needs_greeting(&thread) {
                messages.push(Message {
                    pr,
                    event: Event::Queued,
                });
            }
        }
        Ok(messages)
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
        let sha =
            self.api
                .create_commit(&record.render(), tree, std::slice::from_ref(&record.base))?;
        self.force_ref(&record_ref(record.id), &sha)
    }

    /// The merge commit message for `pr` in batch `id`.
    fn merge_message(&self, id: BatchId, pr: PrNumber) -> String {
        let title = self
            .titles
            .get(&pr)
            .expect("a batch member is enqueued at the latest observation");
        format!("Merge #{}: {title}\n\nDorfq-Batch: {}", pr.0, id.0)
    }

    /// What member `k` of a batch conflicted with when merged onto the
    /// earlier members: trunk when it is the first member or was observed
    /// conflicting; otherwise whatever the probe branch reproduces.
    fn attribute(
        &mut self,
        id: BatchId,
        base: &Sha,
        members: &[BatchMember],
        k: usize,
        observed_conflicting: bool,
    ) -> Result<ConflictWith, ForgeError> {
        if k == 0 || observed_conflicting {
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

    /// Rule 2 against the world as it is now, then the fast-forward; the
    /// refusal, or `None` when trunk is now `tip`.
    fn fast_forward(
        &mut self,
        status: &BatchStatus,
        tip: &Sha,
    ) -> Result<Option<LandRefusal>, ForgeError> {
        let trunk = self.trunk()?;
        if trunk != status.base {
            return Ok(Some(LandRefusal::TrunkMoved { trunk }));
        }
        for member in &status.members {
            let pr = self.api.get_pr(member.pr)?;
            let enqueued = pr.open
                && pr.base == self.settings.trunk
                && pr.labels.contains(&self.settings.label)
                && pr.head == member.head;
            if !enqueued {
                return Ok(Some(LandRefusal::MemberGone { pr: member.pr }));
            }
        }
        let trunk_ref = branch(&self.settings.trunk);
        match self.api.update_ref(&trunk_ref, tip, false)? {
            RefUpdate::Updated => Ok(None),
            RefUpdate::NotFastForward => Ok(Some(LandRefusal::TrunkMoved {
                trunk: self.trunk()?,
            })),
        }
    }

    fn trunk(&mut self) -> Result<Sha, ForgeError> {
        let trunk_ref = branch(&self.settings.trunk);
        self.api
            .get_ref(&trunk_ref)?
            .ok_or_else(|| permanent(format!("trunk branch {trunk_ref} does not exist")))
    }

    /// A landed member leaves the queue: its label is removed, it is no
    /// longer known, and it is told it landed.
    fn complete_landing(
        &mut self,
        pr: PrNumber,
        batch: BatchId,
        tip: &Sha,
    ) -> Result<(), ForgeError> {
        self.api.remove_label(pr, &self.settings.label)?;
        self.known.remove(&pr);
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
    /// what the outbox holds; then tells authors who entered or left the
    /// queue since the previous observation (rule 9).
    fn observe(&mut self) -> Result<Observation, ForgeError> {
        self.flush()?;
        let (observation, titles) = self.fetch_world()?;
        let messages = self.transitions(&observation)?;
        self.ledger.observed(&observation);
        self.known = observation.prs.iter().map(|pr| pr.number).collect();
        self.titles = titles;
        self.outbox.extend(messages);
        Ok(observation)
    }

    /// Records the batch as `started`, merges its members one by one onto
    /// its build branch, and publishes the batch branch at the tip; on a
    /// conflict, records who conflicted with what and publishes nothing.
    fn start_batch(&mut self, batch: &BatchStart) -> Result<(), ForgeError> {
        self.ledger.check_start(batch)?;
        let latest = self
            .ledger
            .latest()
            .expect("check_start accepts nothing before an observation");
        let mut members = Vec::new();
        let mut conflicting = Vec::new();
        for &pr in &batch.prs {
            let snapshot = latest
                .prs
                .iter()
                .find(|p| p.number == pr)
                .expect("check_start accepts only enqueued members");
            members.push(BatchMember {
                pr,
                head: snapshot.head.clone(),
            });
            conflicting.push(snapshot.mergeable == Mergeable::Conflicting);
        }
        let id = batch.id;
        let tree = self.tree_of(&batch.base)?;
        let mut record = Record {
            id,
            base: batch.base.clone(),
            members,
            state: RecordState::Started,
        };
        let started =
            self.api
                .create_commit(&record.render(), &tree, std::slice::from_ref(&batch.base))?;
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
            let with = self.attribute(id, &batch.base, &record.members, k, conflicting[k])?;
            self.api.delete_ref(&build_ref)?;
            record.state = RecordState::Conflict {
                pr: record.members[k].pr,
                with,
            };
            self.write_record(&record, &tree)?;
            self.ledger.started(id);
            return self.flush();
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
    /// and fast-forwards trunk to the tip; a refusal is recorded and
    /// returns `Ok`. After the fast-forward each member's label is removed
    /// and it is told it landed.
    fn land(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        let status = self.ledger.check_land(batch)?.clone();
        let BatchState::Testing { tip, .. } = status.state.clone() else {
            unreachable!("check_land returns a batch reported as testing");
        };
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
                    _ => None,
                })
        });
        if let Some((members, tip)) = landed {
            for member in members {
                if self.known.contains(&member.pr) {
                    self.complete_landing(member.pr, batch, &tip)?;
                }
            }
        }
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
        // TODO(errhandling-write-applied-but-reported-transient-tells-authors-wrong-thing)
        self.api.remove_label(pr, &self.settings.label)?;
        self.ledger.ejected(pr);
        self.known.remove(&pr);
        self.outbox.push_back(Message {
            pr,
            event: Event::Ejected {
                reason: reason.clone(),
            },
        });
        self.flush()
    }
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
