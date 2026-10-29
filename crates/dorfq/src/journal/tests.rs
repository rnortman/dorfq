use std::fs;
use std::path::{Path, PathBuf};

use dorfq_core::{
    BatchId, BatchStart, Departed, Departure, EjectReason, Ejection, ForgeError, Observation, Plan,
    PrBase, PrNumber, PrSnapshot, PrStatus, SchedulerError, Sha, Timestamp,
};
use dorfq_scheduler::SchedulerConfig;
use serde_json::Value;

use super::*;
use events::Event;

/// A fresh directory for one test.
fn dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(std::env::var("TEST_TMPDIR").expect("TEST_TMPDIR under bazel test"))
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn sha(n: u64) -> Sha {
    Sha::parse(&format!("{n:040x}")).unwrap()
}

fn observation(now: u64, prs: &[u64]) -> Observation {
    let prs = prs
        .iter()
        .map(|&n| PrSnapshot::new(PrNumber(n), sha(n), PrBase::Trunk, PrStatus::Ready, 0))
        .collect();
    Observation::new(Timestamp(now), sha(100), 0, prs, Vec::new(), Vec::new())
}

fn report(tick: u64, observation: Observation, plan: Plan) -> TickReport {
    let idle = observation.prs.is_empty() && observation.batches.is_empty() && plan.is_empty();
    TickReport {
        tick,
        observation,
        plan,
        idle,
    }
}

fn busy(tick: u64, now: u64) -> Result<TickReport, DriverError> {
    Ok(report(tick, observation(now, &[1]), Plan::default()))
}

fn idle(tick: u64, now: u64) -> Result<TickReport, DriverError> {
    Ok(report(tick, observation(now, &[]), Plan::default()))
}

fn start_plan() -> Plan {
    Plan {
        start: vec![BatchStart::new(BatchId(1), sha(100), vec![PrNumber(1)])],
        ..Plan::default()
    }
}

fn header(started: u64) -> Header {
    Header::start("mock", &SchedulerConfig::AllAtOnce {}, started)
}

fn open(path: &Path, started: u64) -> (Writer, Vec<String>) {
    Writer::open(path, header(started), None, &mut |_, _| {}).unwrap()
}

/// Every line of the file at `path`, as JSON.
fn lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn a_fresh_path_gets_a_start_header_then_one_line_per_tick() {
    let path = dir("fresh").join("q.journal");
    let (mut writer, notes) = open(&path, 1000);
    assert!(notes.is_empty(), "{notes:?}");
    writer.record(0, 1001, &busy(0, 5)).unwrap();
    let lines = lines(&path);
    assert_eq!(lines.len(), 2);
    let header = &lines[0];
    assert_eq!(header["journal"], 1);
    assert_eq!(header["dorfq"], crate::version());
    assert_eq!(header["forge"], "mock");
    assert_eq!(header["cause"], "start");
    assert_eq!(header["started"], 1000);
    assert_eq!(header["opened"], 1000);
    assert_eq!(header["scheduler"]["kind"], "all-at-once");
    assert_eq!(lines[1]["tick"], 0);
    assert_eq!(lines[1]["at"], 1001);
    assert_eq!(lines[1]["report"]["observation"]["now"], 5);
    assert!(lines[1].get("error").is_none());
}

#[test]
fn an_unchanged_tick_with_an_empty_plan_is_same_and_any_change_is_a_report() {
    let path = dir("same").join("q.journal");
    let (mut writer, _) = open(&path, 1000);
    writer.record(0, 1001, &busy(0, 5)).unwrap();
    writer.record(1, 1002, &busy(1, 6)).unwrap();
    writer
        .record(2, 1003, &Ok(report(2, observation(7, &[1]), start_plan())))
        .unwrap();
    writer
        .record(
            3,
            1004,
            &Ok(report(3, observation(8, &[1, 2]), Plan::default())),
        )
        .unwrap();
    let lines = lines(&path);
    assert_eq!(
        lines[2],
        serde_json::json!({"tick": 1, "at": 1002, "same": true, "now": 6})
    );
    assert_eq!(lines[3]["report"]["plan"]["start"][0]["id"], 1);
    assert_eq!(lines[4]["report"]["observation"]["prs"][1]["number"], 2);
}

