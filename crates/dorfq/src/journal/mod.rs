//! The tick journal: one JSON object per line, appended as the queue runs,
//! rotated into closed segments, and read back by the offline readers.
//!
//! A segment is one file. It begins with a [`Header`] and holds one
//! [`Line`] per tick; a process appending to an existing segment, and a
//! rotation opening a fresh one, each write a header first. A header's
//! `journal` is the format number and selects how the lines after it, up to
//! the next header, are read.

use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use dorfq_core::{Observation, Timestamp};
use dorfq_scheduler::SchedulerConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::driver::{DriverError, TickReport};
use events::{Event, Fold};

pub mod events;
pub mod report;
pub mod status;
pub mod summary;

/// The format number this build writes.
pub const FORMAT: u64 = 1;

/// The formats this build reads.
const READS: &[u64] = &[1];

/// A segment over this many bytes is closed at the next tick.
const SEGMENT_BYTES: u64 = 64 * 1024 * 1024;

/// The longest first line examined when opening an existing file; a longer
/// one is not a journal header.
const HEADER_BYTES: usize = 1024 * 1024;

/// Why a header was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// A process began writing: a new segment, or one appended to.
    Start,
    /// A rotation opened a fresh segment; the process carries on.
    Rotate,
}

/// The line a process writes when it begins writing to a segment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    /// The format number.
    pub journal: u64,
    /// The writing build's version, informational.
    pub dorfq: String,
    /// The forge's kind, which says what `now` is: wall seconds, or the
    /// mock's tick count.
    pub forge: String,
    pub cause: Cause,
    /// The host clock at process start, Unix seconds.
    pub started: u64,
    /// The host clock when this header was written, Unix seconds.
    pub opened: u64,
    /// The scheduler, as configured, that made the plans that follow: the
    /// `[scheduler]` table's serialisation, kept as written, so a segment
    /// stays readable by a build whose scheduler flags differ.
    pub scheduler: Map<String, Value>,
}

impl Header {
    /// A `start` header for a process that began at `started`.
    pub fn start(forge: &str, scheduler: &SchedulerConfig, started: u64) -> Header {
        Header {
            journal: FORMAT,
            dorfq: crate::version().to_owned(),
            forge: forge.to_owned(),
            cause: Cause::Start,
            started,
            opened: started,
            scheduler: match serde_json::to_value(scheduler) {
                Ok(Value::Object(table)) => table,
                _ => unreachable!("a `kind`-tagged scheduler configuration is an object"),
            },
        }
    }
}

/// One tick's line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// The driver's tick count, restarting at 0 per process.
    pub tick: u64,
    /// The host clock at the end of the tick, Unix seconds.
    pub at: u64,
    pub kind: LineKind,
}

/// What a tick's line holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineKind {
    /// What the tick saw and decided; `failure` when an effect then failed.
    Report {
        report: Box<TickReport>,
        failure: Option<Failure>,
    },
    /// The observation equalled the previous journaled one but for `now`,
    /// and the plan was empty.
    Same { now: Timestamp },
    /// The tick ended before the scheduler produced a plan.
    Error(Failure),
}

/// An error a tick ended with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The error's text.
    pub message: String,
    /// Whether the process exited on it.
    pub fatal: bool,
}

/// A line's shape on the wire: one object whose fields say its kind.
#[derive(Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    bound(serialize = "R: Serialize", deserialize = "R: Deserialize<'de>")
)]
struct Wire<R> {
    tick: u64,
    at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report: Option<R>,
    #[serde(default, skip_serializing_if = "is_false")]
    same: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    now: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fatal: Option<bool>,
}

fn is_false(b: &bool) -> bool {
    !b
}

