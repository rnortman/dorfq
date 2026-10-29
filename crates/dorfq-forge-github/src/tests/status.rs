//! The status issue: the startup check, and `publish_status`'s floor and
//! retry rule.

use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};

use super::*;
use crate::messages::STATUS_STARTING;

/// The status issue's number in these tests.
const ISSUE: u64 = 12;

fn with_issue() -> Settings {
    Settings {
        status_issue: Some(ISSUE),
        ..settings()
    }
}

/// A fake repository holding the plain issue [`ISSUE`].
fn fake_with_issue() -> FakeGitHub {
    let fake = FakeGitHub::new();
    fake.state().issues.insert(ISSUE, String::new());
    fake
}

fn new_forge(fake: &FakeGitHub, clock: Clock) -> Result<GitHubForge, ApiError> {
    GitHubForge::new(
        Box::new(fake.clone()),
        with_issue(),
        clock,
        Box::new(io::sink()),
        None,
    )
}

/// A forge over `fake` with the status issue, whose clock the test sets.
fn status_forge(fake: &FakeGitHub) -> (GitHubForge, Rc<Cell<u64>>) {
    let (clock, now) = set_clock();
    let forge = new_forge(fake, clock).unwrap_or_else(|e| panic!("{e}"));
    (forge, now)
}

fn body(fake: &FakeGitHub) -> String {
    fake.state().issues[&ISSUE].clone()
}

fn status_writes(fake: &FakeGitHub) -> usize {
    fake.calls_to(&["set_issue_body"]).len()
}

#[test]
fn new_checks_the_issue_after_trunk_and_the_label_and_writes_its_starting_body() {
    let fake = fake_with_issue();
    let forge = new_forge(&fake, clock());
    assert!(forge.is_ok());
    assert_eq!(
        fake.calls_to(&["get_ref", "label_exists", "get_issue", "set_issue_body"]),
        [
            "get_ref refs/heads/main",
            "label_exists queue",
            "get_issue 12",
            "set_issue_body 12",
        ]
    );
    assert_eq!(fake.writes(), ["set_issue_body 12"]);
    assert_eq!(body(&fake), STATUS_STARTING);
    assert!(body(&fake).starts_with("<!-- dorfq: status -->\n"));
}

#[test]
fn new_refuses_a_missing_issue_or_a_pull_request_s_number_and_writes_nothing() {
    let missing = FakeGitHub::new();
    let error = new_forge(&missing, clock()).err().unwrap();
    assert!(matches!(error, ApiError::Permanent(_)));
    assert!(
        error
            .to_string()
            .ends_with("status issue #12 does not exist"),
        "{error}"
    );
    assert!(missing.writes().is_empty());

    let pull_request = FakeGitHub::new();
    pull_request.open_pr(ISSUE);
    let error = new_forge(&pull_request, clock()).err().unwrap();
    assert!(matches!(error, ApiError::Permanent(_)));
    assert!(
        error
            .to_string()
            .ends_with("status issue #12 is a pull request"),
        "{error}"
    );
    assert!(pull_request.writes().is_empty());
}

#[test]
fn new_fails_with_github_s_answer_when_the_starting_body_is_refused() {
    for kind in [Failure::Permanent, Failure::Transient] {
        let fake = fake_with_issue();
        fake.fail("set_issue_body", kind);
        let error = new_forge(&fake, clock()).err().unwrap();
        assert!(
            error.to_string().contains("set_issue_body: injected"),
            "{error}"
        );
        assert_eq!(body(&fake), "");
    }
}

#[test]
fn a_status_body_is_written_at_once_and_the_next_waits_for_the_floor() {
    let fake = fake_with_issue();
    let (mut forge, now) = status_forge(&fake);
    // The starting body leaves the first edit due at once.
    forge.publish_status("first".to_owned());
    assert_eq!(body(&fake), "first");
    assert_eq!(status_writes(&fake), 2);
    // Inside the floor: kept, and a newer body replaces it.
    now.set(NOW.0 + 10);
    forge.publish_status("second".to_owned());
    forge.publish_status("third".to_owned());
    assert_eq!(body(&fake), "first");
    assert_eq!(status_writes(&fake), 2);
    now.set(NOW.0 + 29);
    forge.observe().unwrap();
    assert_eq!(body(&fake), "first");
    // The first observation at the floor writes the newest.
    now.set(NOW.0 + 30);
    forge.observe().unwrap();
    assert_eq!(body(&fake), "third");
    assert_eq!(status_writes(&fake), 3);
    // Nothing pending, nothing written.
    now.set(NOW.0 + 100);
    forge.observe().unwrap();
    assert_eq!(status_writes(&fake), 3);
}