#[test]
fn errors_are_journaled_with_the_report_when_the_scheduler_had_stepped() {
    let path = dir("errors").join("q.journal");
    let (mut writer, _) = open(&path, 1000);
    let effect = Err(DriverError::Effect {
        report: Box::new(report(0, observation(5, &[1]), start_plan())),
        error: ForgeError::Transient("eject flaked".into()),
    });
    writer.record(0, 1001, &effect).unwrap();
    let observe = Err(DriverError::Observe(ForgeError::Transient(
        "rate limited".into(),
    )));
    writer.record(1, 1002, &observe).unwrap();
    let stumped = Err(DriverError::Scheduler(SchedulerError::new(
        "posterior underflow",
    )));
    writer.record(2, 1003, &stumped).unwrap();
    let lines = lines(&path);
    assert_eq!(lines[1]["report"]["plan"]["start"][0]["id"], 1);
    assert_eq!(lines[1]["fatal"], false);
    assert!(lines[1]["error"].as_str().unwrap().contains("eject flaked"));
    assert_eq!(lines[2]["fatal"], false);
    assert!(lines[2]["error"].as_str().unwrap().contains("rate limited"));
    assert!(lines[2].get("report").is_none());
    assert_eq!(lines[3]["fatal"], true);
    assert!(
        lines[3]["error"]
            .as_str()
            .unwrap()
            .contains("posterior underflow")
    );
}

#[test]
fn an_idle_tick_after_a_busy_report_rotates_and_a_quiet_stretch_does_not() {
    let dir = dir("rotate-idle");
    let path = dir.join("q.journal");
    let (mut writer, _) = open(&path, 1000);
    writer.record(0, 1001, &idle(0, 5)).unwrap();
    writer.record(1, 1002, &idle(1, 6)).unwrap();
    assert_eq!(names(&dir), ["q.journal"]);
    writer.record(2, 1003, &busy(2, 7)).unwrap();
    writer.record(3, 1050, &idle(3, 8)).unwrap();
    writer.record(4, 1051, &idle(4, 9)).unwrap();
    assert_eq!(names(&dir), ["q.journal", "q.journal.1000"]);
    let closed = lines(&dir.join("q.journal.1000"));
    assert_eq!(closed.len(), 5);
    assert_eq!(closed[4]["tick"], 3);
    assert!(closed[4]["report"]["idle"].as_bool().unwrap());
    let current = lines(&path);
    assert_eq!(current[0]["cause"], "rotate");
    assert_eq!(current[0]["started"], 1000);
    assert_eq!(current[0]["opened"], 1050);
    // The first tick after a header is a report, though it repeats.
    assert_eq!(current[1]["tick"], 4);
    assert!(current[1]["report"]["idle"].as_bool().unwrap());
}

#[test]
fn the_idle_tick_reporting_the_last_departure_ends_the_closed_segment() {
    let dir = dir("rotate-departure");
    let path = dir.join("q.journal");
    let (mut writer, _) = open(&path, 1000);
    let eject = Plan {
        eject: vec![Ejection::new(
            PrNumber(1),
            EjectReason::BatchFailed { batch: BatchId(1) },
        )],
        ..Plan::default()
    };
    writer
        .record(0, 1001, &Ok(report(0, observation(5, &[1]), eject)))
        .unwrap();
    let mut gone = observation(6, &[]);
    gone.departed = vec![Departure::new(PrNumber(1), sha(1), Departed::Ejected)];
    writer
        .record(1, 1002, &Ok(report(1, gone, Plan::default())))
        .unwrap();
    assert_eq!(names(&dir), ["q.journal", "q.journal.1000"]);
    let closed = lines(&dir.join("q.journal.1000"));
    assert_eq!(closed.len(), 3);
    assert_eq!(
        closed[2]["report"]["observation"]["departed"][0]["how"],
        "ejected"
    );
    assert_eq!(lines(&path).len(), 1);
}

#[test]
fn a_segment_over_the_size_bound_rotates() {
    let dir = dir("rotate-size");
    let path = dir.join("q.journal");
    let (mut writer, _) = open(&path, 1000);
    writer.record(0, 1001, &busy(0, 5)).unwrap();
    writer.limit = 1;
    writer.record(1, 1002, &busy(1, 6)).unwrap();
    assert_eq!(names(&dir), ["q.journal", "q.journal.1000"]);
    assert_eq!(lines(&dir.join("q.journal.1000")).len(), 3);
    writer.limit = SEGMENT_BYTES;
    writer.record(2, 1003, &busy(2, 7)).unwrap();
    let current = lines(&path);
    assert_eq!(current[0]["cause"], "rotate");
    assert_eq!(current[0]["opened"], 1002);
    // The first tick after a header is a report, though it repeats.
    assert_eq!(current[1]["report"]["observation"]["now"], 7);
}

