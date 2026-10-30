//! The capture file: each pull request head's changed files, read from the
//! forge at the head's sighting, one JSON object per line, keyed by
//! `(pr, head)`.
//!
//! A head is sighted by an `arrive` or `repush` event and is owed a record
//! until one is written — its files, or the permanent failure that kept
//! them from being read — or until a fetch finds it is no longer the live
//! head, after which it is owed one again only if sighted again. The file
//! rotates with the journal, so a head is recorded at most once per capture
//! file; a fresh file holds again every head its segment shows arriving
//! that is the live head at its fetch.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead as _, BufReader};
use std::ops::Bound;
use std::path::{Path, PathBuf};

use dorfq_core::{ForgeError, PrNumber, Sha};
use dorfq_forge_github::HeadFiles;
use serde::{Deserialize, Serialize};

use super::events::{Event, Kind};
use super::{JournalError, append, create_fresh, io_error, open_beside};

/// The format number every record carries.
const FORMAT: u64 = 1;

/// The most heads one [`Capture::fetch`] asks for; the rest stay pending
/// for the next tick.
const HEADS_PER_TICK: usize = 8;

/// Reads a pull request's files at a head: `Ok(Err(now))` when the forge's
/// head is `now`, not the head asked.
pub type Reader<'a> = dyn FnMut(PrNumber, &Sha) -> Result<Result<HeadFiles, Sha>, ForgeError> + 'a;

/// The capture file beside the journal and what this process knows of it.
pub(super) struct Capture {
    path: PathBuf,
    file: File,
    /// Every `(pr, head)` with a record in the current file: read from it at
    /// open, added to as this process writes, emptied when the file is
    /// reopened fresh at a rotation.
    seen: BTreeSet<(u64, Sha)>,
    /// Sighted, not yet recorded: the forge time of the first sighting.
    /// Survives a rotation: a sighting is owed a record whichever file is
    /// open when its fetch lands.
    pending: BTreeMap<(u64, Sha), u64>,
    /// The last head [`Capture::fetch`] asked for; the next call starts
    /// after it.
    last: Option<(u64, Sha)>,
}

/// One line of the capture file.
#[derive(Serialize)]
struct Record<'a> {
    capture: u64,
    pr: u64,
    head: &'a Sha,
    /// The forge time of the first sighting.
    t: u64,
    /// The host time of the fetch.
    at: u64,
    #[serde(flatten)]
    outcome: Outcome<'a>,
}

/// What a record says of the head: its files, or why there are none.
#[derive(Serialize)]
#[serde(untagged)]
enum Outcome<'a> {
    Files {
        base: &'a Sha,
        additions: u64,
        deletions: u64,
        changed_files: u64,
        files: Vec<FileLine<'a>>,
    },
    Error {
        error: String,
    },
}

/// One changed file in a record.
#[derive(Serialize)]
struct FileLine<'a> {
    path: &'a str,
    additions: u64,
    deletions: u64,
    status: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous: Option<&'a str>,
}

/// The two fields of a record of any format that key it.
#[derive(Deserialize)]
struct Key {
    pr: u64,
    head: Sha,
}

impl Capture {
    /// Opens the capture file at `path` as [`open_beside`] does, then reads
    /// every line's key; a line without a `pr` and a `head` is refused
    /// naming the line.
    pub(super) fn open(
        path: &Path,
        others: &[(&str, &Path, &File)],
        notes: &mut Vec<String>,
    ) -> Result<Capture, JournalError> {
        let file = open_beside(path, "capture", others, notes)?;
        let mut seen = BTreeSet::new();
        let mut reader = BufReader::new(File::open(path).map_err(io_error(path))?);
        let mut line = Vec::new();
        let mut number = 0;
        loop {
            line.clear();
            if reader
                .read_until(b'\n', &mut line)
                .map_err(io_error(path))?
                == 0
            {
                break;
            }
            number += 1;
            let key: Key = serde_json::from_slice(&line).map_err(|e| JournalError::Line {
                path: path.to_owned(),
                line: number,
                message: format!("not a capture record: {e}"),
            })?;
            seen.insert((key.pr, key.head));
        }
        Ok(Capture {
            path: path.to_owned(),
            file,
            seen,
            pending: BTreeMap::new(),
            last: None,
        })
    }

    /// The path of the file.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Reopens the file fresh and empty at its path, the old one having
    /// been renamed aside; forgets its keys and keeps what is pending.
    pub(super) fn fresh(&mut self) -> Result<(), JournalError> {
        self.file = create_fresh(&self.path)?;
        self.seen.clear();
        Ok(())
    }

