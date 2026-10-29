//! `wait` and the hints it resolves.

use std::sync::mpsc::{self, Sender};
use std::time::{Duration as StdDuration, UNIX_EPOCH};

use super::*;
use crate::webhook::Hint;
use crate::{Holds, Made, Resolution, WaitHold, hold, hold_line, not_queued_line};

/// A clock reading `readings` seconds after [`NOW`] in turn, then failing;
/// the count of readings taken.
fn scripted(readings: &[u64]) -> (Clock, Rc<Cell<usize>>) {
    let readings: Vec<u64> = readings.to_vec();
    let taken = Rc::new(Cell::new(0));
    let counter = Rc::clone(&taken);
    let clock: Clock = Box::new(move || {
        let index = counter.get();
        counter.set(index + 1);
        readings
            .get(index)
            .map(|&offset| Timestamp(NOW.0 + offset))
            .ok_or_else(|| "the script ran out".to_owned())
    });
    (clock, taken)
}

/// A forge over `fake` reading `clock`, with a hint channel.
fn hinted(fake: &FakeGitHub, clock: Clock) -> (GitHubForge, Sender<Hint>) {
    let (send, receive) = mpsc::channel();
    let forge = GitHubForge::new(
        Box::new(fake.clone()),
        settings(),
        clock,
        Box::new(io::sink()),
        Some(receive),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    (forge, send)
}

/// A forge with a hint channel whose clock the test sets.
fn hinted_at(fake: &FakeGitHub) -> (GitHubForge, Sender<Hint>, Rc<Cell<u64>>) {
    let (clock, now) = set_clock();
    let (forge, send) = hinted(fake, clock);
    (forge, send, now)
}

/// Every call `forge` makes observing at `offset` seconds after [`NOW`].
fn calls_observing(
    fake: &FakeGitHub,
    forge: &mut GitHubForge,
    clock: &Cell<u64>,
    offset: u64,
) -> (Observation, Vec<String>) {
    clock.set(NOW.0 + offset);
    let before = fake.state().calls.len();
    let observation = forge.observe().unwrap();
    (observation, fake.state().calls[before..].to_vec())
}

#[test]
fn wait_returns_at_once_while_something_is_due() {
    let fake = FakeGitHub::new();
    let (clock, taken) = scripted(&[0, 0]);
    let (mut forge, _send) = hinted(&fake, clock);
    let before = fake.state().calls.len();
    forge.wait(false);
    assert_eq!(taken.get(), 2);
    assert_eq!(fake.state().calls.len(), before);
}

#[test]
fn a_hint_naming_a_queued_head_ends_the_wait_and_reads_that_pull_request() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 2);
    let (mut forge, send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    clock.set(NOW.0 + 10);
    send.send(Hint::Commit(heads[1].clone())).unwrap();
    forge.wait(false);
    let (_, calls) = calls_observing(&fake, &mut forge, &clock, 10);
    assert_eq!(
        calls,
        ["get_pr 2".to_owned(), format!("checks {}", heads[1])]
    );
}

#[test]
fn a_hint_naming_a_testing_batch_s_tip_reads_that_batch_alone() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let tip = tip_over(&fake, &[1]);
    put_record(&fake, 9, &[1], RecordState::Testing { tip: tip.clone() });
    fake.set_ref(&batch_ref(9), &tip);
    let (mut forge, _send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    passed(&fake, &tip);
    forge.resolve(Hint::Commit(tip.clone()), Timestamp(NOW.0 + 10));
    let (observation, calls) = calls_observing(&fake, &mut forge, &clock, 10);
    assert_eq!(
        calls,
        [
            "get_ref refs/dorfq/batches/9".to_owned(),
            format!("get_commit {}", fake.get("refs/dorfq/batches/9").unwrap()),
            format!("get_ref {}", batch_ref(9)),
            format!("checks {tip}"),
        ]
    );
    assert_eq!(
        observation.batches[0].state,
        BatchState::Testing {
            tip,
            ci: CiStatus::Passed
        }
    );
}

#[test]
fn a_hint_naming_an_unknown_commit_makes_nothing_due() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    let elsewhere = fake.commit(&[fake.trunk()]);
    forge.resolve(Hint::Commit(elsewhere), Timestamp(NOW.0 + 10));
    let (_, calls) = calls_observing(&fake, &mut forge, &clock, 10);
    assert!(calls.is_empty(), "{calls:?}");
}