#[test]
fn rotations_sharing_a_second_never_overwrite_a_closed_segment() {
    let dir = dir("no-clobber");
    let path = dir.join("q.journal");
    let (mut writer, _) = open(&path, 1000);
    for tick in 0..3 {
        writer.record(2 * tick, 1000, &busy(2 * tick, 0)).unwrap();
        writer
            .record(2 * tick + 1, 1000, &idle(2 * tick + 1, 0))
            .unwrap();
    }
    assert_eq!(
        names(&dir),
        [
            "q.journal",
            "q.journal.1000",
            "q.journal.1000.1",
            "q.journal.1000.2"
        ]
    );
    for closed in ["q.journal.1000", "q.journal.1000.1", "q.journal.1000.2"] {
        assert!(!lines(&dir.join(closed)).is_empty(), "{closed}");
    }
}

#[test]
fn reopening_a_segment_appends_a_start_header_and_reports_the_first_tick() {
    let dir = dir("reopen");
    let path = dir.join("q.journal");
    let (mut writer, _) = open(&path, 1000);
    writer.record(0, 1001, &busy(0, 5)).unwrap();
    drop(writer);
    let (mut writer, notes) = open(&path, 2000);
    assert!(notes.is_empty(), "{notes:?}");
    writer.record(0, 2001, &busy(0, 6)).unwrap();
    let lines = lines(&path);
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[2]["cause"], "start");
    assert_eq!(lines[2]["started"], 2000);
    assert_eq!(lines[3]["report"]["observation"]["now"], 6);
    // The segment keeps its first header's name when it closes.
    writer.record(1, 2002, &idle(1, 7)).unwrap();
    assert_eq!(names(&dir), ["q.journal", "q.journal.1000"]);
}

#[test]
fn an_empty_file_gets_a_header() {
    let path = dir("empty").join("q.journal");
    fs::write(&path, "").unwrap();
    let (_writer, notes) = open(&path, 1000);
    assert!(notes.is_empty(), "{notes:?}");
    let lines = lines(&path);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["cause"], "start");
}

#[test]
fn an_unterminated_last_line_is_cut_off_with_a_note() {
    let path = dir("cut").join("q.journal");
    let (mut writer, _) = open(&path, 1000);
    writer.record(0, 1001, &busy(0, 5)).unwrap();
    drop(writer);
    let mut text = fs::read(&path).unwrap();
    text.extend_from_slice(b"{\"tick\":1,\"at\":10");
    fs::write(&path, &text).unwrap();
    let (_writer, notes) = open(&path, 2000);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("q.journal"), "{}", notes[0]);
    assert!(notes[0].contains("line 3"), "{}", notes[0]);
    let lines = lines(&path);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[2]["started"], 2000);
}

#[test]
fn a_half_written_header_leaves_an_empty_file_and_a_fresh_header() {
    let path = dir("half-header").join("q.journal");
    fs::write(&path, "{\"journal\":1,\"dorfq\":\"0.1").unwrap();
    let (_writer, notes) = open(&path, 1000);
    assert_eq!(notes.len(), 1, "{notes:?}");
    let lines = lines(&path);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["started"], 1000);
}

#[test]
fn a_segment_of_another_format_is_closed_and_a_fresh_one_opened() {
    let dir = dir("other-format");
    let path = dir.join("q.journal");
    fs::write(&path, "{\"journal\":2,\"opened\":900,\"shape\":\"new\"}\n").unwrap();
    let (_writer, notes) = open(&path, 1000);
    assert_eq!(names(&dir), ["q.journal", "q.journal.900"]);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].contains("is format 2, this build writes 1"),
        "{}",
        notes[0]
    );
    assert!(notes[0].contains("q.journal.900"), "{}", notes[0]);
    assert_eq!(
        fs::read_to_string(dir.join("q.journal.900")).unwrap(),
        "{\"journal\":2,\"opened\":900,\"shape\":\"new\"}\n"
    );
    let lines = lines(&path);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["journal"], 1);
}