    /// Marks the head of each `arrive` and `repush` in `events` pending
    /// unless the file holds it; a head already pending keeps its first
    /// sighting's time.
    pub(super) fn sight(&mut self, events: &[Event]) {
        for event in events {
            let (pr, head) = match &event.kind {
                Kind::Arrive { pr, head, .. } | Kind::Repush { pr, head } => (*pr, head),
                _ => continue,
            };
            let key = (pr, head.clone());
            if !self.seen.contains(&key) {
                self.pending.entry(key).or_insert(event.t);
            }
        }
    }

    /// Reads up to [`HEADS_PER_TICK`] pending heads through `read`, in key
    /// order from just after the last head the previous call asked for,
    /// wrapping round, so a head that keeps failing never holds back the
    /// heads behind it; records each one's files or its permanent failure
    /// at host time `at`; a head found moved is dropped from `pending`
    /// without a record. A transient failure leaves its head pending and
    /// ends the call. Returns a line for the operator per head not recorded
    /// with its files.
    pub(super) fn fetch(
        &mut self,
        at: u64,
        read: &mut Reader<'_>,
    ) -> Result<Vec<String>, JournalError> {
        let mut lines = Vec::new();
        self.ask(at, read, &mut lines)?;
        Ok(lines)
    }

    /// Fetches as [`Capture::fetch`] does, call after call, until nothing
    /// is pending or a transient failure ends a call; then names each head
    /// still pending as not read. Returns the operator's lines.
    pub(super) fn drain(
        &mut self,
        at: u64,
        read: &mut Reader<'_>,
    ) -> Result<Vec<String>, JournalError> {
        let mut lines = Vec::new();
        while !self.pending.is_empty() && self.ask(at, read, &mut lines)? {}
        for (pr, head) in self.pending.keys() {
            lines.push(format!(
                "capture of #{pr} at {head} not read before the exit"
            ));
        }
        Ok(lines)
    }

    /// One [`Capture::fetch`], its operator lines pushed onto `lines`;
    /// false when a transient failure ended it.
    fn ask(
        &mut self,
        at: u64,
        read: &mut Reader<'_>,
        lines: &mut Vec<String>,
    ) -> Result<bool, JournalError> {
        let keys: Vec<(u64, Sha)> = match &self.last {
            Some(last) => self
                .pending
                .range((Bound::Excluded(last), Bound::Unbounded))
                .chain(self.pending.range(..=last))
                .map(|(key, _)| key.clone())
                .take(HEADS_PER_TICK)
                .collect(),
            None => self.pending.keys().take(HEADS_PER_TICK).cloned().collect(),
        };
        for key in keys {
            self.last = Some(key.clone());
            let (pr, head) = &key;
            match read(PrNumber(*pr), head) {
                Ok(Ok(files)) => self.write(&key, at, files_outcome(&files))?,
                Ok(Err(now)) => {
                    lines.push(format!(
                        "capture of #{pr} at {head} not read: the head moved to {now}"
                    ));
                    // Kept out of `seen`, so a push back to this head is fetched.
                    self.pending.remove(&key);
                    continue;
                }
                Err(ForgeError::Permanent(e)) => {
                    let error = e.to_string();
                    self.write(&key, at, Outcome::Error { error })?;
                    lines.push(format!(
                        "capture of #{pr} at {head} recorded as failed: {e}"
                    ));
                }
                Err(ForgeError::Transient(e)) => {
                    lines.push(format!("capture of #{pr} at {head}: {e}; retrying"));
                    return Ok(false);
                }
            }
            self.pending.remove(&key);
            self.seen.insert(key);
        }
        Ok(true)
    }

    /// Appends the record of pending `key`, fetched at `at`.
    fn write(
        &mut self,
        key: &(u64, Sha),
        at: u64,
        outcome: Outcome<'_>,
    ) -> Result<(), JournalError> {
        let record = Record {
            capture: FORMAT,
            pr: key.0,
            head: &key.1,
            t: self.pending[key],
            at,
            outcome,
        };
        let text = serde_json::to_string(&record).expect("a capture record serialises") + "\n";
        append(&mut self.file, &self.path, text.as_bytes())
    }
}

/// The record of a head whose files were read.
fn files_outcome(files: &HeadFiles) -> Outcome<'_> {
    Outcome::Files {
        base: &files.base,
        additions: files.additions,
        deletions: files.deletions,
        changed_files: files.changed_files,
        files: files
            .files
            .iter()
            .map(|file| FileLine {
                path: &file.path,
                additions: file.additions,
                deletions: file.deletions,
                status: &file.status,
                previous: file.previous.as_deref(),
            })
            .collect(),
    }
}

#[cfg(test)]
pub(super) mod tests;