#[test]
fn a_hint_for_trunk_reads_trunk() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    forge.resolve(Hint::Trunk, Timestamp(NOW.0 + 10));
    let (_, calls) = calls_observing(&fake, &mut forge, &clock, 10);
    assert_eq!(calls, ["get_ref refs/heads/main"]);
}

#[test]
fn a_hint_for_an_uncached_labelled_pull_request_reads_and_reports_it() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    let head = ready_pr(&fake, 2);
    forge.resolve(Hint::Pr(PrNumber(2)), Timestamp(NOW.0 + 1));
    let (observation, calls) = calls_observing(&fake, &mut forge, &clock, 1);
    // Its greeting is posted in the same observation.
    assert_eq!(
        calls,
        [
            "get_pr 2".to_owned(),
            format!("checks {head}"),
            format!("compare {head} {}", fake.trunk()),
            "create_comment 2".to_owned(),
        ]
    );
    assert_eq!(
        observation
            .prs
            .iter()
            .map(|pr| pr.number.0)
            .collect::<Vec<_>>(),
        [1, 2]
    );
}

#[test]
fn a_hint_for_an_uncached_unlabelled_pull_request_reads_it_once_and_never_reports_it() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    fake.open_pr(2);
    fake.pr(2).labels.clear();
    forge.resolve(Hint::Pr(PrNumber(2)), Timestamp(NOW.0 + 1));
    let (observation, calls) = calls_observing(&fake, &mut forge, &clock, 1);
    assert_eq!(calls, ["get_pr 2"]);
    assert_eq!(observation.prs.len(), 1);
    assert!(observation.departed.is_empty());
    let (_, calls) = calls_observing(&fake, &mut forge, &clock, 2);
    assert!(calls.is_empty(), "{calls:?}");
}

#[test]
fn two_hints_for_one_pull_request_within_the_floor_read_it_once() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 1);
    let (mut forge, _send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    forge.resolve(Hint::Pr(PrNumber(1)), Timestamp(NOW.0 + 1));
    forge.resolve(Hint::Commit(heads[0].clone()), Timestamp(NOW.0 + 2));
    // The floor holds both until five seconds after the last read.
    let (_, calls) = calls_observing(&fake, &mut forge, &clock, 4);
    assert!(calls.is_empty(), "{calls:?}");
    let (_, calls) = calls_observing(&fake, &mut forge, &clock, 5);
    assert_eq!(
        calls,
        ["get_pr 1".to_owned(), format!("checks {}", heads[0])]
    );
    let (_, calls) = calls_observing(&fake, &mut forge, &clock, 6);
    assert!(calls.is_empty(), "{calls:?}");
}

#[test]
fn a_disconnected_hint_channel_ends_the_wait_and_fails_the_next_observation() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    drop(send);
    forge.wait(false);
    let before = fake.state().calls.len();
    let error = forge.observe().unwrap_err();
    assert!(
        is_permanent(&error, "webhook listener thread exited"),
        "{error}"
    );
    assert_eq!(fake.state().calls.len(), before);
}

#[test]
fn a_recorded_rate_limit_pause_holds_the_wait_and_no_hint_shortens_it() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 1);
    // Read at 0, then: the wait's start, a reading before the pause ends
    // (a hint arrives), the reading the hint is resolved at, one more
    // before the pause ends (no hint), its end.
    let (clock, taken) = scripted(&[0, 1, 2, 2, 2, 3]);
    let (mut forge, send) = hinted(&fake, clock);
    forge.observe().unwrap();
    forge.mark_due(Entity::Trunk);
    fake.state().rate_limit_pause = Some(UNIX_EPOCH + StdDuration::from_secs(NOW.0 + 3));
    send.send(Hint::Commit(heads[0].clone())).unwrap();
    forge.wait(false);
    assert_eq!(taken.get(), 6);
    assert_eq!(forge.schedule.next_deadline(), Some(Timestamp(NOW.0 + 3)));
    assert_eq!(fake.state().rate_limit_pause, None);
}

#[test]
fn a_pause_ending_inside_a_second_rounds_up() {
    let fake = FakeGitHub::new();
    let (clock, _) = scripted(&[0, 100]);
    let (mut forge, _send) = hinted(&fake, clock);
    fake.state().rate_limit_pause =
        Some(UNIX_EPOCH + StdDuration::from_millis((NOW.0 + 40) * 1000 + 1));
    forge.wait(false);
    assert_eq!(forge.schedule.next_deadline(), Some(Timestamp(NOW.0 + 41)));
}