impl<'a> Wire<&'a TickReport> {
    /// A line borrowing its report: a `report` line when `report` is given,
    /// a `same` line when `now` is, and `failure` as `error` and `fatal`.
    fn borrowing(
        tick: u64,
        at: u64,
        report: Option<&'a TickReport>,
        now: Option<Timestamp>,
        failure: Option<&Failure>,
    ) -> Self {
        Wire {
            tick,
            at,
            report,
            same: now.is_some(),
            now,
            error: failure.map(|f| f.message.clone()),
            fatal: failure.map(|f| f.fatal),
        }
    }
}

impl Serialize for Line {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let (report, now, failure) = match &self.kind {
            LineKind::Report { report, failure } => (Some(&**report), None, failure.as_ref()),
            LineKind::Same { now } => (None, Some(*now), None),
            LineKind::Error(failure) => (None, None, Some(failure)),
        };
        Wire::borrowing(self.tick, self.at, report, now, failure).serialize(s)
    }
}

impl<'de> Deserialize<'de> for Line {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Line, D::Error> {
        use serde::de::Error as _;
        let wire = Wire::<TickReport>::deserialize(d)?;
        let failure = match (wire.error, wire.fatal) {
            (Some(message), Some(fatal)) => Some(Failure { message, fatal }),
            (None, None) => None,
            _ => return Err(D::Error::custom("`error` and `fatal` come together")),
        };
        let kind = match (wire.report, wire.same, wire.now, failure) {
            (Some(report), false, None, failure) => LineKind::Report {
                report: Box::new(report),
                failure,
            },
            (None, true, Some(now), None) => LineKind::Same { now },
            (None, false, None, Some(failure)) => LineKind::Error(failure),
            _ => {
                return Err(D::Error::custom(
                    "not a tick line: expected `report`, `same` with `now`, or `error`",
                ));
            }
        };
        Ok(Line {
            tick: wire.tick,
            at: wire.at,
            kind,
        })
    }
}

/// A journal that could not be opened, written or read.
#[derive(Debug)]
pub enum JournalError {
    /// The file system refused.
    Io { path: PathBuf, error: io::Error },
    /// A line is not what the journal allows there.
    Line {
        path: PathBuf,
        line: usize,
        message: String,
    },
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JournalError::Io { path, error } => write!(f, "journal {}: {error}", path.display()),
            JournalError::Line {
                path,
                line,
                message,
            } => write!(f, "{}:{line}: {message}", path.display()),
        }
    }
}

impl Error for JournalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            JournalError::Io { error, .. } => Some(error),
            JournalError::Line { .. } => None,
        }
    }
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> JournalError + '_ {
    move |error| JournalError::Io {
        path: path.to_owned(),
        error,
    }
}

/// A consumer of the current segment's replay at start, and of each live
/// tick: the tick's observation when its line carried one (`None` for a
/// `same` line, an error without a report, and a header), and the events
/// the fold produced for it.
pub type Sink<'a> = &'a mut dyn FnMut(Option<&Observation>, &[Event]);

// TODO(journal-capture-at-first-sight): each pull request head's changed
// paths, with additions and deletions, recorded once beside the journal.
/// Appends a running process's ticks to the journal at one path, rotating
/// closed segments aside, and folds them into the event stream, appended to
/// the events file when there is one.
pub struct Writer {
    path: PathBuf,
    file: File,
    /// The current segment's length.
    bytes: u64,
    /// The size past which the segment is closed.
    limit: u64,
    /// The current segment's first header's `opened`, which names it once
    /// closed.
    first_opened: u64,
    /// The header this process writes, `cause` and `opened` set per write.
    header: Header,
    /// The observation of the last line written since the last header.
    last: Option<Observation>,
    /// Whether a non-idle `report` was written since the last header.
    busy: bool,
    /// The event stream of every line read at open and written since.
    fold: Fold,
    /// The events file, rotated with the journal.
    events: Option<EventsFile>,
}

/// The file the events of each tick are appended to.
struct EventsFile {
    path: PathBuf,
    file: File,
}

