//! What each observation reads, under a clock the test sets.

use super::*;

/// A forge over `fake` whose clock reads [`NOW`] until the returned cell
/// moves it.
fn timed(fake: &FakeGitHub) -> (GitHubForge, Buffer, Rc<Cell<u64>>) {
    let (clock, now) = set_clock();
    let (forge, sink) = forge_with_clock(fake, clock);
    (forge, sink, now)
}

fn call_count(fake: &FakeGitHub) -> usize {
    fake.state().calls.len()
}

/// Every call made since `since` calls had been.
fn calls_since(fake: &FakeGitHub, since: usize) -> Vec<String> {
    fake.state().calls[since..].to_vec()
}

/// The pull requests read since `since` calls had been, by number.
fn prs_read_since(fake: &FakeGitHub, since: usize) -> Vec<u64> {
    prs_read(&calls_since(fake, since))
}

/// The pull requests `calls` read, by number.
fn prs_read(calls: &[String]) -> Vec<u64> {
    calls
        .iter()
        .filter_map(|c| c.strip_prefix("get_pr "))
        .map(|n| n.parse().unwrap())
        .collect()
}

/// Observes at `now` seconds after [`NOW`]; the calls it made.
fn observe_at(
    fake: &FakeGitHub,
    forge: &mut GitHubForge,
    clock: &Cell<u64>,
    now: u64,
) -> (Observation, Vec<String>) {
    clock.set(NOW.0 + now);
    let before = call_count(fake);
    let observation = forge.observe().unwrap();
    (observation, calls_since(fake, before))
}

#[test]
fn the_first_observation_reads_the_listing_then_trunk_then_each_pull_request() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 2);
    let (mut forge, _, clock) = timed(&fake);
    let (observation, calls) = observe_at(&fake, &mut forge, &clock, 0);
    assert_eq!(observation.prs.len(), 2);
    assert_eq!(
        calls,
        [
            "list_labelled queue".to_owned(),
            "list_refs refs/dorfq/batches/".to_owned(),
            "get_ref refs/heads/main".to_owned(),
            "get_pr 1".to_owned(),
            format!("checks {}", heads[0]),
            "get_pr 2".to_owned(),
            format!("checks {}", heads[1]),
            format!("compare {} {}", heads[0], fake.trunk()),
            format!("compare {} {}", heads[1], fake.trunk()),
            // Each is greeted unless its thread shows it was.
            "list_comments 1".to_owned(),
            "list_comments 2".to_owned(),
        ]
    );
}

#[test]
fn an_observation_with_nothing_due_makes_no_call_and_reports_the_same() {
    let fake = FakeGitHub::new();
    queued(&fake, 2);
    stacked(&fake, 3, 2);
    // Blocked on a plain branch: its thread is read once.
    fake.open_pr(4);
    fake.set_ref(&branch("release"), &fake.trunk());
    fake.pr(4).base = "release".to_owned();
    let tip = tip_over(&fake, &[1]);
    put_record(&fake, 9, &[1], RecordState::Testing { tip: tip.clone() });
    fake.set_ref(&batch_ref(9), &tip);
    let (mut forge, _, clock) = timed(&fake);
    let (first, _) = observe_at(&fake, &mut forge, &clock, 0);
    forge.flush().unwrap();

    let (second, calls) = observe_at(&fake, &mut forge, &clock, BATCH_SECS - 1);

    assert_eq!(calls, Vec::<String>::new());
    assert_eq!(second.prs, first.prs);
    assert_eq!(second.batches, first.batches);
    assert_eq!(second.now, Timestamp(NOW.0 + BATCH_SECS - 1));
}