#[test]
fn a_transient_refusal_keeps_the_body_and_retries_it_backing_off() {
    let fake = fake_with_issue();
    let (mut forge, now) = status_forge(&fake);
    fake.fail("set_issue_body", Failure::Transient);
    forge.publish_status("first".to_owned());
    assert_eq!(body(&fake), STATUS_STARTING);
    let status = forge.status.as_ref().unwrap();
    assert_eq!(status.pending.as_deref(), Some("first"));
    assert_eq!(
        (status.backoff, status.due),
        (Some(5), Timestamp(NOW.0 + 5))
    );
    // A second failure at the retry doubles the hold.
    now.set(NOW.0 + 5);
    fake.fail("set_issue_body", Failure::Transient);
    forge.observe().unwrap();
    let status = forge.status.as_ref().unwrap();
    assert_eq!(
        (status.backoff, status.due),
        (Some(10), Timestamp(NOW.0 + 15))
    );
    // A newer body replaces the kept one and waits for the same retry.
    forge.publish_status("second".to_owned());
    assert_eq!(status_writes(&fake), 3);
    now.set(NOW.0 + 15);
    forge.observe().unwrap();
    assert_eq!(body(&fake), "second");
    let status = forge.status.as_ref().unwrap();
    assert_eq!(status.pending, None);
    assert_eq!((status.backoff, status.due), (None, Timestamp(NOW.0 + 45)));
}

#[test]
fn a_permanent_refusal_drops_the_body_and_a_later_one_is_written() {
    let fake = fake_with_issue();
    let (mut forge, now) = status_forge(&fake);
    fake.fail("set_issue_body", Failure::Permanent);
    forge.publish_status("refused".to_owned());
    let status = forge.status.as_ref().unwrap();
    assert_eq!((status.pending.as_deref(), status.backoff), (None, None));
    assert_eq!(body(&fake), STATUS_STARTING);
    // Nothing is retried.
    now.set(NOW.0 + 60);
    forge.observe().unwrap();
    assert_eq!(status_writes(&fake), 2);
    forge.publish_status("later".to_owned());
    assert_eq!(body(&fake), "later");
}

#[test]
fn a_clock_that_cannot_be_read_writes_the_body_at_once() {
    let fake = fake_with_issue();
    let (clock, now) = set_clock();
    let readable = Rc::new(Cell::new(true));
    let gate = Rc::clone(&readable);
    let clock: Clock = Box::new(move || {
        if gate.get() {
            clock()
        } else {
            Err("no clock".to_owned())
        }
    });
    let mut forge = new_forge(&fake, clock).unwrap();
    forge.publish_status("first".to_owned());
    now.set(NOW.0 + 1);
    readable.set(false);
    forge.publish_status("second".to_owned());
    assert_eq!(body(&fake), "second");
}

#[test]
fn without_a_status_issue_publish_status_writes_nothing() {
    let fake = fake_with_issue();
    let (mut forge, _) = forge(&fake);
    forge.publish_status("ignored".to_owned());
    assert!(fake.writes().is_empty());
    assert!(forge.status.is_none());
}

/// The error a write is refused with inside a pause ending at `until`.
fn refused_until(error: &ForgeError, until: u64) -> bool {
    matches!(error, ForgeError::Transient(_))
        && error.to_string().ends_with(&format!(
            "rate limited until unix time {until}; nothing is written before then"
        ))
}

/// The pause end these tests inject: ninety seconds after [`NOW`].
fn pause_end() -> SystemTime {
    UNIX_EPOCH + StdDuration::from_secs(NOW.0 + 90)
}