#[test]
fn transient_failures_back_off_doubling_to_a_minute_and_a_completed_tick_resets() {
    // Each wait reads its start, a second before its hold ends, the same
    // second again for the hint that arrives, and the hold's end. A hint
    // sent before each wait ends the receive at once, so the hold, not a
    // sleep, decides when the wait returns.
    let holds = [
        (true, 0, 5),
        (true, 5, 15),
        (true, 15, 35),
        (true, 35, 75),
        (true, 75, 135),
        (true, 135, 195),
        (false, 195, 195),
        (true, 195, 200),
    ];
    let readings: Vec<u64> = holds
        .iter()
        .flat_map(|&(transient, start, end)| {
            if transient {
                vec![start, end - 1, end - 1, end]
            } else {
                vec![start, end]
            }
        })
        .collect();
    let (clock, taken) = scripted(&readings);
    let fake = FakeGitHub::new();
    let (mut forge, send) = hinted(&fake, clock);
    let mut expected = 0;
    for (transient, start, end) in holds {
        if transient {
            send.send(Hint::Trunk).unwrap();
            expected += 4;
        } else {
            expected += 2;
        }
        forge.wait(transient);
        assert_eq!(taken.get(), expected, "the hold from {start} to {end}");
    }
}

#[test]
fn a_backoff_holds_the_wait_though_something_is_due() {
    let fake = FakeGitHub::new();
    // The wait's start, a reading inside the hold, its end.
    let (clock, taken) = scripted(&[0, 4, 5]);
    let (mut forge, _send) = hinted(&fake, clock);
    forge.wait(true);
    assert_eq!(taken.get(), 3);
}

#[test]
fn without_a_receiver_wait_sleeps_to_the_next_deadline() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    // Observe at 0; the wait's start and a reading a second before trunk's
    // deadline sleep one second; the deadline ends it.
    let (clock, taken) = scripted(&[0, BATCH_SECS - 1, BATCH_SECS - 1, BATCH_SECS]);
    let (mut forge, _) = forge_with_clock(&fake, clock);
    forge.observe().unwrap();
    forge.wait(false);
    assert_eq!(taken.get(), 4);
}

/// The lines `resolve` writes for `hint` at `offset` seconds after [`NOW`].
fn resolved(forge: &mut GitHubForge, hint: Hint, offset: u64) -> String {
    forge.resolve(hint, Timestamp(NOW.0 + offset)).to_string()
}

#[test]
fn a_resolved_pull_request_hint_says_whether_it_was_cached_and_when_it_is_due() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    assert_eq!(
        resolved(&mut forge, Hint::Pr(PrNumber(2)), 2),
        "dorfq: hint pull request #2: not cached; due now"
    );
    assert_eq!(
        resolved(&mut forge, Hint::Pr(PrNumber(1)), 2),
        "dorfq: hint pull request #1: cached; due in 3 s (read less than 5 s ago)"
    );
    // Long after the read, the floor no longer applies.
    assert_eq!(
        resolved(&mut forge, Hint::Pr(PrNumber(1)), 200),
        "dorfq: hint pull request #1: cached; due now"
    );
}

#[test]
fn a_cached_pull_request_hinted_outside_the_floor_is_due_now() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    assert_eq!(
        resolved(&mut forge, Hint::Pr(PrNumber(1)), 5),
        "dorfq: hint pull request #1: cached; due now"
    );
}

#[test]
fn a_resolved_commit_hint_names_each_entity_it_made_due_or_says_it_was_dropped() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 1);
    let tip = tip_over(&fake, &[1]);
    put_record(&fake, 9, &[1], RecordState::Testing { tip: tip.clone() });
    fake.set_ref(&batch_ref(9), &tip);
    let (mut forge, _send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    assert_eq!(
        resolved(&mut forge, Hint::Commit(heads[0].clone()), 2),
        format!(
            "dorfq: hint commit {}: pull request #1 due in 3 s (read less than 5 s ago)",
            heads[0]
        )
    );
    assert_eq!(
        resolved(&mut forge, Hint::Commit(heads[0].clone()), 10),
        format!("dorfq: hint commit {}: pull request #1 due now", heads[0])
    );
    assert_eq!(
        resolved(&mut forge, Hint::Commit(tip.clone()), 10),
        format!("dorfq: hint commit {tip}: batch 9 due now")
    );
    let elsewhere = fake.commit(&[fake.trunk()]);
    assert_eq!(
        resolved(&mut forge, Hint::Commit(elsewhere.clone()), 10),
        format!(
            "dorfq: hint commit {elsewhere}: matches no cached pull request head or \
             testing batch tip; dropped"
        )
    );
}