impl EventsFile {
    /// Opens the events file at `path`, creating it, and cuts off an
    /// unterminated last line with a note.
    fn open(path: &Path, notes: &mut Vec<String>) -> Result<EventsFile, JournalError> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)
            .map_err(io_error(path))?;
        let found = survey(&mut file).map_err(io_error(path))?;
        cut_unterminated(&file, path, &found, "events", notes)?;
        Ok(EventsFile {
            path: path.to_owned(),
            file,
        })
    }

    /// Appends one line per event, as `dorfq journal events` prints it.
    fn append(&mut self, events: &[Event]) -> Result<(), JournalError> {
        let text: String = events.iter().map(events::line).collect();
        append(&mut self.file, &self.path, text.as_bytes())
    }
}

/// Cuts `file` at `path` back to its last complete line when `found` has an
/// unterminated one, with a note naming the file as `what`.
fn cut_unterminated(
    file: &File,
    path: &Path,
    found: &Found,
    what: &str,
    notes: &mut Vec<String>,
) -> Result<(), JournalError> {
    if found.len > found.complete {
        file.set_len(found.complete).map_err(io_error(path))?;
        notes.push(format!(
            "{what} {}: cut off unterminated line {}",
            path.display(),
            found.lines + 1
        ));
    }
    Ok(())
}

/// Appends `text` to `file` at `path` and flushes it.
fn append(file: &mut File, path: &Path, text: &[u8]) -> Result<(), JournalError> {
    file.write_all(text).map_err(io_error(path))?;
    file.flush().map_err(io_error(path))
}

/// Renames each file in `paths` to `<path>.<opened>`, or each to that with
/// the first `.1`, `.2`, … suffix free for all of them, so the closed
/// files' names pair; returns the names in `paths`' order.
fn close(paths: &[&Path], opened: u64) -> Result<Vec<PathBuf>, JournalError> {
    let mut suffix = format!(".{opened}");
    let mut n = 0;
    // Creating a name claims it, so no other writer's segment is replaced by
    // the rename onto it. A claim that cannot be had for every file is
    // released and the next suffix tried.
    let names = 'claim: loop {
        let mut names: Vec<PathBuf> = Vec::new();
        for path in paths {
            let name = PathBuf::from(format!("{}{suffix}", path.display()));
            match OpenOptions::new().write(true).create_new(true).open(&name) {
                Ok(_) => names.push(name),
                Err(error) => {
                    for claimed in &names {
                        fs::remove_file(claimed).map_err(io_error(claimed))?;
                    }
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(io_error(&name)(error));
                    }
                    n += 1;
                    suffix = format!(".{opened}.{n}");
                    continue 'claim;
                }
            }
        }
        break names;
    };
    for (path, name) in paths.iter().zip(&names) {
        fs::rename(path, name).map_err(io_error(path))?;
    }
    Ok(names)
}

/// Whether the open files `a` at `a_path` and `b` at `b_path` are one file,
/// under two names or one.
fn same_file(a: &File, a_path: &Path, b: &File, b_path: &Path) -> Result<bool, JournalError> {
    let a = a.metadata().map_err(io_error(a_path))?;
    let b = b.metadata().map_err(io_error(b_path))?;
    Ok((a.dev(), a.ino()) == (b.dev(), b.ino()))
}

/// Opens a fresh, empty file at `path`.
fn create_fresh(path: &Path) -> Result<File, JournalError> {
    OpenOptions::new()
        .append(true)
        .create_new(true)
        .open(path)
        .map_err(io_error(path))
}

/// The state of a file found at the journal's path.
struct Found {
    len: u64,
    /// Complete lines.
    lines: usize,
    /// The offset after the last `\n`.
    complete: u64,
    /// The first line, without its `\n`; `None` when it is longer than
    /// [`HEADER_BYTES`].
    first: Option<Vec<u8>>,
}

fn survey(file: &mut File) -> io::Result<Found> {
    let mut found = Found {
        len: 0,
        lines: 0,
        complete: 0,
        first: Some(Vec::new()),
    };
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            return Ok(found);
        }
        for &byte in &buffer[..n] {
            found.len += 1;
            if byte == b'\n' {
                found.lines += 1;
                found.complete = found.len;
            } else if found.lines == 0
                && let Some(first) = &mut found.first
            {
                if first.len() == HEADER_BYTES {
                    found.first = None;
                } else {
                    first.push(byte);
                }
            }
        }
    }
}