#[test]
fn a_file_that_is_not_a_journal_is_refused_and_left_unchanged() {
    for (name, text) in [
        ("not-json", "hello\nworld\n"),
        ("other-json", "{\"opened\":1}\n"),
        ("fragment", "hello"),
    ] {
        let dir = dir(&format!("foreign-{name}"));
        let path = dir.join("q.journal");
        fs::write(&path, text).unwrap();
        let Err(error) = Writer::open(&path, header(1000), None, &mut |_, _| {}) else {
            panic!("{name}: opened");
        };
        let message = error.to_string();
        assert!(message.contains("q.journal:1:"), "{name}: {message}");
        assert_eq!(fs::read_to_string(&path).unwrap(), text, "{name}");
        assert_eq!(names(&dir), ["q.journal"], "{name}");
    }
}

/// A header line padded with spaces to `len` bytes.
fn padded_header(len: usize) -> String {
    let line = header_line();
    let padding = " ".repeat(len - line.len());
    line + &padding
}

#[test]
fn a_first_line_of_the_header_cap_is_read_and_a_longer_one_is_refused_unchanged() {
    let path = dir("header-cap").join("q.journal");
    fs::write(&path, padded_header(HEADER_BYTES) + "\n").unwrap();
    let (_writer, notes) = open(&path, 2000);
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(lines(&path).len(), 2);
    for (name, text) in [
        ("over", padded_header(HEADER_BYTES + 1) + "\n"),
        ("over-unterminated", padded_header(HEADER_BYTES + 1)),
    ] {
        let dir = dir(&format!("header-cap-{name}"));
        let path = dir.join("q.journal");
        fs::write(&path, &text).unwrap();
        let Err(error) = Writer::open(&path, header(2000), None, &mut |_, _| {}) else {
            panic!("{name}: opened");
        };
        let message = error.to_string();
        assert!(message.contains("q.journal:1:"), "{name}: {message}");
        assert!(fs::read_to_string(&path).unwrap() == text, "{name}");
        assert_eq!(names(&dir), ["q.journal"], "{name}");
    }
}

/// Writes a segment holding two processes' ticks, the second process's
/// last line unterminated.
fn two_process_segment(path: &Path) {
    let (mut writer, _) = open(path, 1000);
    writer.record(0, 1001, &busy(0, 5)).unwrap();
    writer.record(1, 1002, &busy(1, 6)).unwrap();
    drop(writer);
    let (mut writer, _) = open(path, 2000);
    writer
        .record(
            0,
            2001,
            &Err(DriverError::Observe(ForgeError::Transient("x".into()))),
        )
        .unwrap();
}

#[test]
fn the_reader_parses_every_process_in_a_segment() {
    let path = dir("read-two").join("q.journal");
    two_process_segment(&path);
    let read = read(std::slice::from_ref(&path)).unwrap();
    assert!(read.notes.is_empty(), "{:?}", read.notes);
    let kinds: Vec<&str> = read
        .entries
        .iter()
        .map(|entry| match entry {
            Entry::Header(_) => "header",
            Entry::Tick(Line {
                kind: LineKind::Report { .. },
                ..
            }) => "report",
            Entry::Tick(Line {
                kind: LineKind::Same { .. },
                ..
            }) => "same",
            Entry::Tick(Line {
                kind: LineKind::Error(_),
                ..
            }) => "error",
        })
        .collect();
    assert_eq!(kinds, ["header", "report", "same", "header", "error"]);
    let Entry::Header(second) = &read.entries[3] else {
        unreachable!()
    };
    assert_eq!(second, &header(2000));
    let Entry::Tick(line) = &read.entries[1] else {
        unreachable!()
    };
    assert_eq!(
        line.kind,
        LineKind::Report {
            report: Box::new(busy(0, 5).unwrap()),
            failure: None
        }
    );
}

#[test]
fn every_line_kind_round_trips() {
    for kind in [
        LineKind::Report {
            report: Box::new(report(3, observation(5, &[1]), start_plan())),
            failure: Some(Failure {
                message: "eject flaked".to_owned(),
                fatal: false,
            }),
        },
        LineKind::Report {
            report: Box::new(busy(3, 5).unwrap()),
            failure: None,
        },
        LineKind::Same { now: Timestamp(9) },
        LineKind::Error(Failure {
            message: "gone".to_owned(),
            fatal: true,
        }),
    ] {
        let line = Line {
            tick: 3,
            at: 1000,
            kind,
        };
        let text = serde_json::to_string(&line).unwrap();
        assert_eq!(serde_json::from_str::<Line>(&text).unwrap(), line, "{text}");
    }
}