#[test]
fn a_commit_hint_naming_several_entities_writes_a_line_for_each() {
    let resolution = Resolution {
        hint: Hint::Commit(Sha::parse(&"a".repeat(40)).unwrap()),
        now: Timestamp(100),
        made: Made::Commit(vec![
            (Entity::Pr(PrNumber(3)), Timestamp(100)),
            (Entity::Batch(BatchId(4)), Timestamp(102)),
        ]),
    };
    let sha = "a".repeat(40);
    assert_eq!(
        resolution.to_string(),
        format!(
            "dorfq: hint commit {sha}: pull request #3 due now\n\
             dorfq: hint commit {sha}: batch 4 due in 2 s (read less than 5 s ago)"
        )
    );
}

#[test]
fn a_resolved_trunk_hint_says_when_trunk_is_due() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    assert_eq!(
        resolved(&mut forge, Hint::Trunk, 1),
        "dorfq: hint trunk: due in 4 s (read less than 5 s ago)"
    );
    assert_eq!(
        resolved(&mut forge, Hint::Trunk, 10),
        "dorfq: hint trunk: due now"
    );
}

#[test]
fn a_hold_is_a_backoff_a_rate_limit_pause_or_nothing_due_sooner() {
    let t = Timestamp;
    // A wait that began at 0 with no backoff and no comment kept.
    let idle = Holds {
        start: t(0),
        held: t(0),
        outbox_due: None,
        status_due: None,
    };
    // A backoff that is what the wake is.
    assert_eq!(
        hold(
            t(10),
            None,
            Holds {
                held: t(20),
                ..idle
            }
        ),
        WaitHold::Backoff
    );
    assert_eq!(
        hold(
            t(20),
            None,
            Holds {
                held: t(20),
                ..idle
            }
        ),
        WaitHold::Backoff
    );
    // A pause holding the earliest deadline back, with no backoff.
    assert_eq!(hold(t(50), Some(t(50)), idle), WaitHold::RateLimit);
    // Both: the later decides.
    assert_eq!(
        hold(
            t(30),
            Some(t(30)),
            Holds {
                held: t(40),
                ..idle
            }
        ),
        WaitHold::Backoff
    );
    assert_eq!(
        hold(
            t(50),
            Some(t(50)),
            Holds {
                held: t(40),
                ..idle
            }
        ),
        WaitHold::RateLimit
    );
    // A pause that has ended, or none, and no backoff.
    assert_eq!(hold(t(50), Some(t(10)), idle), WaitHold::Floor);
    assert_eq!(hold(t(50), None, idle), WaitHold::Floor);
}

#[test]
fn a_hold_line_names_the_wait_and_its_reason() {
    assert_eq!(
        hold_line(42, WaitHold::RateLimit),
        "dorfq: next tick in 42 s: rate-limit pause"
    );
    assert_eq!(
        hold_line(5, WaitHold::Backoff),
        "dorfq: next tick in 5 s: backing off after a transient failure"
    );
    assert_eq!(
        hold_line(3, WaitHold::Floor),
        "dorfq: next tick in 3 s: nothing due sooner"
    );
}

#[test]
fn wait_reads_the_clock_again_when_a_hint_arrives() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    // Observe at 0, then: the wait's start and its loop's first reading at
    // 1, where nothing is due before trunk at 30; the hint arrives at once
    // and is resolved at a reading of its own, 20; the loop's next reading
    // sees what is due and returns.
    let (clock, taken) = scripted(&[0, 1, 1, 20, 20]);
    let (mut forge, send) = hinted(&fake, clock);
    forge.observe().unwrap();
    send.send(Hint::Pr(PrNumber(1))).unwrap();
    forge.wait(false);
    assert_eq!(taken.get(), 5);
    // Resolved at 20, the pull request is due at 20; at the loop's reading
    // of 1 the floor would have made it due at 5.
    assert_eq!(forge.schedule.next_deadline(), Some(Timestamp(NOW.0 + 20)));
}