#[test]
fn trunk_and_testing_batches_are_due_on_the_batch_interval() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let tip = tip_over(&fake, &[1]);
    put_record(&fake, 9, &[1], RecordState::Testing { tip: tip.clone() });
    fake.set_ref(&batch_ref(9), &tip);
    let (mut forge, _, clock) = timed(&fake);
    observe_at(&fake, &mut forge, &clock, 0);
    passed(&fake, &tip);

    let (observation, calls) = observe_at(&fake, &mut forge, &clock, BATCH_SECS);

    assert_eq!(
        calls,
        [
            "get_ref refs/heads/main".to_owned(),
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
fn only_trunk_due_and_unmoved_reads_trunk_alone() {
    let fake = FakeGitHub::new();
    queued(&fake, 2);
    let (mut forge, _, clock) = timed(&fake);
    observe_at(&fake, &mut forge, &clock, 0);
    let (_, calls) = observe_at(&fake, &mut forge, &clock, BATCH_SECS);
    assert_eq!(calls, ["get_ref refs/heads/main"]);
}

#[test]
fn a_pull_request_due_alone_is_read_alone() {
    let fake = FakeGitHub::new();
    queued(&fake, 2);
    let (mut forge, _, clock) = timed(&fake);
    observe_at(&fake, &mut forge, &clock, 0);
    let head = fake.commit(&[fake.trunk()]);
    fake.push(2, &head);
    passed(&fake, &head);
    forge.mark_due(Entity::Pr(PrNumber(2)));

    let (observation, calls) = observe_at(&fake, &mut forge, &clock, 1);

    assert_eq!(
        calls,
        [
            "get_pr 2".to_owned(),
            format!("checks {head}"),
            // The new head's staleness.
            format!("compare {head} {}", fake.trunk()),
        ]
    );
    assert_eq!(observation.prs[1].head, head);
}

#[test]
fn a_base_read_at_a_new_head_holds_its_unread_child_by_one_compare() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let child = stacked(&fake, 2, 1);
    let (mut forge, _, clock) = timed(&fake);
    observe_at(&fake, &mut forge, &clock, 0);
    let base_head = fake.pr(1).head.clone();
    let moved = fake.commit(&[base_head]);
    fake.push(1, &moved);
    passed(&fake, &moved);
    forge.mark_due(Entity::Pr(PrNumber(1)));

    let (observation, calls) = observe_at(&fake, &mut forge, &clock, 1);

    assert_eq!(
        calls,
        [
            "get_pr 1".to_owned(),
            format!("checks {moved}"),
            // The new head's staleness, then whether it is still the
            // child's ancestor.
            format!("compare {moved} {}", fake.trunk()),
            format!("is_ancestor {moved} {child}"),
            // The child's author is told of the hold in the same pass.
            "create_comment 2".to_owned(),
        ]
    );
    assert_eq!(
        observation.prs[1].status,
        waiting(&[Hold::AncestorMoved { pr: PrNumber(1) }])
    );
    // The pair is read once, ever.
    forge.mark_due(Entity::Pr(PrNumber(1)));
    let (_, calls) = observe_at(&fake, &mut forge, &clock, 2);
    assert_eq!(calls, ["get_pr 1".to_owned(), format!("checks {moved}")]);
}

#[test]
fn unknown_mergeability_is_read_again_at_the_floor_and_a_settled_one_at_the_interval() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    fake.pr(1).merge_state = MergeState::Unknown;
    let (mut forge, _, clock) = timed(&fake);
    let (observation, _) = observe_at(&fake, &mut forge, &clock, 0);
    assert_eq!(
        observation.prs[0].status,
        waiting(&[Hold::MergeabilityUnknown])
    );

    let (observation, calls) = observe_at(&fake, &mut forge, &clock, 4);
    assert!(prs_read(&calls).is_empty(), "{calls:?}");
    assert_eq!(
        observation.prs[0].status,
        waiting(&[Hold::MergeabilityUnknown])
    );

    let (observation, calls) = observe_at(&fake, &mut forge, &clock, 5);
    assert_eq!(prs_read(&calls), [1]);
    assert_eq!(
        observation.prs[0].status,
        waiting(&[Hold::MergeabilityUnknown])
    );

    fake.pr(1).merge_state = MergeState::Clean;
    let (observation, calls) = observe_at(&fake, &mut forge, &clock, 10);
    assert_eq!(prs_read(&calls), [1]);
    assert_eq!(observation.prs[0].status, PrStatus::Ready);

    let (_, calls) = observe_at(&fake, &mut forge, &clock, 10 + PR_SECS - 1);
    assert!(prs_read(&calls).is_empty(), "{calls:?}");
    let (_, calls) = observe_at(&fake, &mut forge, &clock, 10 + PR_SECS);
    assert_eq!(prs_read(&calls), [1]);
}

#[test]
fn pull_requests_read_together_come_due_spread_and_stay_spread() {
    let fake = FakeGitHub::new();
    queued(&fake, 3);
    let (mut forge, _, clock) = timed(&fake);
    observe_at(&fake, &mut forge, &clock, 0);
    // Due at PR_SECS, then a third and two thirds of it later.
    let third = PR_SECS / 3;
    for (at, read) in [
        (PR_SECS, vec![1]),
        (PR_SECS + third - 1, vec![]),
        (PR_SECS + third, vec![2]),
        (PR_SECS + 2 * third, vec![3]),
        (2 * PR_SECS, vec![1]),
        (2 * PR_SECS + third, vec![2]),
    ] {
        let before = call_count(&fake);
        clock.set(NOW.0 + at);
        forge.observe().unwrap();
        assert_eq!(prs_read_since(&fake, before), read, "at {at}");
    }
}