fn read_error(name: &str, text: &str) -> String {
    let path = dir(name).join("q.journal");
    fs::write(&path, text).unwrap();
    read(&[path]).unwrap_err().to_string()
}

/// A valid format-1 header line.
fn header_line() -> String {
    serde_json::to_string(&header(1000)).unwrap()
}

#[test]
fn a_malformed_middle_line_names_the_path_and_line() {
    let text = format!(
        "{}\nnot json\n{{\"tick\":0,\"at\":1,\"error\":\"x\",\"fatal\":false}}\n",
        header_line()
    );
    let message = read_error("read-malformed", &text);
    assert!(message.contains("q.journal:2: not JSON"), "{message}");
    let text = format!("{}\n3\n", header_line());
    let message = read_error("read-not-an-object", &text);
    assert!(
        message.contains("q.journal:2: not a journal line"),
        "{message}"
    );
}

#[test]
fn a_format_1_line_that_does_not_deserialise_names_the_path_and_line() {
    for (name, line) in [
        (
            "both",
            "{\"tick\":0,\"at\":1,\"same\":true,\"now\":3,\"error\":\"x\",\"fatal\":false}",
        ),
        ("half-error", "{\"tick\":0,\"at\":1,\"error\":\"x\"}"),
        ("same-without-now", "{\"tick\":0,\"at\":1,\"same\":true}"),
        (
            "unknown-field",
            "{\"tick\":0,\"at\":1,\"error\":\"x\",\"fatal\":false,\"extra\":1}",
        ),
        (
            "bad-report",
            "{\"tick\":0,\"at\":1,\"report\":{\"tick\":0}}",
        ),
    ] {
        let message = read_error(
            &format!("read-shape-{name}"),
            &format!("{}\n{line}\n", header_line()),
        );
        assert!(
            message.contains("q.journal:2: not a format-1 tick line"),
            "{name}: {message}"
        );
    }
}

#[test]
fn a_header_of_a_format_this_build_cannot_read_is_refused_by_name() {
    let message = read_error("read-format-2", "{\"journal\":2,\"opened\":900}\n");
    assert!(
        message.contains("q.journal:1: journal format 2 is not one this build reads ([1])"),
        "{message}"
    );
}

#[test]
fn a_first_line_that_is_not_a_header_is_refused() {
    let message = read_error(
        "read-headless",
        "{\"tick\":0,\"at\":1,\"error\":\"x\",\"fatal\":false}\n",
    );
    assert!(
        message.contains("q.journal:1: the first line is not a header"),
        "{message}"
    );
}

#[test]
fn a_trailing_partial_line_is_skipped_with_a_note() {
    let path = dir("read-partial").join("q.journal");
    two_process_segment(&path);
    let mut text = fs::read(&path).unwrap();
    text.extend_from_slice(b"{\"tick\":1,");
    fs::write(&path, &text).unwrap();
    let read = read(&[path]).unwrap();
    assert_eq!(read.entries.len(), 5);
    assert_eq!(read.notes.len(), 1, "{:?}", read.notes);
    assert!(read.notes[0].contains("q.journal:6:"), "{}", read.notes[0]);
}

#[test]
fn segments_given_in_order_read_as_one_sequence() {
    let dir = dir("read-segments");
    let path = dir.join("q.journal");
    let (mut writer, _) = open(&path, 1000);
    writer.record(0, 1001, &busy(0, 5)).unwrap();
    writer.record(1, 1002, &idle(1, 6)).unwrap();
    writer.record(2, 1003, &busy(2, 7)).unwrap();
    let read = read(&[dir.join("q.journal.1000"), path]).unwrap();
    let ticks: Vec<u64> = read
        .entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Tick(line) => Some(line.tick),
            Entry::Header(_) => None,
        })
        .collect();
    assert_eq!(ticks, [0, 1, 2]);
    let causes: Vec<Cause> = read
        .entries
        .iter()
        .filter_map(|entry| match entry {
            Entry::Header(header) => Some(header.cause),
            Entry::Tick(_) => None,
        })
        .collect();
    assert_eq!(causes, [Cause::Start, Cause::Rotate]);
}

#[test]
fn a_header_whose_scheduler_table_this_build_would_reject_still_reads() {
    let path = dir("read-retired-flag").join("q.journal");
    let mut header = serde_json::to_value(header(1000)).unwrap();
    header["scheduler"] = serde_json::json!({"kind": "bayes", "retired_flag": "kept"});
    fs::write(&path, format!("{header}\n")).unwrap();
    let read = read(&[path]).unwrap();
    let [Entry::Header(header)] = read.entries.as_slice() else {
        panic!("{:?}", read.entries);
    };
    assert_eq!(header.scheduler["retired_flag"], "kept");
}