#[test]
fn a_wait_held_after_hints_says_how_long_and_why() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    let at = |offset| Timestamp(NOW.0 + offset);
    // Trunk, due at 30, is the earliest deadline.
    assert_eq!(
        forge
            .held_line(
                at(10),
                Holds {
                    start: at(1),
                    held: at(1),
                    outbox_due: None,
                    status_due: None,
                },
            )
            .as_deref(),
        Some("dorfq: next tick in 20 s: nothing due sooner")
    );
    assert_eq!(
        forge
            .held_line(
                at(10),
                Holds {
                    start: at(1),
                    held: at(61),
                    outbox_due: None,
                    status_due: None,
                },
            )
            .as_deref(),
        Some("dorfq: next tick in 51 s: backing off after a transient failure")
    );
    assert_eq!(
        forge.held_line(
            at(30),
            Holds {
                start: at(1),
                held: at(1),
                outbox_due: None,
                status_due: None,
            },
        ),
        None
    );
    forge.schedule.pause(at(100));
    assert_eq!(
        forge
            .held_line(
                at(10),
                Holds {
                    start: at(1),
                    held: at(1),
                    outbox_due: None,
                    status_due: None,
                },
            )
            .as_deref(),
        Some("dorfq: next tick in 90 s: rate-limit pause")
    );
}

#[test]
fn only_a_first_read_that_finds_a_pull_request_not_queued_writes_a_line() {
    assert_eq!(
        not_queued_line(PrNumber(7), false, false, "queue").as_deref(),
        Some(r#"dorfq: pull request #7: read; not open and labelled "queue"; not queued"#)
    );
    assert_eq!(not_queued_line(PrNumber(7), true, false, "queue"), None);
    assert_eq!(not_queued_line(PrNumber(7), false, true, "queue"), None);
    assert_eq!(not_queued_line(PrNumber(7), true, true, "queue"), None);
}

#[test]
fn a_hint_for_a_pull_request_that_left_the_queue_reads_it_once_more() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    // The label goes, and the observation that reads it drops the pull
    // request from the cache.
    fake.pr(1).labels.clear();
    forge.resolve(Hint::Pr(PrNumber(1)), Timestamp(NOW.0 + 5));
    let (observation, _) = calls_observing(&fake, &mut forge, &clock, 5);
    assert_eq!(
        observation
            .departed
            .iter()
            .map(|departure| departure.pr.0)
            .collect::<Vec<_>>(),
        [1]
    );
    // The `unlabeled` delivery's hint, drained after that observation,
    // finds it uncached; the next observation reads it and reports nothing.
    assert_eq!(
        resolved(&mut forge, Hint::Pr(PrNumber(1)), 6),
        "dorfq: hint pull request #1: not cached; due now"
    );
    let (observation, calls) = calls_observing(&fake, &mut forge, &clock, 6);
    assert_eq!(calls, ["get_pr 1"]);
    assert!(observation.prs.is_empty());
    assert!(observation.departed.is_empty());
}

/// Moves every deadline of a forge that observed pull request 1 to its
/// interval after `offset` seconds past [`NOW`], so nothing is due for a
/// while.
fn quiet_after(forge: &mut GitHubForge, offset: u64) {
    let at = Timestamp(NOW.0 + offset);
    for entity in [Entity::Listing, Entity::Trunk, Entity::Pr(PrNumber(1))] {
        forge.schedule.refreshed(entity, at, 1, 0);
    }
}

/// Fails the next comment post, and has pull request 1 notified, so the
/// outbox keeps the notice.
fn kept_notice(fake: &FakeGitHub, forge: &mut GitHubForge) {
    fake.fail("create_comment", Failure::Transient);
    forge.notify(PrNumber(1), &BROKEN).unwrap();
    assert_eq!(forge.outbox.len(), 1);
}