#[test]
fn the_listing_omitting_cached_pull_requests_reads_them_in_the_same_pass() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 3);
    let (mut forge, _, clock) = timed(&fake);
    observe_at(&fake, &mut forge, &clock, 0);
    // 1 falls between two pages; 2 is unlabelled; 3 is listed and not due.
    fake.state().unlisted.insert(1);
    fake.pr(2).labels.clear();

    let (observation, calls) = observe_at(&fake, &mut forge, &clock, LISTING_SECS);

    assert_eq!(prs_read(&calls), [1, 2]);
    assert_eq!(
        observation
            .prs
            .iter()
            .map(|p| p.number.0)
            .collect::<Vec<_>>(),
        [1, 3]
    );
    assert_eq!(
        observation.departed,
        [Departure::new(
            PrNumber(2),
            heads[1].clone(),
            Departed::Withdrawn(Withdrawal::IntentCleared),
        )]
    );
    // 2 left the cache and its schedule: nothing reads it again.
    assert!(!forge.schedule.tracks(Entity::Pr(PrNumber(2))));
}

#[test]
fn a_pull_request_newly_listed_is_read_and_reported_in_the_same_pass() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _, clock) = timed(&fake);
    observe_at(&fake, &mut forge, &clock, 0);
    ready_pr(&fake, 2);

    let (observation, calls) = observe_at(&fake, &mut forge, &clock, LISTING_SECS);

    assert_eq!(prs_read(&calls), [2]);
    assert_eq!(observation.prs.len(), 2);
}

#[test]
fn a_transient_failure_keeps_the_refreshes_before_it_and_the_retry_reads_the_rest() {
    let fake = FakeGitHub::new();
    queued(&fake, 5);
    let (mut forge, _, clock) = timed(&fake);
    observe_at(&fake, &mut forge, &clock, 0);
    for n in 1..=5 {
        forge.mark_due(Entity::Pr(PrNumber(n)));
    }
    fake.fail_on("get_pr", "3", Failure::Transient);
    clock.set(NOW.0 + 1);
    let before = call_count(&fake);
    assert!(matches!(forge.observe(), Err(ForgeError::Transient(_))));
    assert_eq!(prs_read_since(&fake, before), [1, 2, 3]);

    let (observation, calls) = observe_at(&fake, &mut forge, &clock, 1);

    assert_eq!(prs_read(&calls), [3, 4, 5]);
    assert_eq!(observation.prs.len(), 5);
}

#[test]
fn a_put_back_member_is_read_on_its_base_in_the_same_pass() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    stacked(&fake, 2, 1);
    let (mut forge, _, clock) = timed(&fake);
    observe_at(&fake, &mut forge, &clock, 0);
    // A landing of 1 and 2 that moved 2 onto trunk and died before the
    // fast-forward, written behind the queue's back.
    let tip = tip_over(&fake, &[1, 2]);
    put_record(&fake, 9, &[1, 2], RecordState::Landing { tip: tip.clone() });
    let record_ref = "refs/dorfq/batches/9";
    let sha = fake.get(record_ref).unwrap();
    let message = fake.state().commits[&sha].message.clone();
    let mut record = Record::parse(record_ref, BatchId(9), &message).unwrap();
    record.moved = vec![Moved {
        pr: PrNumber(2),
        base: PrNumber(1),
        branch: "pr-1".to_owned(),
    }];
    fake.state().commits.get_mut(&sha).unwrap().message = record.render().unwrap();
    fake.set_ref(&batch_ref(9), &tip);
    fake.pr(2).base = "main".to_owned();

    let (observation, calls) = observe_at(&fake, &mut forge, &clock, LISTING_SECS);

    let set_base = calls.iter().position(|c| c == "set_base 2 pr-1").unwrap();
    let read = calls.iter().rposition(|c| c == "get_pr 2").unwrap();
    assert!(set_base < read, "{calls:?}");
    assert_eq!(observation.prs[1].base, PrBase::Pr(PrNumber(1)));
}