#[test]
fn a_header_whose_scheduler_is_not_a_table_names_the_path_and_line() {
    let mut header = serde_json::to_value(header(1000)).unwrap();
    header["scheduler"] = Value::from("bayes");
    let message = read_error("read-scheduler-string", &format!("{header}\n"));
    assert!(message.contains("q.journal:1:"), "{message}");
}

/// Each event as the line `dorfq journal events` prints for it.
fn event_lines(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect()
}

/// The lines of the text file at `path`.
fn text_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn derive_at(path: &Path) -> Vec<Event> {
    events::derive(
        &read(std::slice::from_ref(&path.to_owned()))
            .unwrap()
            .entries,
    )
}

#[test]
fn opening_a_segment_seeds_entry_by_entry_and_the_first_live_tick_continues_it() {
    let path = dir("seed").join("q.journal");
    let (mut writer, _) = open(&path, 1000);
    writer.record(0, 1001, &busy(0, 5)).unwrap();
    writer.record(1, 1002, &busy(1, 6)).unwrap();
    let observe = Err(DriverError::Observe(ForgeError::Transient("x".into())));
    writer.record(2, 1003, &observe).unwrap();
    let effect = Err(DriverError::Effect {
        report: Box::new(report(3, observation(8, &[1]), start_plan())),
        error: ForgeError::Transient("y".into()),
    });
    writer.record(3, 1004, &effect).unwrap();
    drop(writer);
    let before = derive_at(&path);
    let mut calls = Vec::new();
    let mut seeded = Vec::new();
    let (mut writer, _) = Writer::open(&path, header(2000), None, &mut |o, events| {
        calls.push(o.map(|o| o.now.0));
        seeded.extend_from_slice(events);
    })
    .unwrap();
    // Header, report, `same`, error, report with an error: one call each.
    assert_eq!(calls, [None, Some(5), None, None, Some(8)]);
    assert_eq!(seeded, before);
    let live = writer.record(0, 2001, &busy(0, 9)).unwrap();
    assert!(
        matches!(live[0].kind, events::Kind::Restart { started: 2000, .. }),
        "{live:?}"
    );
    assert!(
        !live
            .iter()
            .any(|e| matches!(e.kind, events::Kind::Arrive { .. })),
        "{live:?}"
    );
    seeded.extend(live);
    assert_eq!(seeded, derive_at(&path));
}

#[test]
fn an_error_before_any_observation_is_written_once_when_the_next_process_observes() {
    let dir = dir("seed-waiting");
    let path = dir.join("q.journal");
    let events = dir.join("q.events");
    let (mut writer, _) = Writer::open(&path, header(1000), Some(&events), &mut |_, _| {}).unwrap();
    let fatal = Err(DriverError::Scheduler(SchedulerError::new("stumped")));
    assert!(writer.record(0, 1001, &fatal).unwrap().is_empty());
    drop(writer);
    let mut seeded = Vec::new();
    let (mut writer, _) = Writer::open(&path, header(2000), Some(&events), &mut |_, e| {
        seeded.extend_from_slice(e)
    })
    .unwrap();
    // `derive` releases the error and the restart at the end; the seed and
    // the file wait for an observation.
    let derived = event_lines(&derive_at(&path));
    let written = text_lines(&events);
    assert!(
        written.len() < derived.len() && derived.starts_with(&written),
        "{written:?} {derived:?}"
    );
    assert!(seeded.is_empty(), "{seeded:?}");
    writer.record(0, 2001, &busy(0, 7)).unwrap();
    let derived = derive_at(&path);
    assert_eq!(text_lines(&events), event_lines(&derived));
    let at_7 = |name: &str| {
        derived
            .iter()
            .filter(|e| e.t == 7 && kind_of(e) == name)
            .count()
    };
    assert_eq!((at_7("error"), at_7("restart")), (1, 1), "{derived:?}");
}