#[test]
fn a_rate_limit_pause_a_status_write_is_answered_with_refuses_the_effects_until_it_ends() {
    let fake = fake_with_issue();
    queued(&fake, 1);
    let (mut forge, now) = status_forge(&fake);
    forge.observe().unwrap();
    forge.publish_status("first".to_owned());
    now.set(NOW.0 + 10);
    forge.publish_status("second".to_owned());
    // At the floor the held body's write is refused rate limited, with a
    // pause to 90: the observation stands and the body stays pending.
    now.set(NOW.0 + 30);
    fake.fail("set_issue_body", Failure::RateLimited(pause_end()));
    forge.observe().unwrap();
    assert_eq!(
        forge.status.as_ref().unwrap().pending.as_deref(),
        Some("second")
    );
    let writes = fake.writes().len();
    let error = forge.eject(PrNumber(1), &CONFLICTS_WITH_TRUNK).unwrap_err();
    assert!(refused_until(&error, NOW.0 + 90), "{error}");
    assert_eq!(fake.writes().len(), writes);
    // Inside the pause the next observation writes nothing and an effect
    // still refuses.
    now.set(NOW.0 + 89);
    forge.observe().unwrap();
    assert!(refused_until(
        &forge.eject(PrNumber(1), &CONFLICTS_WITH_TRUNK).unwrap_err(),
        NOW.0 + 90
    ));
    assert_eq!(fake.writes().len(), writes);
    // At its end the observation writes the body and the effect runs.
    now.set(NOW.0 + 90);
    forge.observe().unwrap();
    assert_eq!(body(&fake), "second");
    forge.eject(PrNumber(1), &CONFLICTS_WITH_TRUNK).unwrap();
    assert_eq!(fake.comments(1).len(), 2);
}

#[test]
fn a_rate_limited_comment_post_keeps_the_observation_s_departures_and_refuses_the_effects() {
    let fake = FakeGitHub::new();
    queued(&fake, 2);
    let (clock, now) = set_clock();
    let (mut forge, sink) = forge_with_clock(&fake, clock);
    forge.observe().unwrap();
    forge.eject(PrNumber(2), &CONFLICTS_WITH_TRUNK).unwrap();
    // A pull request the next observation owes its greeting.
    ready_pr(&fake, 3);
    sink.0.borrow_mut().clear();
    now.set(NOW.0 + 10);
    fake.fail("create_comment", Failure::RateLimited(pause_end()));
    let observation = forge.observe().unwrap();
    assert_eq!(
        observation
            .departed
            .iter()
            .map(|d| (d.pr, d.how.clone()))
            .collect::<Vec<_>>(),
        [(PrNumber(2), Departed::Ejected)]
    );
    assert!(fake.comments(3).is_empty());
    assert_eq!(forge.outbox.len(), 1, "the greeting is kept");
    let writes = fake.writes().len();
    let error = forge.eject(PrNumber(1), &CONFLICTS_WITH_TRUNK).unwrap_err();
    assert!(refused_until(&error, NOW.0 + 90), "{error}");
    assert_eq!(fake.writes().len(), writes);
    // The observation at the pause's end posts the kept greeting once.
    now.set(NOW.0 + 90);
    forge.observe().unwrap();
    assert_eq!(fake.comments(3).len(), 1);
    assert_eq!(sink.lines(), ["message to pr 3: queued"]);
    forge.eject(PrNumber(1), &CONFLICTS_WITH_TRUNK).unwrap();
}

#[test]
fn a_pause_announced_to_an_effect_s_closing_post_refuses_the_next_effect() {
    let fake = FakeGitHub::new();
    queued(&fake, 2);
    let (clock, _) = set_clock();
    let (mut forge, _) = forge_with_clock(&fake, clock);
    forge.observe().unwrap();
    fake.fail("create_comment", Failure::RateLimited(pause_end()));
    // The ejection is done and its comment kept; the plan's next effect
    // refuses before it writes.
    forge.eject(PrNumber(1), &CONFLICTS_WITH_TRUNK).unwrap();
    assert!(fake.comments(1).len() == 1, "only the greeting");
    let writes = fake.writes().len();
    let error = forge.eject(PrNumber(2), &CONFLICTS_WITH_TRUNK).unwrap_err();
    assert!(refused_until(&error, NOW.0 + 90), "{error}");
    assert_eq!(fake.writes().len(), writes);
}

#[test]
fn a_recorded_pause_refuses_a_write_while_the_clock_cannot_be_read() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let readable = Rc::new(Cell::new(true));
    let reads = Rc::clone(&readable);
    let clock: Clock = Box::new(move || {
        if reads.get() {
            Ok(NOW)
        } else {
            Err("the clock cannot be read".to_owned())
        }
    });
    let (mut forge, _) = forge_with_clock(&fake, clock);
    forge.observe().unwrap();
    // A pause is recorded and the clock cannot rule out that it is in force.
    fake.state().rate_limit_pause = Some(pause_end());
    readable.set(false);
    let writes = fake.writes().len();
    let error = forge.eject(PrNumber(1), &CONFLICTS_WITH_TRUNK).unwrap_err();
    assert!(refused_until(&error, NOW.0 + 90), "{error}");
    assert_eq!(fake.writes().len(), writes);
}