/// A header's two fields every format keeps, read from a line of any
/// format; `None` when the line is not a journal header.
fn header_stamp(line: &[u8]) -> Option<(u64, u64)> {
    #[derive(Deserialize)]
    struct Stamp {
        journal: u64,
        opened: u64,
    }
    let stamp: Stamp = serde_json::from_slice(line).ok()?;
    Some((stamp.journal, stamp.opened))
}

impl Writer {
    /// Opens the journal at `path` for a process starting with `header`
    /// (`cause` `start`): creates the file or appends to it, opens the
    /// events file at `events` when given, replays the current segment
    /// into the fold and `seed` entry by entry, then writes the header.
    /// Returns the writer and what the operator should be told.
    ///
    /// An unterminated last line, which a crash mid-write leaves, is cut
    /// off, in the journal and in the events file. A segment of a format
    /// this build does not write is closed, with the events file, as a
    /// rotation closes them, and both opened fresh. A file whose first line
    /// is not a journal header is refused and left as it is, an events path
    /// naming the journal's own file is refused, and a segment with a line
    /// that does not read is refused naming the line.
    pub fn open(
        path: &Path,
        header: Header,
        events: Option<&Path>,
        seed: Sink<'_>,
    ) -> Result<(Writer, Vec<String>), JournalError> {
        let mut notes = Vec::new();
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)
            .map_err(io_error(path))?;
        let found = survey(&mut file).map_err(io_error(path))?;
        let not_a_journal = || JournalError::Line {
            path: path.to_owned(),
            line: 1,
            message: "not a dorfq journal: the first line is not a journal header".to_owned(),
        };
        let stamp = if found.lines > 0 {
            let first = found.first.as_deref().ok_or_else(not_a_journal)?;
            Some(header_stamp(first).ok_or_else(not_a_journal)?)
        } else {
            None
        };
        // With no complete line, only a fragment of a header is ours.
        if found.len > found.complete
            && found.lines == 0
            && !found
                .first
                .as_deref()
                .is_some_and(|f| f.starts_with(b"{\"journal\":"))
        {
            return Err(not_a_journal());
        }
        cut_unterminated(&file, path, &found, "journal", &mut notes)?;
        let mut writer = Writer {
            path: path.to_owned(),
            file,
            bytes: found.complete,
            limit: SEGMENT_BYTES,
            first_opened: header.opened,
            header,
            last: None,
            busy: false,
            fold: Fold::default(),
            events: None,
        };
        writer.events = events
            .map(|events| EventsFile::open(events, &mut notes))
            .transpose()?;
        if let Some(events) = &writer.events
            && same_file(&writer.file, path, &events.file, &events.path)?
        {
            return Err(JournalError::Io {
                path: events.path.clone(),
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("events names the journal's own file, {}", path.display()),
                ),
            });
        }
        match stamp {
            Some((FORMAT, opened)) => writer.first_opened = opened,
            Some((format, opened)) => {
                writer.first_opened = opened;
                let name = writer.rotate_files()?;
                notes.push(format!(
                    "journal at {} is format {format}, this build writes {FORMAT}: closed it as {}",
                    path.display(),
                    name.display()
                ));
            }
            None => {}
        }
        for entry in self::entries(path)? {
            let entry = entry?;
            writer.fold.entry(&entry);
            let observation = match &entry {
                Entry::Tick(Line {
                    kind: LineKind::Report { report, .. },
                    ..
                }) => Some(&report.observation),
                Entry::Tick(_) | Entry::Header(_) => None,
            };
            seed(observation, &writer.fold.take());
        }
        writer.write_header(Cause::Start, writer.header.opened)?;
        Ok((writer, notes))
    }

    /// Writes tick `tick`'s line, ending at host time `at`, folds it and
    /// appends its events to the events file, then rotates when the tick is
    /// idle after a busy stretch or the segment is full. Returns the tick's
    /// events.
    pub fn record(
        &mut self,
        tick: u64,
        at: u64,
        outcome: &Result<TickReport, DriverError>,
    ) -> Result<Vec<Event>, JournalError> {
        let failure = outcome.as_ref().err().map(|error| Failure {
            message: error.to_string(),
            fatal: error.is_fatal(),
        });
        let report = outcome.as_ref().map_or_else(DriverError::report, Some);
        let idle_after_busy = report.is_some_and(|r| r.idle) && self.busy;
        match (report, &failure) {
            (Some(report), None) if self.last.as_ref().is_some_and(|last| report.repeats(last)) => {
                let now = report.observation.now;
                self.write(&Wire::borrowing(tick, at, None, Some(now), None))?;
                self.fold.same(now, at, tick);
            }
            (Some(report), failure) => {
                self.last = Some(report.observation.clone());
                self.busy |= !report.idle;
                self.write(&Wire::borrowing(
                    tick,
                    at,
                    Some(report),
                    None,
                    failure.as_ref(),
                ))?;
                self.fold
                    .observe(&report.observation, &report.plan, at, tick);
                if let Some(failure) = failure {
                    self.fold.error(failure, at);
                }
            }
            (None, Some(failure)) => {
                self.write(&Wire::borrowing(tick, at, None, None, Some(failure)))?;
                self.fold.error(failure, at);
            }
            (None, None) => unreachable!("an outcome is a report or an error"),
        }
        let events = self.fold.take();
        if let Some(file) = &mut self.events {
            file.append(&events)?;
        }
        // The idle tick ends the closed segment, so the departures it
        // reports stay with the lifetimes they end.
        if idle_after_busy || self.bytes > self.limit {
            self.rotate_files()?;
            self.write_header(Cause::Rotate, at)?;
        }
        Ok(events)
    }

    /// Closes the journal segment and the events file together under the
    /// segment's name, then opens both fresh and empty. Returns the closed
    /// segment's name.
    fn rotate_files(&mut self) -> Result<PathBuf, JournalError> {
        let mut paths = vec![self.path.as_path()];
        if let Some(file) = &self.events {
            paths.push(&file.path);
        }
        let name = close(&paths, self.first_opened)?.remove(0);
        self.reopen()?;
        if let Some(file) = &mut self.events {
            file.file = create_fresh(&file.path)?;
        }
        Ok(name)
    }

    fn write_header(&mut self, cause: Cause, opened: u64) -> Result<(), JournalError> {
        self.header.cause = cause;
        self.header.opened = opened;
        if self.bytes == 0 {
            self.first_opened = opened;
        }
        self.last = None;
        self.busy = false;
        let header = self.header.clone();
        self.write(&header)?;
        self.fold.header(&header);
        Ok(())
    }

    fn write(&mut self, line: &impl Serialize) -> Result<(), JournalError> {
        let mut text = serde_json::to_vec(line).map_err(|e| JournalError::Io {
            path: self.path.clone(),
            error: e.into(),
        })?;
        text.push(b'\n');
        append(&mut self.file, &self.path, &text)?;
        self.bytes += text.len() as u64;
        Ok(())
    }

    /// Opens a fresh, empty segment at the path.
    fn reopen(&mut self) -> Result<(), JournalError> {
        self.file = create_fresh(&self.path)?;
        self.bytes = 0;
        Ok(())
    }
}