#[test]
fn a_restart_on_a_segment_begun_at_an_idle_rotation_continues_the_events_file() {
    let fatal = || Err(DriverError::Scheduler(SchedulerError::new("stumped")));
    // The rotating process exits on an error; exits before its next tick.
    for (name, last) in [("error", Some(fatal())), ("quiet", None)] {
        let dir = dir(&format!("seed-rotated-{name}"));
        let path = dir.join("q.journal");
        let events = dir.join("q.events");
        let (mut writer, _) =
            Writer::open(&path, header(1000), Some(&events), &mut |_, _| {}).unwrap();
        writer.record(0, 1001, &busy(0, 5)).unwrap();
        writer.record(1, 1002, &idle(1, 6)).unwrap();
        if let Some(last) = &last {
            writer.record(2, 1003, last).unwrap();
        }
        drop(writer);
        let (mut writer, _) =
            Writer::open(&path, header(2000), Some(&events), &mut |_, _| {}).unwrap();
        writer.record(0, 2001, &busy(0, 7)).unwrap();
        let mut written = text_lines(&dir.join("q.events.1000"));
        written.extend(text_lines(&events));
        let all = read(&[dir.join("q.journal.1000"), path]).unwrap();
        let derived = events::derive(&all.entries);
        assert_eq!(written, event_lines(&derived), "{name}");
        let errors: Vec<u64> = derived
            .iter()
            .filter(|e| kind_of(e) == "error")
            .map(|e| e.t)
            .collect();
        let expected: &[u64] = if last.is_some() { &[7] } else { &[] };
        assert_eq!(errors, expected, "{name}");
    }
}

#[test]
fn a_segment_of_another_format_is_closed_with_its_events_file() {
    let dir = dir("other-format-events");
    let path = dir.join("q.journal");
    let events = dir.join("q.events");
    fs::write(&path, "{\"journal\":2,\"opened\":900,\"shape\":\"new\"}\n").unwrap();
    fs::write(&events, "{\"t\":1,\"kind\":\"old\"}\n").unwrap();
    let (mut writer, _) = Writer::open(&path, header(1000), Some(&events), &mut |_, _| {}).unwrap();
    assert_eq!(
        names(&dir),
        ["q.events", "q.events.900", "q.journal", "q.journal.900"]
    );
    assert_eq!(
        fs::read_to_string(dir.join("q.events.900")).unwrap(),
        "{\"t\":1,\"kind\":\"old\"}\n"
    );
    assert_eq!(fs::read_to_string(&events).unwrap(), "");
    let written = writer.record(0, 1001, &busy(0, 5)).unwrap();
    assert_eq!(text_lines(&events), event_lines(&written));
    assert_eq!(event_lines(&written), event_lines(&derive_at(&path)));
}