#[test]
fn a_kept_comment_holds_the_wait_backing_off_to_a_minute_and_an_empty_outbox_resets() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let elsewhere = fake.commit(&[fake.trunk()]);
    // Each held wait reads its start, a second before its hold ends, the
    // same second again for the hint that arrives, and the hold's end. The
    // hint matches nothing, so the outbox's hold, not a deadline, decides
    // when the wait returns.
    let holds = [(0, 5), (5, 15), (15, 35), (35, 75), (75, 135), (135, 195)];
    let mut readings = vec![0];
    for (start, end) in holds {
        readings.extend([start, end - 1, end - 1, end]);
    }
    // The wait after the outbox emptied, ended by an uncached pull
    // request's hint; the observation after it; a new kept comment's hold.
    readings.extend([200, 200, 201, 201, 201, 202, 206, 206, 207]);
    let (clock, taken) = scripted(&readings);
    let (mut forge, send) = hinted(&fake, clock);
    forge.observe().unwrap();
    quiet_after(&mut forge, 1000);
    kept_notice(&fake, &mut forge);
    let mut expected = 1;
    for (start, end) in holds {
        send.send(Hint::Commit(elsewhere.clone())).unwrap();
        forge.wait(false);
        expected += 4;
        assert_eq!(taken.get(), expected, "the hold from {start} to {end}");
        assert_eq!(forge.outbox_backoff, Some(end - start));
    }
    forge.flush().unwrap();
    assert!(forge.outbox.is_empty());
    send.send(Hint::Pr(PrNumber(2))).unwrap();
    forge.wait(false);
    assert_eq!(taken.get(), expected + 4);
    assert_eq!(forge.outbox_backoff, None);
    fake.open_pr(2);
    forge.observe().unwrap();
    kept_notice(&fake, &mut forge);
    send.send(Hint::Commit(elsewhere)).unwrap();
    forge.wait(false);
    assert_eq!(taken.get(), expected + 9);
    assert_eq!(forge.outbox_backoff, Some(5));
}

#[test]
fn a_rate_limit_pause_holds_a_kept_comment_s_retry_to_its_end() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let elsewhere = fake.commit(&[fake.trunk()]);
    // Observe at 0; the wait's start, a second before the pause ends, the
    // hint's own reading, the pause's end.
    let (clock, taken) = scripted(&[0, 0, 19, 19, 20]);
    let (mut forge, send) = hinted(&fake, clock);
    forge.observe().unwrap();
    quiet_after(&mut forge, 1000);
    kept_notice(&fake, &mut forge);
    fake.state().rate_limit_pause = Some(UNIX_EPOCH + StdDuration::from_secs(NOW.0 + 20));
    send.send(Hint::Commit(elsewhere)).unwrap();
    forge.wait(false);
    assert_eq!(taken.get(), 5);
    let at = |offset| Timestamp(NOW.0 + offset);
    assert_eq!(
        forge
            .held_line(
                at(19),
                Holds {
                    start: at(0),
                    held: at(0),
                    outbox_due: Some(at(20)),
                    status_due: None,
                },
            )
            .as_deref(),
        Some("dorfq: next tick in 1 s: rate-limit pause")
    );
}

#[test]
fn a_hint_that_makes_something_due_ends_a_kept_comment_s_hold() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    // Observe at 0; the wait's start, its loop's reading inside the hold,
    // the hint's own reading, and the loop's next reading, which sees the
    // hinted pull request due.
    let (clock, taken) = scripted(&[0, 0, 1, 1, 1]);
    let (mut forge, send) = hinted(&fake, clock);
    forge.observe().unwrap();
    quiet_after(&mut forge, 1000);
    kept_notice(&fake, &mut forge);
    send.send(Hint::Pr(PrNumber(2))).unwrap();
    forge.wait(false);
    assert_eq!(taken.get(), 5);
    assert_eq!(forge.outbox_backoff, Some(5));
}

#[test]
fn a_retry_tick_posts_the_kept_comment_and_reads_nothing() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _send, clock) = hinted_at(&fake);
    let (before, _) = calls_observing(&fake, &mut forge, &clock, 0);
    kept_notice(&fake, &mut forge);
    let (mut after, calls) = calls_observing(&fake, &mut forge, &clock, 1);
    assert_eq!(calls, ["create_comment 1"]);
    assert!(forge.outbox.is_empty());
    after.now = before.now;
    assert_eq!(after, before);
}

