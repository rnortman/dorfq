use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::Path;

use dorfq_core::{ForgeError, PrNumber, Sha};
use dorfq_forge_github::HeadFiles;
use dorfq_forge_github::api::ChangedFile;
use serde_json::Value;

use super::super::events::{Event, Kind};
use super::super::tests::{dir, sha};
use super::*;

/// What a test reader answers for one read.
pub(in crate::journal) type Answer = Result<Result<HeadFiles, Sha>, ForgeError>;

/// A test reader: the answers scripted per `(pr, head)`, taken in order,
/// then [`one_file`] once a head's are used up; every read logged.
#[derive(Default)]
pub(in crate::journal) struct Script {
    answers: BTreeMap<(u64, Sha), VecDeque<Answer>>,
    pub(in crate::journal) calls: Vec<(u64, Sha)>,
}

impl Script {
    pub(in crate::journal) fn answer(&mut self, pr: u64, head: Sha, answer: Answer) {
        self.answers
            .entry((pr, head))
            .or_default()
            .push_back(answer);
    }

    pub(in crate::journal) fn read(&mut self, pr: PrNumber, head: &Sha) -> Answer {
        self.calls.push((pr.0, head.clone()));
        self.answers
            .get_mut(&(pr.0, head.clone()))
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(|| Ok(Ok(one_file(pr.0))))
    }
}

/// Pull request `pr`'s files when the script says nothing else: one file,
/// `pr/<pr>`, one line added.
pub(in crate::journal) fn one_file(pr: u64) -> HeadFiles {
    HeadFiles {
        base: sha(100),
        additions: 1,
        deletions: 0,
        changed_files: 1,
        files: vec![ChangedFile {
            path: format!("pr/{pr}"),
            additions: 1,
            deletions: 0,
            status: "modified".to_owned(),
            previous: None,
        }],
    }
}

/// Every line of the capture file at `path`, as JSON.
pub(in crate::journal) fn records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn arrive(t: u64, pr: u64, head: &Sha) -> Event {
    Event {
        t,
        kind: Kind::Arrive {
            pr,
            base: None,
            atomic: false,
            head: head.clone(),
        },
    }
}

fn repush(t: u64, pr: u64, head: &Sha) -> Event {
    Event {
        t,
        kind: Kind::Repush {
            pr,
            head: head.clone(),
        },
    }
}

fn open(path: &Path) -> Capture {
    let mut notes = Vec::new();
    let capture = Capture::open(path, &[], &mut notes).unwrap();
    assert!(notes.is_empty(), "{notes:?}");
    capture
}

fn fetch(capture: &mut Capture, at: u64, script: &mut Script) -> Vec<String> {
    capture
        .fetch(at, &mut |pr, head| script.read(pr, head))
        .unwrap()
}

#[test]
fn a_record_carries_the_key_the_first_sighting_the_fetch_and_the_files_in_order() {
    let path = dir("capture-shape").join("q.capture");
    let mut capture = open(&path);
    let mut script = Script::default();
    let files = vec![
        ChangedFile {
            path: "crates/x/src/lib.rs".to_owned(),
            additions: 100,
            deletions: 7,
            status: "modified".to_owned(),
            previous: None,
        },
        ChangedFile {
            path: "docs/new.md".to_owned(),
            additions: 20,
            deletions: 0,
            status: "renamed".to_owned(),
            previous: Some("docs/old.md".to_owned()),
        },
    ];
    script.answer(
        42,
        sha(42),
        Ok(Ok(HeadFiles {
            base: sha(100),
            additions: 120,
            deletions: 7,
            changed_files: 3,
            files,
        })),
    );
    // A head sighted again before its fetch keeps its first sighting's time.
    capture.sight(&[arrive(5, 42, &sha(42))]);
    capture.sight(&[repush(7, 42, &sha(42))]);
    assert!(fetch(&mut capture, 1003, &mut script).is_empty());
    let expected = format!(
        "{{\"capture\":1,\"pr\":42,\"head\":\"{}\",\"t\":5,\"at\":1003,\"base\":\"{}\",\
         \"additions\":120,\"deletions\":7,\"changed_files\":3,\"files\":[\
         {{\"path\":\"crates/x/src/lib.rs\",\"additions\":100,\"deletions\":7,\"status\":\"modified\"}},\
         {{\"path\":\"docs/new.md\",\"additions\":20,\"deletions\":0,\"status\":\"renamed\",\
         \"previous\":\"docs/old.md\"}}]}}\n",
        sha(42),
        sha(100)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), expected);
    assert_eq!(script.calls, [(42, sha(42))]);
}