#[test]
fn an_events_path_naming_the_journal_s_own_file_is_refused() {
    let dir = dir("events-same-file");
    let path = dir.join("q.journal");
    drop(open(&path, 1000));
    let text = fs::read_to_string(&path).unwrap();
    fs::hard_link(&path, dir.join("linked")).unwrap();
    for events in [
        path.clone(),
        dir.join(".").join("q.journal"),
        dir.join("..")
            .join(dir.file_name().unwrap())
            .join("q.journal"),
        dir.join("linked"),
    ] {
        let Err(error) = Writer::open(&path, header(2000), Some(&events), &mut |_, _| {}) else {
            panic!("{}: opened", events.display());
        };
        let message = error.to_string();
        assert!(
            message.contains("events names the journal's own file"),
            "{message}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }
    assert_eq!(names(&dir), ["linked", "q.journal"]);
}

fn kind_of(event: &Event) -> String {
    serde_json::to_value(event).unwrap()["kind"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn the_events_file_rotates_with_the_journal_under_the_same_name() {
    let dir = dir("events-rotate");
    let path = dir.join("q.journal");
    let events = dir.join("q.events");
    let (mut writer, _) = Writer::open(&path, header(1000), Some(&events), &mut |_, _| {}).unwrap();
    let mut written = writer.record(0, 1001, &busy(0, 5)).unwrap();
    written.extend(writer.record(1, 1002, &idle(1, 6)).unwrap());
    written.extend(writer.record(2, 1003, &busy(2, 7)).unwrap());
    assert_eq!(
        names(&dir),
        ["q.events", "q.events.1000", "q.journal", "q.journal.1000"]
    );
    let mut lines = text_lines(&dir.join("q.events.1000"));
    lines.extend(text_lines(&events));
    assert_eq!(lines, event_lines(&written));
    let all = read(&[dir.join("q.journal.1000"), path]).unwrap();
    assert_eq!(lines, event_lines(&events::derive(&all.entries)));
}

#[test]
fn a_name_taken_on_either_side_moves_both_closed_names_to_one_free_suffix() {
    let dir = dir("events-collide");
    let path = dir.join("q.journal");
    let events = dir.join("q.events");
    fs::write(dir.join("q.journal.1000"), "").unwrap();
    fs::write(dir.join("q.events.1000.1"), "").unwrap();
    let (mut writer, _) = Writer::open(&path, header(1000), Some(&events), &mut |_, _| {}).unwrap();
    writer.record(0, 1001, &busy(0, 5)).unwrap();
    writer.record(1, 1002, &idle(1, 6)).unwrap();
    assert_eq!(
        names(&dir),
        [
            "q.events",
            "q.events.1000.1",
            "q.events.1000.2",
            "q.journal",
            "q.journal.1000",
            "q.journal.1000.2"
        ]
    );
    assert_eq!(fs::read_to_string(dir.join("q.journal.1000")).unwrap(), "");
    assert_eq!(fs::read_to_string(dir.join("q.events.1000.1")).unwrap(), "");
    assert!(!text_lines(&dir.join("q.events.1000.2")).is_empty());
}

#[test]
fn an_unterminated_last_events_line_is_cut_off_with_a_note() {
    let dir = dir("events-cut");
    let path = dir.join("q.journal");
    let events = dir.join("q.events");
    let (mut writer, _) = Writer::open(&path, header(1000), Some(&events), &mut |_, _| {}).unwrap();
    writer.record(0, 1001, &busy(0, 5)).unwrap();
    drop(writer);
    let whole = fs::read_to_string(&events).unwrap();
    fs::write(&events, whole.clone() + "{\"t\":5,").unwrap();
    let (_writer, notes) =
        Writer::open(&path, header(2000), Some(&events), &mut |_, _| {}).unwrap();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("q.events"), "{}", notes[0]);
    assert!(
        notes[0].contains(&format!("line {}", whole.lines().count() + 1)),
        "{}",
        notes[0]
    );
    assert_eq!(fs::read_to_string(&events).unwrap(), whole);
}

#[test]
fn a_segment_with_a_line_that_does_not_read_is_refused_before_the_header() {
    let path = dir("seed-corrupt").join("q.journal");
    let text = format!("{}\nnot json\n", header_line());
    fs::write(&path, &text).unwrap();
    let Err(error) = Writer::open(&path, header(2000), None, &mut |_, _| {}) else {
        panic!("opened");
    };
    let message = error.to_string();
    assert!(message.contains("q.journal:2: not JSON"), "{message}");
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn entries_yield_what_read_collects_and_note_an_unterminated_last_line() {
    let path = dir("entries-partial").join("q.journal");
    two_process_segment(&path);
    let mut text = fs::read(&path).unwrap();
    text.extend_from_slice(b"{\"tick\":1,");
    fs::write(&path, &text).unwrap();
    let read = read(std::slice::from_ref(&path)).unwrap();
    let mut entries = entries(&path).unwrap();
    let collected: Vec<Entry> = entries.by_ref().map(Result::unwrap).collect();
    assert_eq!(collected, read.entries);
    assert_eq!(entries.notes, read.notes);
    assert_eq!(entries.notes.len(), 1);
}

/// One configuration of every scheduler kind, `bayes` under both
/// speculations, on eight lanes. `bisect` holds up to four pull requests a
/// batch so that a failed batch is split and replay covers the bisection;
/// at one a batch is never split.
fn every_scheduler() -> Vec<SchedulerConfig> {
    let bayes = |speculation: &str| {
        format!(
            "kind = \"bayes\"\nlanes = 8\nmax_batch = 8\np_interact = 0.001\np_flake = 0.03\n\
             p_miss = 0.02\np_trunk = 0.001\nrun_cost = 0.5\nw_reject = 1.0\nw_age = 0.02\n\
             eject_above = 0.9\nnotify_above = 0.5\nothers_below = 0.1\n\
             speculate_above = 0.0\nspeculation = \"{speculation}\"\n"
        )
    };
    [
        "kind = \"all-at-once\"\n".to_owned(),
        "kind = \"bisect\"\nlanes = 8\nmax_batch = 4\n".to_owned(),
        bayes("chain"),
        bayes("tree"),
    ]
    .iter()
    .map(|text| {
        let config: SchedulerConfig = toml::from_str(text).unwrap();
        config.validate().unwrap();
        config
    })
    .collect()
}

mod fixture;
mod harness;