#[test]
fn a_kept_comment_s_retry_is_a_hold_of_its_own_below_a_backoff_and_a_pause() {
    let t = Timestamp;
    // A wait that began at 0 with no backoff and no comment kept.
    let idle = Holds {
        start: t(0),
        held: t(0),
        outbox_due: None,
        status_due: None,
    };
    // The retry is sooner than any deadline.
    assert_eq!(
        hold(
            t(30),
            None,
            Holds {
                outbox_due: Some(t(5)),
                ..idle
            }
        ),
        WaitHold::Outbox
    );
    assert_eq!(
        hold(
            t(30),
            None,
            Holds {
                held: t(10),
                outbox_due: Some(t(20)),
                ..idle
            }
        ),
        WaitHold::Outbox
    );
    // The retry is no sooner than the earliest deadline.
    assert_eq!(
        hold(
            t(30),
            None,
            Holds {
                outbox_due: Some(t(30)),
                ..idle
            }
        ),
        WaitHold::Floor
    );
    assert_eq!(
        hold(
            t(30),
            None,
            Holds {
                outbox_due: Some(t(40)),
                ..idle
            }
        ),
        WaitHold::Floor
    );
    // A backoff or a pause at or past the retry decides.
    assert_eq!(
        hold(
            t(30),
            None,
            Holds {
                held: t(10),
                outbox_due: Some(t(5)),
                ..idle
            }
        ),
        WaitHold::Backoff
    );
    assert_eq!(
        hold(
            t(30),
            Some(t(20)),
            Holds {
                outbox_due: Some(t(20)),
                ..idle
            }
        ),
        WaitHold::RateLimit
    );
    assert_eq!(
        hold_line(5, WaitHold::Outbox),
        "dorfq: next tick in 5 s: retrying a comment post"
    );
}

#[test]
fn a_wait_held_for_a_kept_comment_says_so() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _send, clock) = hinted_at(&fake);
    calls_observing(&fake, &mut forge, &clock, 0);
    let at = |offset| Timestamp(NOW.0 + offset);
    // Trunk, due at 30, is the earliest deadline.
    assert_eq!(
        forge
            .held_line(
                at(0),
                Holds {
                    start: at(0),
                    held: at(0),
                    outbox_due: Some(at(5)),
                    status_due: None,
                },
            )
            .as_deref(),
        Some("dorfq: next tick in 5 s: retrying a comment post")
    );
}

/// A forge over `fake`, with the status issue 12 and a hint channel,
/// reading `clock`.
fn status_hinted(fake: &FakeGitHub, clock: Clock) -> (GitHubForge, Sender<Hint>) {
    fake.state().issues.insert(12, String::new());
    let (send, receive) = mpsc::channel();
    let settings = Settings {
        status_issue: Some(12),
        ..settings()
    };
    let forge = GitHubForge::new(
        Box::new(fake.clone()),
        settings,
        clock,
        Box::new(io::sink()),
        Some(receive),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    (forge, send)
}

#[test]
fn a_kept_status_body_holds_the_wait_and_the_next_observation_writes_it() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let elsewhere = fake.commit(&[fake.trunk()]);
    // Observe at 0; the refused write sent and returned at 0; the wait's
    // start, a second before the retry, the hint's own reading, the retry;
    // the observation and the write sent and returned at 5.
    let (clock, taken) = scripted(&[0, 0, 0, 0, 4, 4, 5, 5, 5, 5]);
    let (mut forge, send) = status_hinted(&fake, clock);
    forge.observe().unwrap();
    quiet_after(&mut forge, 1000);
    fake.fail("set_issue_body", Failure::Transient);
    forge.publish_status("status".to_owned());
    assert_eq!(taken.get(), 3);
    send.send(Hint::Commit(elsewhere)).unwrap();
    forge.wait(false);
    assert_eq!(taken.get(), 7);
    forge.observe().unwrap();
    assert_eq!(taken.get(), 10);
    assert_eq!(fake.state().issues[&12], "status");
}

#[test]
fn a_status_write_s_retry_and_floor_are_timed_from_its_return() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    // Observe at 0; a refused write sent at 0 that returns at 30; a write
    // sent at 35 that returns at 60.
    let (clock, taken) = scripted(&[0, 0, 30, 35, 60]);
    let (mut forge, _send) = status_hinted(&fake, clock);
    forge.observe().unwrap();
    fake.fail("set_issue_body", Failure::Transient);
    forge.publish_status("first".to_owned());
    let status = forge.status.as_ref().unwrap();
    assert_eq!(
        (status.backoff, status.due),
        (Some(5), Timestamp(NOW.0 + 35))
    );
    forge.publish_status("second".to_owned());
    assert_eq!(taken.get(), 5);
    let status = forge.status.as_ref().unwrap();
    assert_eq!(
        (status.pending.as_deref(), status.due),
        (None, Timestamp(NOW.0 + 90))
    );
}