/// One parsed line of a journal.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Header(Header),
    Tick(Line),
}

/// The entries of one or more segments, and what the reader should be told.
#[derive(Debug)]
pub struct Read {
    pub entries: Vec<Entry>,
    pub notes: Vec<String>,
}

/// Reads the segments at `paths`, in the order given, into one sequence of
/// entries. Each header's format selects how the lines after it are read.
/// A file's unterminated last line, the live segment's tick in progress, is
/// skipped with a note.
pub fn read(paths: &[PathBuf]) -> Result<Read, JournalError> {
    let mut read = Read {
        entries: Vec::new(),
        notes: Vec::new(),
    };
    for path in paths {
        read_segment(path, &mut read)?;
    }
    Ok(read)
}

/// What a line says before it is read as its kind: a header has `journal`.
#[derive(Deserialize)]
struct Peek {
    journal: Option<Value>,
}

fn read_segment(path: &Path, read: &mut Read) -> Result<(), JournalError> {
    let mut entries = entries(path)?;
    // TODO(efficiency-journal-readers-materialise-every-segment): consume
    // the entries as they are read instead of collecting every segment.
    for entry in &mut entries {
        read.entries.push(entry?);
    }
    read.notes.append(&mut entries.notes);
    Ok(())
}

/// The entries of the segment at `path`, read one line at a time. Each
/// header's format selects how the lines after it are read. An unterminated
/// last line ends the entries with a note; a line that does not read is an
/// error, after which there are no more.
pub fn entries(path: &Path) -> Result<Entries, JournalError> {
    let file = File::open(path).map_err(io_error(path))?;
    Ok(Entries {
        path: path.to_owned(),
        reader: BufReader::new(file),
        format: None,
        number: 0,
        ended: false,
        notes: Vec::new(),
    })
}