#[test]
fn an_unterminated_last_capture_line_is_cut_off_with_a_note() {
    let path = dir("capture-cut").join("q.capture");
    let mut capture = open(&path);
    let mut script = Script::default();
    capture.sight(&[arrive(5, 1, &sha(1))]);
    fetch(&mut capture, 1001, &mut script);
    drop(capture);
    let whole = fs::read_to_string(&path).unwrap();
    fs::write(&path, whole.clone() + "{\"capture\":1,\"pr\":2,").unwrap();
    let mut notes = Vec::new();
    let mut capture = Capture::open(&path, &[], &mut notes).unwrap();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(
        notes[0],
        format!("capture {}: cut off unterminated line 2", path.display())
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), whole);
    // The cut record's head is not held, so it is fetched again.
    capture.sight(&[arrive(5, 1, &sha(1)), arrive(6, 2, &sha(2))]);
    script.calls.clear();
    fetch(&mut capture, 2001, &mut script);
    assert_eq!(script.calls, [(2, sha(2))]);
    assert_eq!(records(&path).len(), 2);
}

#[test]
fn a_capture_line_without_a_key_is_refused_naming_the_line() {
    let path = dir("capture-keyless").join("q.capture");
    let mut capture = open(&path);
    capture.sight(&[arrive(5, 1, &sha(1))]);
    fetch(&mut capture, 1001, &mut Script::default());
    drop(capture);
    let text = fs::read_to_string(&path).unwrap() + "{\"capture\":1,\"pr\":2}\n";
    fs::write(&path, &text).unwrap();
    let Err(error) = Capture::open(&path, &[], &mut Vec::new()) else {
        panic!("opened");
    };
    let message = error.to_string();
    assert!(
        message.contains("q.capture:2: not a capture record"),
        "{message}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn fetch_records_at_most_eight_heads_a_tick_and_leaves_the_rest_pending() {
    let path = dir("capture-bound").join("q.capture");
    let mut capture = open(&path);
    let mut script = Script::default();
    script.answer(3, sha(3), Ok(Err(sha(33))));
    script.answer(
        5,
        sha(5),
        Err(ForgeError::Permanent("GET /pulls/5: HTTP 404".into())),
    );
    let sighted: Vec<Event> = (1..=10).map(|pr| arrive(5, pr, &sha(pr))).collect();
    capture.sight(&sighted);
    let lines = fetch(&mut capture, 1001, &mut script);
    let asked = |calls: &[(u64, Sha)]| calls.iter().map(|(pr, _)| *pr).collect::<Vec<u64>>();
    assert_eq!(asked(&script.calls), [1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(lines.len(), 2, "{lines:?}");
    // The moved head counts as one of the eight asked, not one recorded.
    assert_eq!(records(&path).len(), 7);
    script.calls.clear();
    assert!(fetch(&mut capture, 1002, &mut script).is_empty());
    assert_eq!(asked(&script.calls), [9, 10]);
    let recorded: Vec<u64> = records(&path)
        .iter()
        .map(|r| r["pr"].as_u64().unwrap())
        .collect();
    assert_eq!(recorded, [1, 2, 4, 5, 6, 7, 8, 9, 10]);
    script.calls.clear();
    fetch(&mut capture, 1003, &mut script);
    assert!(script.calls.is_empty(), "{:?}", script.calls);
}

#[test]
fn a_transient_failure_leaves_the_head_pending_and_ends_the_tick_s_fetching() {
    let path = dir("capture-transient").join("q.capture");
    let mut capture = open(&path);
    let mut script = Script::default();
    script.answer(
        1,
        sha(1),
        Err(ForgeError::Transient("GET /pulls/1: HTTP 502".into())),
    );
    capture.sight(&[arrive(5, 1, &sha(1)), arrive(5, 2, &sha(2))]);
    let lines = fetch(&mut capture, 1001, &mut script);
    assert_eq!(
        lines,
        [format!(
            "capture of #1 at {}: GET /pulls/1: HTTP 502; retrying",
            sha(1)
        )]
    );
    assert_eq!(script.calls, [(1, sha(1))]);
    assert_eq!(fs::read_to_string(&path).unwrap(), "");
    // The next call starts after the head that failed, then wraps round to it.
    assert!(fetch(&mut capture, 1002, &mut script).is_empty());
    assert_eq!(script.calls, [(1, sha(1)), (2, sha(2)), (1, sha(1))]);
    let records = records(&path);
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["pr"], 2);
    assert_eq!(
        (&records[1]["pr"], &records[1]["at"]),
        (&1.into(), &1002.into())
    );
    assert_eq!(records[1]["t"], 5);
}

#[test]
fn a_head_that_keeps_failing_transiently_does_not_hold_back_the_heads_behind_it() {
    let path = dir("capture-starve").join("q.capture");
    let mut capture = open(&path);
    let mut script = Script::default();
    for _ in 0..2 {
        script.answer(
            1,
            sha(1),
            Err(ForgeError::Transient("GET /pulls/1: timed out".into())),
        );
    }
    let sighted: Vec<Event> = (1..=10).map(|pr| arrive(5, pr, &sha(pr))).collect();
    capture.sight(&sighted);
    let asked = |calls: &[(u64, Sha)]| calls.iter().map(|(pr, _)| *pr).collect::<Vec<u64>>();
    assert_eq!(fetch(&mut capture, 1001, &mut script).len(), 1);
    assert_eq!(asked(&script.calls), [1]);
    script.calls.clear();
    assert!(fetch(&mut capture, 1002, &mut script).is_empty());
    assert_eq!(asked(&script.calls), [2, 3, 4, 5, 6, 7, 8, 9]);
    script.calls.clear();
    assert_eq!(fetch(&mut capture, 1003, &mut script).len(), 1);
    assert_eq!(asked(&script.calls), [10, 1]);
    let recorded: Vec<u64> = records(&path)
        .iter()
        .map(|r| r["pr"].as_u64().unwrap())
        .collect();
    assert_eq!(recorded, (2..=10).collect::<Vec<u64>>());
    script.calls.clear();
    assert!(fetch(&mut capture, 1004, &mut script).is_empty());
    assert_eq!(asked(&script.calls), [1]);
    assert_eq!(records(&path).len(), 10);
}

#[test]
fn a_permanent_failure_is_recorded_once() {
    let path = dir("capture-failed").join("q.capture");
    let mut capture = open(&path);
    let mut script = Script::default();
    script.answer(
        2,
        sha(2),
        Err(ForgeError::Permanent(
            "GET /pulls/2: HTTP 404: Not Found".into(),
        )),
    );
    let sighted = [arrive(5, 2, &sha(2))];
    capture.sight(&sighted);
    let lines = fetch(&mut capture, 1001, &mut script);
    assert_eq!(
        lines,
        [format!(
            "capture of #2 at {} recorded as failed: GET /pulls/2: HTTP 404: Not Found",
            sha(2)
        )]
    );
    let records = records(&path);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["error"], "GET /pulls/2: HTTP 404: Not Found");
    assert!(records[0].get("files").is_none(), "{}", records[0]);
    assert!(records[0].get("base").is_none(), "{}", records[0]);
    assert_eq!(records[0]["capture"], 1);
    // Sighted again in this process, then after a restart onto the file.
    capture.sight(&sighted);
    fetch(&mut capture, 1002, &mut script);
    drop(capture);
    let mut capture = open(&path);
    capture.sight(&sighted);
    fetch(&mut capture, 2001, &mut script);
    assert_eq!(script.calls, [(2, sha(2))]);
    assert_eq!(super::super::tests::text_lines(&path).len(), 1);
}

#[test]
fn a_moved_head_writes_nothing_and_is_fetched_when_sighted_again() {
    let path = dir("capture-moved").join("q.capture");
    let mut capture = open(&path);
    let mut script = Script::default();
    script.answer(1, sha(1), Ok(Err(sha(11))));
    script.answer(1, sha(1), Ok(Ok(one_file(1))));
    capture.sight(&[arrive(5, 1, &sha(1))]);
    let lines = fetch(&mut capture, 1001, &mut script);
    assert_eq!(
        lines,
        [format!(
            "capture of #1 at {} not read: the head moved to {}",
            sha(1),
            sha(11)
        )]
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "");
    // Nothing is pending once the move is reported.
    fetch(&mut capture, 1002, &mut script);
    assert_eq!(script.calls, [(1, sha(1))]);
    // Pushed back to the head: it is sighted again and its files are read.
    capture.sight(&[repush(9, 1, &sha(1))]);
    assert!(fetch(&mut capture, 1003, &mut script).is_empty());
    let records = records(&path);
    assert_eq!(records.len(), 1);
    assert_eq!(
        (&records[0]["pr"], &records[0]["t"], &records[0]["at"]),
        (&1.into(), &9.into(), &1003.into())
    );
    assert_eq!(records[0]["files"][0]["path"], "pr/1");
    // A restart onto the file holds the head.
    drop(capture);
    let mut capture = open(&path);
    capture.sight(&[arrive(5, 1, &sha(1)), repush(9, 1, &sha(1))]);
    fetch(&mut capture, 2001, &mut script);
    assert_eq!(script.calls, [(1, sha(1)), (1, sha(1))]);
    assert_eq!(super::super::tests::text_lines(&path).len(), 1);
}