#[test]
fn a_notice_inside_a_pause_is_kept_and_posted_once_when_it_ends() {
    let fake = FakeGitHub::new();
    let (clock, now) = set_clock();
    let (mut forge, sink) = forge_with_clock(&fake, clock);
    // A queued pull request the observation owes its greeting; the post is
    // answered with a pause.
    ready_pr(&fake, 3);
    fake.fail("create_comment", Failure::RateLimited(pause_end()));
    forge.observe().unwrap();
    assert_eq!(forge.outbox.len(), 1, "the greeting is kept");
    let writes = fake.writes().len();

    forge.notify(PrNumber(3), &BROKEN).unwrap();

    assert_eq!(fake.writes().len(), writes);
    assert_eq!(
        forge.outbox.len(),
        2,
        "the notice waits behind the greeting"
    );
    now.set(NOW.0 + 90);
    forge.observe().unwrap();
    assert_eq!(marks(&fake, 3), ["queued", "notice"]);
    assert_eq!(
        sink.lines(),
        [
            "message to pr 3: queued".to_owned(),
            format!("message to pr 3: notice: {BROKEN}"),
        ]
    );
}

#[test]
fn a_pause_announced_to_a_read_inside_observe_keeps_the_outbox_until_it_ends() {
    let fake = FakeGitHub::new();
    let (clock, now) = set_clock();
    let (mut forge, _) = forge_with_clock(&fake, clock);
    forge.observe().unwrap();
    // An hour on, every entity is due and the listing is read.
    ready_pr(&fake, 3);
    fake.fail("create_comment", Failure::Transient);
    now.set(NOW.0 + 3600);
    forge.observe().unwrap();
    assert_eq!(forge.outbox.len(), 1, "the greeting is kept");
    now.set(NOW.0 + 7200);
    let until = UNIX_EPOCH + StdDuration::from_secs(NOW.0 + 7290);
    fake.fail("list_labelled", Failure::RateLimited(until));

    let error = forge.observe().unwrap_err();

    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert!(fake.comments(3).is_empty());
    assert_eq!(forge.outbox.len(), 1, "still kept");
    now.set(NOW.0 + 7290);
    forge.observe().unwrap();
    assert_eq!(fake.comments(3).len(), 1);
}

#[test]
fn a_landing_whose_retarget_is_answered_with_a_pause_leaves_the_put_back_to_its_record() {
    // A stack 3 on 2 on 1 in batch 1, passed: 2 moves onto trunk, and the
    // retarget of 3 is answered with a pause, so the move back of 2 is
    // refused unsent.
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    stacked(&fake, 2, 1);
    stacked(&fake, 3, 2);
    let (clock, now) = set_clock();
    let (mut forge, _) = forge_with_clock(&fake, clock);
    forge.observe().unwrap();
    forge
        .start_batch(&start(1, &fake.trunk(), &[1, 2, 3]))
        .unwrap();
    let tip = fake.get(&batch_ref(1)).expect("published");
    passed(&fake, &tip);
    // Past the batch's deadline, so the observation reads it passed.
    now.set(NOW.0 + 3600);
    forge.observe().unwrap();
    let until = NOW.0 + 3690;
    fake.fail_on(
        "set_base",
        "3 main",
        Failure::RateLimited(UNIX_EPOCH + StdDuration::from_secs(until)),
    );

    let error = forge.land(BatchId(1)).unwrap_err();

    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert_eq!(
        fake.calls_to(&["set_base"]),
        ["set_base 2 main", "set_base 3 main"]
    );
    assert_eq!(fake.pr(2).base, "main", "left for the record");
    assert_eq!(
        record_of(&fake, 1).state,
        RecordState::Landing { tip: tip.clone() }
    );
    // An observation inside the pause has its put-back refused unsent, and
    // the record stays for the next one.
    now.set(until - 1);
    let error = forge.observe().unwrap_err();
    assert!(refused_until(&error, until), "{error}");
    assert_eq!(
        fake.calls_to(&["set_base"]),
        ["set_base 2 main", "set_base 3 main"]
    );
    assert_eq!(record_of(&fake, 1).state, RecordState::Landing { tip });
    // At the pause's end the record's recovery puts 2 back on its base.
    now.set(until);
    forge.observe().unwrap();
    assert_eq!(
        fake.calls_to(&["set_base"]),
        ["set_base 2 main", "set_base 3 main", "set_base 2 pr-1"]
    );
    assert_eq!(fake.pr(2).base, "pr-1");
}