#[test]
fn a_rate_limit_pause_holds_a_kept_status_body_s_retry_to_its_end() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let elsewhere = fake.commit(&[fake.trunk()]);
    // Observe at 0; the refused write sent and returned at 0; the wait's
    // start, a second before the pause ends, the hint's own reading, the
    // pause's end.
    let (clock, taken) = scripted(&[0, 0, 0, 0, 19, 19, 20]);
    let (mut forge, send) = status_hinted(&fake, clock);
    forge.observe().unwrap();
    quiet_after(&mut forge, 1000);
    fake.fail("set_issue_body", Failure::Transient);
    forge.publish_status("status".to_owned());
    fake.state().rate_limit_pause = Some(UNIX_EPOCH + StdDuration::from_secs(NOW.0 + 20));
    send.send(Hint::Commit(elsewhere)).unwrap();
    forge.wait(false);
    assert_eq!(taken.get(), 7);
}

#[test]
fn a_body_handed_over_inside_a_pause_waits_for_its_end() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let elsewhere = fake.commit(&[fake.trunk()]);
    // Observe at 0; the held write at 0; the wait's start, a second before
    // the pause ends, the hint's own reading, the pause's end; the
    // observation, the held write's due check, its pause gate and its return
    // at 20.
    let (clock, taken) = scripted(&[0, 0, 0, 19, 19, 20, 20, 20, 20, 20]);
    let (mut forge, send) = status_hinted(&fake, clock);
    forge.observe().unwrap();
    quiet_after(&mut forge, 1000);
    // The rate-limited tick's response recorded the pause; nothing has
    // read it yet.
    fake.state().rate_limit_pause = Some(UNIX_EPOCH + StdDuration::from_secs(NOW.0 + 20));
    forge.publish_status("status".to_owned());
    assert_eq!(fake.writes(), ["set_issue_body 12"]);
    send.send(Hint::Commit(elsewhere)).unwrap();
    forge.wait(false);
    assert_eq!(taken.get(), 6);
    assert_eq!(fake.writes(), ["set_issue_body 12"]);
    forge.observe().unwrap();
    assert_eq!(taken.get(), 10);
    assert_eq!(fake.writes(), ["set_issue_body 12", "set_issue_body 12"]);
    assert_eq!(fake.state().issues[&12], "status");
}

#[test]
fn a_held_status_body_is_a_hold_of_its_own_after_a_kept_comment() {
    let t = Timestamp;
    // A wait that began at 0 with no backoff, no comment kept and no body
    // held.
    let idle = Holds {
        start: t(0),
        held: t(0),
        outbox_due: None,
        status_due: None,
    };
    // The write is sooner than any deadline.
    assert_eq!(
        hold(
            t(30),
            None,
            Holds {
                status_due: Some(t(5)),
                ..idle
            }
        ),
        WaitHold::Status
    );
    // The write is no sooner than the earliest deadline.
    assert_eq!(
        hold(
            t(30),
            None,
            Holds {
                status_due: Some(t(30)),
                ..idle
            }
        ),
        WaitHold::Floor
    );
    // Of a kept comment's retry and the write, the sooner names the hold;
    // at the same time, the comment's retry does.
    assert_eq!(
        hold(
            t(30),
            None,
            Holds {
                outbox_due: Some(t(20)),
                status_due: Some(t(5)),
                ..idle
            }
        ),
        WaitHold::Status
    );
    assert_eq!(
        hold(
            t(30),
            None,
            Holds {
                outbox_due: Some(t(5)),
                status_due: Some(t(20)),
                ..idle
            }
        ),
        WaitHold::Outbox
    );
    assert_eq!(
        hold(
            t(30),
            None,
            Holds {
                outbox_due: Some(t(5)),
                status_due: Some(t(5)),
                ..idle
            }
        ),
        WaitHold::Outbox
    );
    // A backoff or a pause at or past the write decides.
    assert_eq!(
        hold(
            t(30),
            None,
            Holds {
                held: t(10),
                status_due: Some(t(5)),
                ..idle
            }
        ),
        WaitHold::Backoff
    );
    assert_eq!(
        hold(
            t(30),
            Some(t(20)),
            Holds {
                status_due: Some(t(20)),
                ..idle
            }
        ),
        WaitHold::RateLimit
    );
    // The wake is the earliest of the three.
    assert_eq!(
        Holds {
            outbox_due: Some(t(20)),
            status_due: Some(t(5)),
            ..idle
        }
        .wake(t(30)),
        t(5)
    );
    assert_eq!(
        hold_line(5, WaitHold::Status),
        "dorfq: next tick in 5 s: writing the status issue"
    );
}