/// The entries of one segment, in order; see [`entries`].
pub struct Entries {
    path: PathBuf,
    reader: BufReader<File>,
    /// The format of the last header read.
    format: Option<u64>,
    /// The number of the last line read.
    number: usize,
    ended: bool,
    /// What the reader should be told: the unterminated last line skipped.
    pub notes: Vec<String>,
}

impl Entries {
    fn error(&self, message: String) -> JournalError {
        JournalError::Line {
            path: self.path.clone(),
            line: self.number,
            message,
        }
    }

    /// The next line's entry, `None` at the end.
    fn read(&mut self) -> Result<Option<Entry>, JournalError> {
        let mut line = Vec::new();
        self.reader
            .read_until(b'\n', &mut line)
            .map_err(io_error(&self.path))?;
        if line.is_empty() {
            return Ok(None);
        }
        self.number += 1;
        if line.pop() != Some(b'\n') {
            self.notes.push(format!(
                "{}:{}: skipped an unterminated last line",
                self.path.display(),
                self.number
            ));
            return Ok(None);
        }
        let peek: Peek = serde_json::from_slice(&line).map_err(|e| {
            let what = if e.is_data() {
                "not a journal line"
            } else {
                "not JSON"
            };
            self.error(format!("{what}: {e}"))
        })?;
        if let Some(journal) = peek.journal {
            let journal = journal
                .as_u64()
                .filter(|j| READS.contains(j))
                .ok_or_else(|| {
                    self.error(format!(
                        "journal format {journal} is not one this build reads ({READS:?})"
                    ))
                })?;
            let header = match journal {
                1 => serde_json::from_slice::<Header>(&line),
                _ => unreachable!("READS lists the formats matched here"),
            }
            .map_err(|e| self.error(format!("not a format-{journal} header: {e}")))?;
            self.format = Some(journal);
            Ok(Some(Entry::Header(header)))
        } else {
            let format = self
                .format
                .ok_or_else(|| self.error("the first line is not a header".to_owned()))?;
            let line = match format {
                1 => serde_json::from_slice::<Line>(&line),
                _ => unreachable!("READS lists the formats matched here"),
            }
            .map_err(|e| self.error(format!("not a format-{format} tick line: {e}")))?;
            Ok(Some(Entry::Tick(line)))
        }
    }
}

impl Iterator for Entries {
    type Item = Result<Entry, JournalError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ended {
            return None;
        }
        let next = self.read().transpose();
        self.ended = !matches!(next, Some(Ok(_)));
        next
    }
}

#[cfg(test)]
mod tests;