#[test]
fn a_move_back_answered_with_a_pause_stops_and_leaves_the_rest_to_the_record() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    stacked(&fake, 2, 1);
    stacked(&fake, 3, 2);
    let (mut forge, _) = observed(&fake);
    forge
        .start_batch(&start(1, &fake.trunk(), &[1, 2, 3]))
        .unwrap();
    let tip = fake.get(&batch_ref(1)).expect("published");
    passed(&fake, &tip);
    forge.observe().unwrap();
    // Trunk moves under the landing, so it is refused and moves both back.
    let moved = fake.commit(&[fake.trunk()]);
    fake.state().interpose = Some((branch("main"), moved));
    // Past every reading the hourly clock takes in this test.
    let until = UNIX_EPOCH + StdDuration::from_secs(NOW.0 + 1_000_000);
    fake.fail_on("set_base", "2 pr-1", Failure::RateLimited(until));

    let error = forge.land(BatchId(1)).unwrap_err();

    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert_eq!(
        fake.calls_to(&["set_base"]),
        ["set_base 2 main", "set_base 3 main", "set_base 2 pr-1"]
    );
    assert_eq!(fake.pr(3).base, "main", "left for the record");
    assert_eq!(record_of(&fake, 1).state, RecordState::Landing { tip });
}

/// An effect, run on a forge whose ledger accepts it.
type Effect = fn(&mut GitHubForge, &FakeGitHub) -> Result<(), ForgeError>;

#[test]
fn every_effect_refuses_inside_a_pause_before_it_writes() {
    let effects: [(&str, Effect); 4] = [
        ("start_batch", |forge, fake| {
            forge.start_batch(&start(2, &fake.trunk(), &[2]))
        }),
        ("land", |forge, _| forge.land(BatchId(1))),
        ("retire", |forge, _| forge.retire(BatchId(1))),
        ("eject", |forge, _| {
            forge.eject(PrNumber(2), &CONFLICTS_WITH_TRUNK)
        }),
    ];
    // Past every reading the hourly clock takes in this test.
    let until = NOW.0 + 1_000_000;
    let paused = || {
        // Batch 1 holds pull request 1 and has passed; pull request 2 is
        // queued outside it.
        let fake = FakeGitHub::new();
        let (mut forge, _, _) = passed_batch(&fake, 1);
        let head = fake.open_pr(2);
        passed(&fake, &head);
        fake.state().comments.insert(2, vec![marker("queued")]);
        forge.observe().unwrap();
        fake.state().rate_limit_pause = Some(UNIX_EPOCH + StdDuration::from_secs(until));
        let writes = fake.writes().len();
        (fake, forge, writes)
    };
    for (name, effect) in effects {
        let (fake, mut forge, writes) = paused();
        let error = effect(&mut forge, &fake).unwrap_err();
        assert!(refused_until(&error, until), "{name}: {error}");
        assert_eq!(fake.writes().len(), writes, "{name} wrote");
    }
    // A notice makes no write of its own: it is kept in the outbox.
    let (fake, mut forge, writes) = paused();
    forge.notify(PrNumber(2), &BROKEN).unwrap();
    assert_eq!(fake.writes().len(), writes, "notify wrote");
    assert_eq!(forge.outbox.len(), 1);
}

/// A sink every write to fails.
struct Broken;

impl Write for Broken {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("the sink is closed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_failed_flush_still_writes_a_held_status_body_and_fails_the_observation() {
    let fake = fake_with_issue();
    let (clock, now) = set_clock();
    let mut forge = GitHubForge::new(
        Box::new(fake.clone()),
        with_issue(),
        clock,
        Box::new(Broken),
        None,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    forge.publish_status("first".to_owned());
    now.set(NOW.0 + 10);
    forge.publish_status("second".to_owned());
    // A queued pull request the forge has not told yet: the observation
    // posts its comment, and the sink line after it fails.
    let head = fake.open_pr(1);
    passed(&fake, &head);
    now.set(NOW.0 + 30);
    let error = forge.observe().unwrap_err();
    assert!(matches!(error, ForgeError::Permanent(_)), "{error}");
    assert!(error.to_string().contains("the sink is closed"), "{error}");
    assert_eq!(body(&fake), "second");
}