#[test]
fn a_retarget_onto_trunk_makes_the_pull_request_due() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    stacked(&fake, 2, 1);
    let (mut forge, _, clock) = timed(&fake);
    observe_at(&fake, &mut forge, &clock, 0);
    land_batch(&fake, &mut forge, 1, &[1]);
    assert!(
        !forge
            .schedule
            .due(Timestamp(NOW.0))
            .contains(&Entity::Pr(PrNumber(2)))
    );

    let before = call_count(&fake);
    let observation = forge.observe().unwrap();
    assert!(calls_since(&fake, before).contains(&"set_base 2 main".to_owned()));
    assert_eq!(prs_read_since(&fake, before), [1]);
    assert_eq!(observation.prs[0].base, PrBase::Trunk);

    let before = call_count(&fake);
    forge.observe().unwrap();
    assert_eq!(prs_read_since(&fake, before), [2]);
}

#[test]
fn each_effect_makes_the_listing_and_what_it_names_due() {
    let fake = FakeGitHub::new();
    queued(&fake, 3);
    let (mut forge, _, clock) = timed(&fake);
    observe_at(&fake, &mut forge, &clock, 0);
    let due = |forge: &GitHubForge| forge.schedule.due(Timestamp(NOW.0));

    forge
        .start_batch(&start(1, &fake.trunk(), &[1, 2]))
        .unwrap();
    assert_eq!(
        due(&forge),
        [
            Entity::Listing,
            Entity::Batch(BatchId(1)),
            Entity::Pr(PrNumber(1)),
            Entity::Pr(PrNumber(2)),
        ]
    );
    let tip = fake.get(&batch_ref(1)).unwrap();
    passed(&fake, &tip);
    let (_, calls) = observe_at(&fake, &mut forge, &clock, 0);
    assert_eq!(prs_read(&calls), [1, 2]);
    assert!(due(&forge).is_empty());

    forge.land(BatchId(1)).unwrap();
    assert_eq!(
        due(&forge),
        [
            Entity::Listing,
            Entity::Trunk,
            Entity::Batch(BatchId(1)),
            Entity::Pr(PrNumber(1)),
            Entity::Pr(PrNumber(2)),
        ]
    );
    observe_at(&fake, &mut forge, &clock, 0);

    forge.retire(BatchId(1)).unwrap();
    assert_eq!(due(&forge), [Entity::Listing, Entity::Batch(BatchId(1))]);
    observe_at(&fake, &mut forge, &clock, 0);
    assert!(!forge.schedule.tracks(Entity::Batch(BatchId(1))));

    forge.eject(PrNumber(3), &CONFLICTS_WITH_TRUNK).unwrap();
    assert_eq!(due(&forge), [Entity::Listing, Entity::Pr(PrNumber(3))]);
}

#[test]
fn a_start_whose_record_write_was_lost_is_seen_by_the_next_observation() {
    let fake = FakeGitHub::new();
    queued(&fake, 2);
    let (mut forge, _, clock) = timed(&fake);
    observe_at(&fake, &mut forge, &clock, 0);
    fake.state().lose_update = Some("refs/dorfq/batches/1".to_owned());
    let error = forge
        .start_batch(&start(1, &fake.trunk(), &[1, 2]))
        .unwrap_err();
    assert!(matches!(error, ForgeError::Transient(_)), "{error}");

    let (observation, _) = observe_at(&fake, &mut forge, &clock, 0);

    assert!(matches!(
        observation.batches[0].state,
        BatchState::Testing { .. }
    ));
}

#[test]
fn a_blocked_pull_request_s_thread_is_read_once_per_blocker() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    fake.set_ref(&branch("release"), &fake.trunk());
    fake.set_ref(&branch("hotfix"), &fake.trunk());
    fake.pr(1).base = "release".to_owned();
    let (mut forge, _) = forge(&fake);
    let threads = || fake.calls_to(&["list_comments"]).len();

    forge.observe().unwrap();
    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(threads(), 1);
    assert_eq!(fake.comments(1).len(), 1);

    fake.pr(1).base = "hotfix".to_owned();
    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(threads(), 2);
    assert_eq!(fake.comments(1).len(), 2);
}

#[test]
fn a_zero_interval_is_rejected_naming_it() {
    let fake = FakeGitHub::new();
    let mut bad = settings();
    bad.refresh.pr = Duration::ZERO;
    let error = GitHubForge::new(
        Box::new(fake.clone()),
        bad,
        clock(),
        Box::new(io::sink()),
        None,
    )
    .err()
    .unwrap();
    assert!(
        error.to_string().contains("forge setting refresh.pr_secs"),
        "{error}"
    );
}
