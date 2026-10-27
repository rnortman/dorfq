use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::rc::Rc;

use dorfq_core::{
    BatchId, BatchMember, BatchStart, BatchState, CheckName, CiStatus, ConflictWith, Departed,
    Departure, EjectReason, Forge, ForgeError, Hold, LandRefusal, NoticeReason, Observation,
    PrBase, PrNumber, PrStatus, Sha, Timestamp, Withdrawal,
};

use crate::api::{ApiError, CheckOutcome, CheckResult};
use crate::fake::{Failure, FakeGitHub};
use crate::record::{Record, RecordState};
use crate::{Clock, GitHubForge, Settings, branch};

/// A sink the test can read back.
#[derive(Clone, Default)]
struct Buffer(Rc<RefCell<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Buffer {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.borrow().clone())
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn settings() -> Settings {
    Settings {
        trunk: "main".to_owned(),
        label: "queue".to_owned(),
        checks: vec!["check".to_owned(), "lint".to_owned()],
    }
}

/// The time every test forge's clock reads.
const NOW: Timestamp = Timestamp(1_700_000_000);

fn clock() -> Clock {
    Box::new(|| Ok(NOW))
}

fn forge(fake: &FakeGitHub) -> (GitHubForge, Buffer) {
    forge_with_clock(fake, clock())
}

fn forge_with_clock(fake: &FakeGitHub, clock: Clock) -> (GitHubForge, Buffer) {
    let sink = Buffer::default();
    let forge = GitHubForge::new(
        Box::new(fake.clone()),
        settings(),
        clock,
        Box::new(sink.clone()),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    (forge, sink)
}

fn result(name: &str, outcome: CheckOutcome) -> CheckResult {
    CheckResult {
        name: name.to_owned(),
        outcome,
    }
}

fn passed(fake: &FakeGitHub, sha: &Sha) {
    fake.set_checks(
        sha,
        vec![
            result("check", CheckOutcome::Passed),
            result("lint", CheckOutcome::Passed),
        ],
    );
}

/// An open pull request whose own checks passed; its head.
fn ready_pr(fake: &FakeGitHub, number: u64) -> Sha {
    let head = fake.open_pr(number);
    passed(fake, &head);
    head
}

fn is_permanent(error: &ForgeError, needle: &str) -> bool {
    matches!(error, ForgeError::Permanent(_)) && error.to_string().contains(needle)
}

fn marker(event: &str) -> String {
    format!("<!-- dorfq: {event} -->\nearlier")
}

/// Writes a record for batch `id` over `members` (heads of open pull
/// requests) on base trunk, and points its ref at it; the base.
fn put_record(fake: &FakeGitHub, id: u64, members: &[u64], state: RecordState) -> Sha {
    let base = fake.trunk();
    let record = Record {
        id: BatchId(id),
        base: base.clone(),
        members: members
            .iter()
            .map(|&n| BatchMember::new(PrNumber(n), fake.pr(n).head.clone()))
            .collect(),
        state,
    };
    let sha = fake.commit(std::slice::from_ref(&base));
    fake.state().commits.get_mut(&sha).unwrap().message = record.render().unwrap();
    fake.set_ref(&format!("refs/dorfq/batches/{id}"), &sha);
    base
}

/// A commit merging `heads` onto trunk, as a batch tip would be.
fn tip_over(fake: &FakeGitHub, heads: &[u64]) -> Sha {
    let mut parents = vec![fake.trunk()];
    parents.extend(heads.iter().map(|&n| fake.pr(n).head.clone()));
    fake.commit(&parents)
}

fn batch_ref(id: u64) -> String {
    branch(&format!("dorfq/batch/{id}"))
}

#[test]
fn new_rejects_a_missing_trunk_and_writes_nothing() {
    let fake = FakeGitHub::new();
    fake.state().refs.clear();
    let error = GitHubForge::new(
        Box::new(fake.clone()),
        settings(),
        clock(),
        Box::new(io::sink()),
    )
    .err()
    .unwrap();
    assert!(matches!(error, ApiError::Permanent(_)));
    assert!(error.to_string().contains("refs/heads/main"), "{error}");
    assert!(fake.writes().is_empty());
}

#[test]
fn new_rejects_a_missing_label_and_writes_nothing() {
    let fake = FakeGitHub::new();
    fake.state().labels.clear();
    let error = GitHubForge::new(
        Box::new(fake.clone()),
        settings(),
        clock(),
        Box::new(io::sink()),
    )
    .err()
    .unwrap();
    assert!(matches!(error, ApiError::Permanent(_)));
    assert!(error.to_string().contains("\"queue\""), "{error}");
    assert!(fake.writes().is_empty());
}

#[test]
fn new_rejects_empty_settings_before_any_call() {
    let mut no_trunk = settings();
    no_trunk.trunk.clear();
    let mut no_label = settings();
    no_label.label.clear();
    let mut no_checks = settings();
    no_checks.checks.clear();
    let mut empty_check = settings();
    empty_check.checks.push(String::new());
    for (bad, problem) in [
        (no_trunk, "forge setting trunk is empty"),
        (no_label, "forge setting label is empty"),
        (no_checks, "forge setting checks names no check"),
        (empty_check, "forge setting checks[2] is empty"),
    ] {
        let fake = FakeGitHub::new();
        let error = GitHubForge::new(Box::new(fake.clone()), bad, clock(), Box::new(io::sink()))
            .err()
            .unwrap();
        assert!(matches!(error, ApiError::Permanent(_)));
        assert!(error.to_string().ends_with(problem), "{error}");
        assert!(fake.state().calls.is_empty());
    }
}

#[test]
fn observe_reports_trunk_and_labelled_open_prs_against_trunk_sorted() {
    let fake = FakeGitHub::new();
    let heads: Vec<Sha> = [9, 3, 5, 7, 4]
        .into_iter()
        .map(|n| fake.open_pr(n))
        .collect();
    fake.pr(5).labels = vec!["bug".to_owned()];
    fake.pr(7).open = false;
    fake.pr(4).base = "release".to_owned();
    fake.pr(9).labels.push("bug".to_owned());
    let (mut forge, _) = forge(&fake);

    let observation = forge.observe().unwrap();
    assert_eq!(observation.trunk, fake.trunk());
    let numbers: Vec<u64> = observation.prs.iter().map(|p| p.number.0).collect();
    assert_eq!(numbers, [3, 9]);
    assert_eq!(observation.prs[0].head, heads[1]);
    assert_eq!(observation.prs[1].head, heads[0]);
    assert!(observation.batches.is_empty());
}

fn waiting(holds: &[Hold]) -> PrStatus {
    PrStatus::Waiting(holds.iter().copied().collect())
}

fn statuses(forge: &mut GitHubForge) -> Vec<PrStatus> {
    let observation = forge.observe().unwrap();
    observation.prs.into_iter().map(|p| p.status).collect()
}

#[test]
fn observe_maps_mergeable_and_own_ci_to_status() {
    let fake = FakeGitHub::new();
    let heads: Vec<Sha> = (1..=5).map(|n| fake.open_pr(n)).collect();
    for head in &heads[..3] {
        passed(&fake, head);
    }
    fake.set_checks(&heads[4], vec![result("lint", CheckOutcome::Failed)]);
    fake.pr(2).mergeable = Some(false);
    fake.pr(3).mergeable = None;
    fake.pr(4).mergeable = None;
    fake.pr(5).mergeable = Some(false);
    let (mut forge, _) = forge(&fake);

    assert_eq!(
        statuses(&mut forge),
        [
            PrStatus::Ready,
            PrStatus::Conflicting(ConflictWith::Trunk),
            waiting(&[Hold::MergeabilityUnknown]),
            waiting(&[Hold::MergeabilityUnknown, Hold::OwnCiPending]),
            PrStatus::Conflicting(ConflictWith::Trunk),
        ]
    );
}

#[test]
fn observe_reads_own_ci_from_the_required_names() {
    let fake = FakeGitHub::new();
    let heads: Vec<Sha> = (1..=4).map(|n| fake.open_pr(n)).collect();
    passed(&fake, &heads[0]);
    fake.set_checks(&heads[1], vec![result("check", CheckOutcome::Passed)]);
    fake.set_checks(
        &heads[2],
        vec![
            result("lint", CheckOutcome::Failed),
            result("check", CheckOutcome::Pending),
        ],
    );
    fake.set_checks(
        &heads[3],
        vec![
            result("deploy-preview", CheckOutcome::Failed),
            result("check", CheckOutcome::Passed),
            result("lint", CheckOutcome::Passed),
        ],
    );
    let (mut forge, _) = forge(&fake);

    assert_eq!(
        statuses(&mut forge),
        [
            PrStatus::Ready,
            waiting(&[Hold::OwnCiPending]),
            waiting(&[Hold::OwnCiFailed]),
            PrStatus::Ready,
        ]
    );
}

#[test]
fn observe_without_trunk_is_permanent() {
    let fake = FakeGitHub::new();
    let (mut forge, _) = forge(&fake);
    fake.state().refs.clear();
    let error = forge.observe().unwrap_err();
    assert!(is_permanent(&error, "refs/heads/main"), "{error}");
}

#[test]
fn a_transient_read_fails_the_observation() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    let (mut forge, _) = forge(&fake);
    fake.fail("checks", Failure::Transient);
    assert!(matches!(forge.observe(), Err(ForgeError::Transient(_))));
    assert_eq!(forge.observe().unwrap().prs.len(), 1);
}

#[test]
fn a_started_record_is_cleaned_up_and_reported_as_nothing() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    let base = put_record(&fake, 4, &[1], RecordState::Started);
    for name in ["build", "probe", "batch"] {
        fake.set_ref(&branch(&format!("dorfq/{name}/4")), &base);
    }
    let (mut forge, _) = forge(&fake);

    assert!(forge.observe().unwrap().batches.is_empty());
    let left: Vec<String> = fake
        .state()
        .refs
        .keys()
        .filter(|name| name.contains("dorfq"))
        .cloned()
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn a_started_record_without_branches_is_cleaned_up() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    put_record(&fake, 4, &[1], RecordState::Started);
    let (mut forge, _) = forge(&fake);

    assert!(forge.observe().unwrap().batches.is_empty());
    assert_eq!(fake.get("refs/dorfq/batches/4"), None);
    assert_eq!(
        fake.calls_to(&["delete_ref"]).last().unwrap(),
        "delete_ref refs/dorfq/batches/4"
    );
}

#[test]
fn a_testing_record_at_its_branch_is_testing_with_ci_from_the_tip() {
    let fake = FakeGitHub::new();
    let head = fake.open_pr(1);
    fake.open_pr(2);
    let tip = tip_over(&fake, &[1, 2]);
    let base = put_record(&fake, 3, &[2, 1], RecordState::Testing { tip: tip.clone() });
    fake.set_ref(&batch_ref(3), &tip);
    fake.set_checks(&tip, vec![result("check", CheckOutcome::Passed)]);
    let (mut forge, _) = forge(&fake);

    let observation = forge.observe().unwrap();
    assert_eq!(observation.batches.len(), 1);
    let batch = &observation.batches[0];
    assert_eq!(batch.id, BatchId(3));
    assert_eq!(batch.base, base);
    let members: Vec<u64> = batch.members.iter().map(|m| m.pr.0).collect();
    assert_eq!(members, [2, 1]);
    assert_eq!(batch.members[1].head, head);
    assert_eq!(
        batch.state,
        BatchState::Testing {
            tip: tip.clone(),
            ci: CiStatus::Pending
        }
    );

    passed(&fake, &tip);
    assert_eq!(
        forge.observe().unwrap().batches[0].state,
        BatchState::Testing {
            tip: tip.clone(),
            ci: CiStatus::Passed
        }
    );

    fake.set_checks(
        &tip,
        vec![
            result("check", CheckOutcome::Failed),
            result("lint", CheckOutcome::Failed),
            result("deploy-preview", CheckOutcome::Failed),
        ],
    );
    let names = ["check", "lint"].map(|n| CheckName(n.to_owned()));
    assert_eq!(
        forge.observe().unwrap().batches[0].state,
        BatchState::Testing {
            tip,
            ci: CiStatus::Failed(dorfq_core::Failure::new(BTreeSet::from(names)))
        }
    );
}

#[test]
fn observe_reads_now_from_the_clock_it_was_given() {
    let fake = FakeGitHub::new();
    let time = Rc::new(Cell::new(10));
    let reads = Rc::clone(&time);
    let (mut forge, _) = forge_with_clock(&fake, Box::new(move || Ok(Timestamp(reads.get()))));
    assert_eq!(forge.observe().unwrap().now, Timestamp(10));
    time.set(25);
    assert_eq!(forge.observe().unwrap().now, Timestamp(25));
}

#[test]
fn a_clock_that_cannot_be_read_is_permanent() {
    let fake = FakeGitHub::new();
    let (mut forge, _) = forge_with_clock(&fake, Box::new(|| Err("no clock".to_owned())));
    let error = forge.observe().unwrap_err();
    assert!(
        is_permanent(&error, "reading the clock: no clock"),
        "{error}"
    );
}

#[test]
fn a_testing_batch_whose_tip_is_trunk_is_still_testing() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    // Trunk was moved to the tip by another hand.
    let tip = fake.commit(&[fake.trunk()]);
    put_record(&fake, 1, &[1], RecordState::Testing { tip: tip.clone() });
    fake.set_ref(&batch_ref(1), &tip);
    fake.set_ref(&branch("main"), &tip);
    let (mut forge, _) = forge(&fake);

    assert_eq!(
        forge.observe().unwrap().batches[0].state,
        BatchState::Testing {
            tip,
            ci: CiStatus::Pending
        }
    );
    assert!(fake.calls_to(&["is_ancestor"]).is_empty());
}

#[test]
fn a_moved_or_missing_batch_branch_is_permanent() {
    for landing in [false, true] {
        for moved in [false, true] {
            let fake = FakeGitHub::new();
            fake.open_pr(1);
            let tip = tip_over(&fake, &[1]);
            let state = if landing {
                RecordState::Landing { tip: tip.clone() }
            } else {
                RecordState::Testing { tip: tip.clone() }
            };
            put_record(&fake, 2, &[1], state);
            if moved {
                let other = fake.commit(&[tip]);
                fake.set_ref(&batch_ref(2), &other);
            }
            let (mut forge, _) = forge(&fake);

            let error = forge.observe().unwrap_err();
            let needle = if moved {
                "pushed outside"
            } else {
                "deleted outside"
            };
            assert!(is_permanent(&error, needle), "{error}");
            assert!(error.to_string().contains("refs/heads/dorfq/batch/2"));
        }
    }
}

#[test]
fn a_conflict_record_is_a_conflict_and_stray_branches_are_left_alone() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    fake.open_pr(2);
    let base = put_record(
        &fake,
        5,
        &[1, 2],
        RecordState::Conflict {
            pr: PrNumber(2),
            with: ConflictWith::Pr(PrNumber(1)),
        },
    );
    fake.set_ref(&branch("dorfq/build/5"), &base);
    fake.set_ref(&branch("dorfq/probe/5"), &base);
    let (mut forge, _) = forge(&fake);

    assert_eq!(
        forge.observe().unwrap().batches[0].state,
        BatchState::Conflict {
            pr: PrNumber(2),
            with: ConflictWith::Pr(PrNumber(1))
        }
    );
    // A branch no record points at is never read, and never deleted.
    assert_eq!(fake.get(&branch("dorfq/build/5")), Some(base.clone()));
    assert_eq!(fake.get(&branch("dorfq/probe/5")), Some(base));
    assert!(fake.get("refs/dorfq/batches/5").is_some());
    assert_eq!(fake.calls_to(&["delete_ref"]), Vec::<String>::new());
    let reads = fake.calls_to(&["get_ref"]);
    assert!(
        reads
            .iter()
            .all(|c| !c.contains("dorfq/build/5") && !c.contains("dorfq/probe/5")),
        "{reads:?}"
    );
}

#[test]
fn a_published_branch_for_a_conflict_record_is_permanent() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    let base = put_record(
        &fake,
        5,
        &[1],
        RecordState::Conflict {
            pr: PrNumber(1),
            with: ConflictWith::Trunk,
        },
    );
    fake.set_ref(&batch_ref(5), &base);
    let (mut forge, _) = forge(&fake);

    let error = forge.observe().unwrap_err();
    assert!(is_permanent(&error, "never published"), "{error}");
}

#[test]
fn a_landing_record_is_landed_when_its_tip_is_trunk_or_behind_it() {
    for trunk_moved_on in [false, true] {
        let fake = FakeGitHub::new();
        fake.open_pr(1);
        let tip = tip_over(&fake, &[1]);
        put_record(&fake, 6, &[1], RecordState::Landing { tip: tip.clone() });
        fake.set_ref(&batch_ref(6), &tip);
        fake.set_ref(&branch("main"), &tip);
        if trunk_moved_on {
            fake.advance_trunk();
        }
        let (mut forge, _) = forge(&fake);

        assert_eq!(
            forge.observe().unwrap().batches[0].state,
            BatchState::Landed { tip }
        );
    }
}

#[test]
fn a_landing_record_whose_tip_is_not_in_trunk_is_testing() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    let tip = tip_over(&fake, &[1]);
    put_record(&fake, 6, &[1], RecordState::Landing { tip: tip.clone() });
    fake.set_ref(&batch_ref(6), &tip);
    passed(&fake, &tip);
    let (mut forge, _) = forge(&fake);

    assert_eq!(
        forge.observe().unwrap().batches[0].state,
        BatchState::Testing {
            tip,
            ci: CiStatus::Passed
        }
    );
}

#[test]
fn landed_and_refused_records_are_read_back_whatever_the_branch() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    let tips: Vec<Sha> = (0..3).map(|_| tip_over(&fake, &[1])).collect();
    let moved = fake.commit(&[fake.trunk()]);
    put_record(
        &fake,
        1,
        &[1],
        RecordState::Landed {
            tip: tips[0].clone(),
        },
    );
    put_record(
        &fake,
        2,
        &[1],
        RecordState::Refused {
            tip: tips[1].clone(),
            reason: LandRefusal::TrunkMoved {
                trunk: moved.clone(),
            },
        },
    );
    put_record(
        &fake,
        3,
        &[1],
        RecordState::Refused {
            tip: tips[2].clone(),
            reason: LandRefusal::MemberGone { pr: PrNumber(1) },
        },
    );
    fake.set_ref(&batch_ref(2), &tips[1]);
    let (mut forge, _) = forge(&fake);

    let states: Vec<BatchState> = forge
        .observe()
        .unwrap()
        .batches
        .into_iter()
        .map(|b| b.state)
        .collect();
    assert_eq!(
        states,
        [
            BatchState::Landed {
                tip: tips[0].clone()
            },
            BatchState::LandRefused {
                tip: tips[1].clone(),
                reason: LandRefusal::TrunkMoved { trunk: moved }
            },
            BatchState::LandRefused {
                tip: tips[2].clone(),
                reason: LandRefusal::MemberGone { pr: PrNumber(1) }
            },
        ]
    );
}

#[test]
fn batches_are_sorted_by_id() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    for id in [10, 2, 33] {
        let tip = tip_over(&fake, &[1]);
        put_record(&fake, id, &[1], RecordState::Landed { tip });
    }
    let (mut forge, _) = forge(&fake);

    let ids: Vec<u64> = forge
        .observe()
        .unwrap()
        .batches
        .iter()
        .map(|b| b.id.0)
        .collect();
    assert_eq!(ids, [2, 10, 33]);
}

#[test]
fn an_observation_with_two_batches_on_one_tip_is_permanent() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    let tip = tip_over(&fake, &[1]);
    for id in [1, 2] {
        put_record(&fake, id, &[1], RecordState::Landed { tip: tip.clone() });
    }
    let (mut forge, sink) = forge(&fake);

    let error = forge.observe().unwrap_err();
    assert!(
        is_permanent(&error, "batches 1 and 2 have one tip"),
        "{error}"
    );
    assert!(sink.lines().is_empty());
}

#[test]
fn a_foreign_ref_under_the_record_prefix_is_permanent() {
    for suffix in ["notes", "07", "7/extra", ""] {
        let fake = FakeGitHub::new();
        let trunk = fake.trunk();
        let name = format!("refs/dorfq/batches/{suffix}");
        fake.set_ref(&name, &trunk);
        let (mut forge, _) = forge(&fake);

        let error = forge.observe().unwrap_err();
        assert!(is_permanent(&error, &name), "{error}");
    }
}

#[test]
fn a_record_that_does_not_parse_is_permanent_naming_the_ref() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    put_record(&fake, 8, &[1], RecordState::Started);
    let sha = fake.get("refs/dorfq/batches/8").unwrap();
    fake.state().commits.get_mut(&sha).unwrap().message = "dorfq batch record v2\n".to_owned();
    let (mut forge, _) = forge(&fake);

    let error = forge.observe().unwrap_err();
    assert!(is_permanent(&error, "refs/dorfq/batches/8"), "{error}");
    assert!(error.to_string().contains("v2"), "{error}");
}

#[test]
fn a_record_under_another_batch_ref_is_permanent() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    put_record(&fake, 8, &[1], RecordState::Started);
    let sha = fake.get("refs/dorfq/batches/8").unwrap();
    fake.state().refs.remove("refs/dorfq/batches/8");
    fake.set_ref("refs/dorfq/batches/9", &sha);
    let (mut forge, _) = forge(&fake);

    let error = forge.observe().unwrap_err();
    assert!(is_permanent(&error, "refs/dorfq/batches/9"), "{error}");
    assert!(fake.get("refs/dorfq/batches/9").is_some());
}

#[test]
fn a_first_sighting_with_no_marker_is_greeted_at_the_next_flush() {
    let fake = FakeGitHub::new();
    ready_pr(&fake, 1);
    fake.state()
        .comments
        .insert(1, vec!["Please review".to_owned()]);
    let (mut forge, sink) = forge(&fake);

    forge.observe().unwrap();
    assert_eq!(fake.comments(1).len(), 1, "queued, not yet posted");
    forge.observe().unwrap();
    let comments = fake.comments(1);
    assert_eq!(comments.len(), 2);
    assert!(comments[1].starts_with("<!-- dorfq: queued -->\n"));
    assert!(comments[1].contains("`queue` label"));
    assert_eq!(sink.lines(), ["message to pr 1: queued"]);
}

#[test]
fn a_last_marker_of_queued_or_testing_is_not_greeted_again() {
    for last in ["queued", "testing"] {
        let fake = FakeGitHub::new();
        ready_pr(&fake, 1);
        fake.state().comments.insert(
            1,
            vec![marker("withdrawn"), marker(last), "LGTM".to_owned()],
        );
        let (mut forge, sink) = forge(&fake);

        forge.observe().unwrap();
        forge.observe().unwrap();
        assert_eq!(fake.comments(1).len(), 3, "{last}");
        assert!(sink.lines().is_empty());
    }
}

#[test]
fn a_pull_request_is_told_what_holds_it_each_time_it_starts_waiting() {
    let fake = FakeGitHub::new();
    let head = fake.open_pr(1);
    let (mut forge, sink) = forge(&fake);

    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(
        sink.lines(),
        [
            "message to pr 1: queued",
            "message to pr 1: held: own CI pending"
        ]
    );
    let comments = fake.comments(1);
    assert!(comments[1].starts_with("<!-- dorfq: held -->\n"));
    assert!(comments[1].contains("Waiting in the merge queue: own CI pending."));

    passed(&fake, &head);
    forge.observe().unwrap();
    fake.pr(1).mergeable = None;
    forge.observe().unwrap();
    fake.set_checks(&head, vec![result("lint", CheckOutcome::Failed)]);
    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(
        sink.lines()[2..],
        ["message to pr 1: held: own CI failed, mergeability unknown"]
    );
}

#[test]
fn a_pull_request_waiting_only_on_mergeability_is_told_nothing() {
    // GitHub recomputes mergeability each time trunk moves.
    let fake = FakeGitHub::new();
    ready_pr(&fake, 1);
    let head = fake.open_pr(2);
    passed(&fake, &head);
    fake.pr(2).mergeable = None;
    let (mut forge, sink) = forge(&fake);
    forge.observe().unwrap();

    fake.pr(1).mergeable = None;
    fake.pr(2).mergeable = Some(true);
    forge.observe().unwrap();
    fake.pr(1).mergeable = Some(true);
    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(
        sink.lines(),
        ["message to pr 1: queued", "message to pr 2: queued"]
    );
}

#[test]
fn a_first_sighting_already_told_held_is_not_told_again() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    fake.state()
        .comments
        .insert(1, vec![marker("queued"), marker("held")]);
    let (mut forge, sink) = forge(&fake);

    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(fake.comments(1).len(), 2);
    assert!(sink.lines().is_empty());
}

#[test]
fn a_first_sighting_told_held_and_then_notified_is_not_told_held_again() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    fake.state()
        .comments
        .insert(1, vec![marker("queued"), marker("held"), marker("notice")]);
    let (mut forge, sink) = forge(&fake);

    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(fake.comments(1).len(), 3);
    assert!(sink.lines().is_empty());
}

#[test]
fn a_last_marker_of_landed_ejected_or_withdrawn_is_greeted() {
    for last in ["landed", "ejected", "withdrawn"] {
        let fake = FakeGitHub::new();
        ready_pr(&fake, 1);
        fake.state()
            .comments
            .insert(1, vec![marker("queued"), marker(last)]);
        let (mut forge, sink) = forge(&fake);

        forge.observe().unwrap();
        forge.observe().unwrap();
        assert_eq!(sink.lines(), ["message to pr 1: queued"], "{last}");
    }
}

#[test]
fn a_last_marker_of_notice_is_not_greeted_again() {
    let fake = FakeGitHub::new();
    ready_pr(&fake, 1);
    fake.state()
        .comments
        .insert(1, vec![marker("queued"), marker("notice")]);
    let (mut forge, sink) = forge(&fake);

    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(fake.comments(1).len(), 2);
    assert!(sink.lines().is_empty());
}

#[test]
fn a_pull_request_observed_twice_lists_its_comments_once() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    fake.open_pr(2);
    let (mut forge, _) = forge(&fake);

    for _ in 0..3 {
        forge.observe().unwrap();
    }
    assert_eq!(
        fake.calls_to(&["list_comments"]),
        ["list_comments 1", "list_comments 2"]
    );
}

#[test]
fn a_known_pull_request_that_vanishes_is_told_withdrawn() {
    for how in ["unlabelled", "closed"] {
        let fake = FakeGitHub::new();
        ready_pr(&fake, 1);
        let (mut forge, sink) = forge(&fake);
        forge.observe().unwrap();

        match how {
            "unlabelled" => fake.pr(1).labels.clear(),
            _ => fake.pr(1).open = false,
        }
        assert!(forge.observe().unwrap().prs.is_empty());
        forge.observe().unwrap();
        assert_eq!(
            sink.lines(),
            ["message to pr 1: queued", "message to pr 1: withdrawn"],
            "{how}"
        );
        assert!(
            fake.comments(1)[1].starts_with("<!-- dorfq: withdrawn -->"),
            "{how}"
        );
    }
}

#[test]
fn departed_says_how_each_pull_request_left_asking_only_about_the_rest() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 7);
    let (mut forge, sink) = observed(&fake);
    forge.start_batch(&start(1, &fake.trunk(), &[1])).unwrap();
    let tip = fake.get(&batch_ref(1)).expect("published");
    passed(&fake, &tip);
    forge.observe().unwrap();

    // 1 lands, and GitHub marks it merged; 2 is ejected; 3 is unlabelled,
    // 4 closed, 5 retargeted, 6 merged by another hand, 7 retargeted and
    // merged there.
    forge.land(BatchId(1)).unwrap();
    {
        let mut one = fake.pr(1);
        one.open = false;
        one.merged = true;
    }
    forge.eject(PrNumber(2), &CONFLICTS_WITH_TRUNK).unwrap();
    fake.pr(3).labels.clear();
    fake.pr(4).open = false;
    fake.pr(5).base = "release".to_owned();
    {
        let mut six = fake.pr(6);
        six.open = false;
        six.merged = true;
    }
    {
        let mut seven = fake.pr(7);
        seven.base = "release".to_owned();
        seven.open = false;
        seven.merged = true;
    }
    sink.0.borrow_mut().clear();
    let asked_before = fake.calls_to(&["get_pr"]).len();
    let observation = forge.observe().unwrap();

    let how = |n: usize, how| Departure::new(PrNumber(n as u64), heads[n - 1].clone(), how);
    assert_eq!(
        observation.departed,
        [
            how(1, Departed::Landed { batch: BatchId(1) }),
            how(2, Departed::Ejected),
            how(3, Departed::Withdrawn(Withdrawal::IntentCleared)),
            how(4, Departed::Withdrawn(Withdrawal::Closed)),
            how(5, Departed::Withdrawn(Withdrawal::Retargeted)),
            how(6, Departed::Merged),
            how(7, Departed::Withdrawn(Withdrawal::Retargeted)),
        ]
    );
    // 3 and 5 are still open and listed: the listing says how they left.
    assert_eq!(
        fake.calls_to(&["get_pr"])[asked_before..],
        ["get_pr 4", "get_pr 6", "get_pr 7"]
    );
    forge.observe().unwrap();
    assert_eq!(
        sink.lines(),
        [
            "message to pr 3: withdrawn",
            "message to pr 4: withdrawn",
            "message to pr 6: withdrawn",
            "message to pr 7: withdrawn",
            "message to pr 5: blocked on branch release",
        ]
    );
}

#[test]
fn a_pull_request_listed_twice_at_one_head_is_reported_once() {
    // A pull request opened between two page reads shifts the item on the
    // boundary onto the next page too.
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 2);
    fake.state().listed_again.push((1, heads[0].clone()));
    let (mut forge, _) = forge(&fake);

    let observation = forge.observe().unwrap();
    let numbers: Vec<PrNumber> = observation.prs.iter().map(|p| p.number).collect();
    assert_eq!(numbers, [PrNumber(1), PrNumber(2)]);
    assert_eq!(fake.calls_to(&["get_pr"]), ["get_pr 1", "get_pr 2"]);
}

#[test]
fn a_pull_request_listed_at_two_heads_is_listed_again_next_tick() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, sink) = observed(&fake);
    let moved = fake.commit(&[fake.trunk()]);
    fake.state().listed_again.push((1, moved));

    let error = forge.observe().unwrap_err();
    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert!(error.to_string().contains("#1"), "{error}");
    fake.state().listed_again.clear();
    assert!(forge.observe().unwrap().departed.is_empty());
    assert!(sink.lines().is_empty());
}

#[test]
fn an_enqueued_pull_request_the_listing_left_out_is_listed_again_next_tick() {
    // A pull request closed between two page reads shifts the item on the
    // boundary onto the earlier page, which was already read.
    let fake = FakeGitHub::new();
    queued(&fake, 2);
    let (mut forge, sink) = observed(&fake);
    fake.state().unlisted.insert(1);

    let error = forge.observe().unwrap_err();
    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert!(error.to_string().contains("#1"), "{error}");
    fake.state().unlisted.clear();
    let observation = forge.observe().unwrap();
    assert!(observation.departed.is_empty());
    assert_eq!(observation.prs.len(), 2);
    forge.observe().unwrap();
    assert!(sink.lines().is_empty());
}

#[test]
fn a_record_an_earlier_build_published_at_its_base_is_permanent() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let base = fake.trunk();
    put_record(&fake, 3, &[1], RecordState::Testing { tip: base.clone() });
    fake.set_ref(&batch_ref(3), &base);
    let (mut forge, _) = forge(&fake);

    let error = forge.observe().unwrap_err();
    assert!(is_permanent(&error, "batch 3's tip is its base"), "{error}");
}

#[test]
fn a_departure_the_queue_completed_at_retire_is_landed() {
    // The landing's bookkeeping was cut short, so retire completes it for a
    // member still enqueued; that member departs after its batch is gone.
    let fake = FakeGitHub::new();
    let (mut forge, _, _) = passed_batch(&fake, 1);
    fake.fail_on("create_commit", "state: landed", Failure::Transient);
    assert!(forge.land(BatchId(1)).is_err());
    let observation = forge.observe().unwrap();
    assert_eq!(observation.prs.len(), 1);
    forge.retire(BatchId(1)).unwrap();
    let asked_before = fake.calls_to(&["get_pr"]).len();
    let observation = forge.observe().unwrap();
    assert!(observation.batches.is_empty());
    assert_eq!(
        observation.departed,
        [Departure::new(
            PrNumber(1),
            fake.pr(1).head.clone(),
            Departed::Landed { batch: BatchId(1) }
        )]
    );
    assert_eq!(fake.calls_to(&["get_pr"]).len(), asked_before);
}

#[test]
fn a_withdrawn_pull_request_relabelled_is_greeted_again() {
    let fake = FakeGitHub::new();
    ready_pr(&fake, 1);
    let (mut forge, sink) = forge(&fake);
    forge.observe().unwrap();
    fake.pr(1).labels.clear();
    forge.observe().unwrap();
    fake.pr(1).labels.push("queue".to_owned());
    forge.observe().unwrap();
    forge.observe().unwrap();

    assert_eq!(
        sink.lines(),
        [
            "message to pr 1: queued",
            "message to pr 1: withdrawn",
            "message to pr 1: queued"
        ]
    );
}

#[test]
fn a_known_pull_request_that_vanishes_with_its_batch_landed_is_told_landed() {
    let fake = FakeGitHub::new();
    ready_pr(&fake, 1);
    ready_pr(&fake, 2);
    let (mut forge, sink) = forge(&fake);
    forge.observe().unwrap();

    // Batch 4 landed and GitHub closed its members as merged; this process
    // never finished the landing's bookkeeping.
    let tip = tip_over(&fake, &[1, 2]);
    put_record(&fake, 4, &[1, 2], RecordState::Landing { tip: tip.clone() });
    fake.set_ref(&batch_ref(4), &tip);
    fake.set_ref(&branch("main"), &tip);
    fake.pr(1).open = false;
    fake.pr(2).open = false;
    let observation = forge.observe().unwrap();
    assert_eq!(
        observation.batches[0].state,
        BatchState::Landed { tip: tip.clone() }
    );
    forge.observe().unwrap();

    let told: Vec<String> = sink.lines().into_iter().skip(2).collect();
    assert_eq!(
        told,
        [
            format!("message to pr 1: landed as {tip}"),
            format!("message to pr 2: landed as {tip}"),
        ]
    );
    assert!(fake.comments(1)[1].contains(&format!("`main` is now {tip}")));
}

#[test]
fn a_landed_member_still_enqueued_is_told_nothing_at_observe() {
    let fake = FakeGitHub::new();
    ready_pr(&fake, 1);
    let (mut forge, sink) = forge(&fake);
    forge.observe().unwrap();

    let tip = tip_over(&fake, &[1]);
    put_record(&fake, 4, &[1], RecordState::Landed { tip: tip.clone() });
    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(sink.lines(), ["message to pr 1: queued"]);
}

#[test]
fn a_vanished_member_of_an_unlanded_batch_is_told_withdrawn() {
    let fake = FakeGitHub::new();
    ready_pr(&fake, 1);
    let (mut forge, sink) = forge(&fake);
    forge.observe().unwrap();

    let tip = tip_over(&fake, &[1]);
    put_record(&fake, 4, &[1], RecordState::Testing { tip: tip.clone() });
    fake.set_ref(&batch_ref(4), &tip);
    fake.pr(1).labels.clear();
    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(
        sink.lines(),
        ["message to pr 1: queued", "message to pr 1: withdrawn"]
    );
}

#[test]
fn a_failed_observation_changes_nothing_known() {
    let fake = FakeGitHub::new();
    ready_pr(&fake, 1);
    let (mut forge, sink) = forge(&fake);
    fake.fail("list_comments", Failure::Transient);
    assert!(matches!(forge.observe(), Err(ForgeError::Transient(_))));
    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(sink.lines(), ["message to pr 1: queued"]);
}

#[test]
fn the_outbox_keeps_a_comment_through_a_transient_failure() {
    let fake = FakeGitHub::new();
    ready_pr(&fake, 1);
    ready_pr(&fake, 2);
    let (mut forge, sink) = forge(&fake);
    forge.observe().unwrap();

    fake.fail("create_comment", Failure::Transient);
    forge.observe().unwrap();
    assert!(fake.comments(1).is_empty());
    assert!(fake.comments(2).is_empty(), "order is kept behind a retry");
    assert!(sink.lines().is_empty());

    forge.observe().unwrap();
    assert_eq!(fake.comments(1).len(), 1);
    assert_eq!(fake.comments(2).len(), 1);
    assert_eq!(
        sink.lines(),
        ["message to pr 1: queued", "message to pr 2: queued"]
    );
}

#[test]
fn the_outbox_drops_a_comment_github_refuses_for_good() {
    let fake = FakeGitHub::new();
    ready_pr(&fake, 1);
    ready_pr(&fake, 2);
    let (mut forge, sink) = forge(&fake);
    forge.observe().unwrap();

    fake.fail("create_comment", Failure::Permanent);
    forge.observe().unwrap();
    assert!(fake.comments(1).is_empty());
    assert_eq!(fake.comments(2).len(), 1);
    assert_eq!(sink.lines(), ["message to pr 2: queued"]);

    forge.observe().unwrap();
    assert_eq!(fake.calls_to(&["create_comment"]).len(), 2);
}

/// Opens pull requests `1..=n`, each already greeted and passing; their
/// heads.
fn queued(fake: &FakeGitHub, n: u64) -> Vec<Sha> {
    (1..=n)
        .map(|number| {
            let head = fake.open_pr(number);
            passed(fake, &head);
            fake.state().comments.insert(number, vec![marker("queued")]);
            head
        })
        .collect()
}

/// A forge that has made its first observation.
fn observed(fake: &FakeGitHub) -> (GitHubForge, Buffer) {
    let (mut forge, sink) = forge(fake);
    forge.observe().unwrap();
    (forge, sink)
}

const CONFLICTS_WITH_TRUNK: EjectReason = EjectReason::Conflicts {
    with: ConflictWith::Trunk,
};

fn start(id: u64, base: &Sha, prs: &[u64]) -> BatchStart {
    BatchStart::new(
        BatchId(id),
        base.clone(),
        prs.iter().map(|&n| PrNumber(n)).collect(),
    )
}

fn member(pr: u64, head: &Sha) -> BatchMember {
    BatchMember::new(PrNumber(pr), head.clone())
}

fn record_of(fake: &FakeGitHub, id: u64) -> Record {
    let name = format!("refs/dorfq/batches/{id}");
    let sha = fake.get(&name).expect("the record exists");
    let message = fake.state().commits[&sha].message.clone();
    Record::parse(&name, BatchId(id), &message).unwrap()
}

/// The dorfq markers on pull request `n`'s comments, oldest first.
fn marks(fake: &FakeGitHub, n: u64) -> Vec<String> {
    fake.comments(n)
        .iter()
        .filter_map(|c| {
            c.strip_prefix("<!-- dorfq: ")
                .and_then(|rest| rest.split_once(" -->"))
                .map(|(word, _)| word.to_owned())
        })
        .collect()
}

/// Batch 1 over pull requests `1..=n`, built, passed and observed so; the
/// forge, its sink and the tip. The sink is cleared.
fn passed_batch(fake: &FakeGitHub, n: u64) -> (GitHubForge, Buffer, Sha) {
    queued(fake, n);
    let (mut forge, sink) = observed(fake);
    let prs: Vec<u64> = (1..=n).collect();
    forge.start_batch(&start(1, &fake.trunk(), &prs)).unwrap();
    let tip = fake.get(&batch_ref(1)).expect("published");
    passed(fake, &tip);
    let observation = forge.observe().unwrap();
    assert_eq!(
        observation.batches[0].state,
        BatchState::Testing {
            tip: tip.clone(),
            ci: CiStatus::Passed
        }
    );
    sink.0.borrow_mut().clear();
    (forge, sink, tip)
}

fn landed_lines(tip: &Sha, prs: &[u64]) -> Vec<String> {
    prs.iter()
        .map(|n| format!("message to pr {n}: landed as {tip}"))
        .collect()
}

#[test]
fn a_clean_batch_is_built_on_its_build_branch_and_published_at_the_tip() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 2);
    let (mut forge, sink) = observed(&fake);
    let base = fake.trunk();

    forge.start_batch(&start(1, &base, &[1, 2])).unwrap();

    let tip = fake.get(&batch_ref(1)).expect("published");
    assert_eq!(fake.state().commits[&tip].parents[1], heads[1]);
    let record = record_of(&fake, 1);
    assert_eq!(record.base, base);
    assert_eq!(record.members, [member(1, &heads[0]), member(2, &heads[1])]);
    assert_eq!(record.state, RecordState::Testing { tip: tip.clone() });
    let record_sha = fake.get("refs/dorfq/batches/1").unwrap();
    let base_tree = fake.state().commits[&base].tree.clone();
    let record_commit = fake.state().commits[&record_sha].clone();
    assert_eq!(record_commit.tree, base_tree);
    assert_eq!(record_commit.parents, std::slice::from_ref(&base));
    assert!(fake.get(&branch("dorfq/build/1")).is_none());
    assert!(fake.get(&branch("dorfq/probe/1")).is_none());

    let merges = fake.calls_to(&["merge"]);
    assert_eq!(merges.len(), 2);
    assert!(
        merges[0].starts_with(&format!("merge dorfq/build/1 {} ", heads[0])),
        "{}",
        merges[0]
    );
    assert!(
        merges[0].contains(r"Merge #1: Change 1\n\nDorfq-Batch: 1"),
        "{}",
        merges[0]
    );
    assert!(merges[1].contains(r"Merge #2: Change 2\n\nDorfq-Batch: 1"));
    let creates = fake.calls_to(&["create_ref"]);
    assert!(creates[0].starts_with("create_ref refs/dorfq/batches/1 "));
    assert_eq!(
        creates[1..],
        [
            format!("create_ref refs/heads/dorfq/build/1 {base}"),
            format!("create_ref refs/heads/dorfq/batch/1 {tip}"),
        ]
    );
    assert_eq!(
        sink.lines(),
        [
            "message to pr 1: testing in batch 1".to_owned(),
            "message to pr 2: testing in batch 1".to_owned(),
        ]
    );
    assert_eq!(marks(&fake, 2), ["queued", "testing"]);

    let observation = forge.observe().unwrap();
    assert_eq!(
        observation.batches[0].state,
        BatchState::Testing {
            tip,
            ci: CiStatus::Pending
        }
    );
}

#[test]
fn a_member_with_nothing_to_merge_stays_in_the_batch() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 2);
    fake.pr(2).head = heads[0].clone();
    let (mut forge, sink) = observed(&fake);

    forge
        .start_batch(&start(1, &fake.trunk(), &[1, 2]))
        .unwrap();

    let tip = fake.get(&batch_ref(1)).expect("published");
    assert_eq!(fake.state().commits[&tip].parents[1], heads[0]);
    let record = record_of(&fake, 1);
    assert_eq!(record.members, [member(1, &heads[0]), member(2, &heads[0])]);
    assert_eq!(record.state, RecordState::Testing { tip });
    assert_eq!(sink.lines().len(), 2);
}

#[test]
fn a_batch_with_nothing_to_merge_ends_on_an_empty_commit_of_its_own() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let base = fake.trunk();
    fake.pr(1).head = base.clone();
    passed(&fake, &base);
    let (mut forge, _) = observed(&fake);

    forge.start_batch(&start(1, &base, &[1])).unwrap();

    let tip = fake.get(&batch_ref(1)).expect("published");
    assert_ne!(tip, base);
    let commit = fake.state().commits[&tip].clone();
    assert_eq!(commit.parents, std::slice::from_ref(&base));
    assert_eq!(commit.tree, fake.state().commits[&base].tree);
    assert_eq!(commit.message, "dorfq batch 1");
    assert_eq!(
        record_of(&fake, 1).state,
        RecordState::Testing { tip: tip.clone() }
    );
    let observation = forge.observe().unwrap();
    assert_eq!(
        observation.batches[0].state,
        BatchState::Testing {
            tip,
            ci: CiStatus::Pending
        }
    );
}

#[test]
fn a_probe_publishes_an_empty_commit_on_its_base_and_tells_nobody() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let base = fake.trunk();
    let (mut first, sink) = observed(&fake);
    sink.0.borrow_mut().clear();

    first.start_batch(&start(1, &base, &[])).unwrap();

    let tip = fake.get(&batch_ref(1)).expect("published");
    assert_ne!(tip, base);
    let commit = fake.state().commits[&tip].clone();
    assert_eq!(commit.parents, std::slice::from_ref(&base));
    assert_eq!(commit.tree, fake.state().commits[&base].tree);
    assert_eq!(commit.message, "dorfq probe 1");
    assert!(fake.get(&branch("dorfq/build/1")).is_none());
    let record = record_of(&fake, 1);
    assert!(record.members.is_empty());
    assert_eq!(record.state, RecordState::Testing { tip: tip.clone() });
    assert!(sink.lines().is_empty(), "{:?}", sink.lines());

    let (mut restarted, _) = forge(&fake);
    let observation = restarted.observe().unwrap();
    assert!(observation.batches[0].members.is_empty());
    assert_eq!(
        observation.batches[0].state,
        BatchState::Testing {
            tip,
            ci: CiStatus::Pending
        }
    );
}

#[test]
fn land_on_a_passed_probe_is_permanent_and_writes_nothing() {
    let fake = FakeGitHub::new();
    let (mut forge, _, _) = passed_batch(&fake, 0);
    let trunk = fake.trunk();
    let record = fake.get("refs/dorfq/batches/1");

    match forge.land(BatchId(1)) {
        Err(ForgeError::Permanent(e)) => assert!(e.to_string().contains("probe"), "{e}"),
        other => panic!("expected a permanent error, got {other:?}"),
    }
    assert_eq!(fake.trunk(), trunk);
    assert_eq!(fake.get("refs/dorfq/batches/1"), record);
    forge.retire(BatchId(1)).unwrap();
    assert!(forge.observe().unwrap().batches.is_empty());
}

/// Starts batch 1 over `prs` and returns what its record says it
/// conflicted with, after checking that nothing was published or told.
fn conflict_of(
    fake: &FakeGitHub,
    forge: &mut GitHubForge,
    sink: &Buffer,
    prs: &[u64],
) -> RecordState {
    forge.start_batch(&start(1, &fake.trunk(), prs)).unwrap();
    assert!(fake.get(&batch_ref(1)).is_none());
    assert!(fake.get(&branch("dorfq/build/1")).is_none());
    assert!(fake.get(&branch("dorfq/probe/1")).is_none());
    assert!(sink.lines().is_empty(), "{:?}", sink.lines());
    record_of(fake, 1).state
}

fn conflict(pr: u64, with: ConflictWith) -> RecordState {
    RecordState::Conflict {
        pr: PrNumber(pr),
        with,
    }
}

fn probes(fake: &FakeGitHub) -> usize {
    fake.calls_to(&["create_ref"])
        .iter()
        .filter(|c| c.contains("dorfq/probe/"))
        .count()
}

#[test]
fn a_conflict_at_the_first_member_is_with_trunk() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 2);
    fake.state().conflicts_with_trunk.insert(heads[0].clone());
    let (mut forge, sink) = observed(&fake);

    let state = conflict_of(&fake, &mut forge, &sink, &[1, 2]);
    assert_eq!(state, conflict(1, ConflictWith::Trunk));
    assert_eq!(probes(&fake), 0);
    assert_eq!(fake.calls_to(&["merge"]).len(), 1);

    let observation = forge.observe().unwrap();
    assert_eq!(
        observation.batches[0].state,
        BatchState::Conflict {
            pr: PrNumber(1),
            with: ConflictWith::Trunk
        }
    );
}

#[test]
fn start_rejects_a_member_not_ready_before_any_write() {
    let fake = FakeGitHub::new();
    queued(&fake, 3);
    fake.pr(2).mergeable = Some(false);
    fake.pr(3).mergeable = None;
    let (mut forge, _) = observed(&fake);
    let writes = fake.writes().len();

    for n in [2, 3] {
        let error = forge
            .start_batch(&start(1, &fake.trunk(), &[1, n]))
            .unwrap_err();
        assert!(is_permanent(&error, "which is not ready"), "{error}");
    }
    assert_eq!(fake.writes().len(), writes);
}

#[test]
fn the_probe_names_the_earlier_member_a_conflict_needs() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 3);
    fake.state()
        .conflicts
        .insert((heads[1].clone(), heads[2].clone()));
    let (mut forge, sink) = observed(&fake);

    let state = conflict_of(&fake, &mut forge, &sink, &[1, 2, 3]);
    assert_eq!(state, conflict(3, ConflictWith::Pr(PrNumber(2))));
    assert_eq!(probes(&fake), 1);
    let probe_merges: Vec<String> = fake
        .calls_to(&["merge"])
        .into_iter()
        .filter(|c| c.starts_with("merge dorfq/probe/1 "))
        .collect();
    assert_eq!(probe_merges.len(), 5, "3 alone, 1 then 3, 2 then 3");
}

#[test]
fn the_probe_finds_a_member_that_conflicts_alone_with_trunk() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 2);
    fake.state().conflicts_with_trunk.insert(heads[1].clone());
    let (mut forge, sink) = observed(&fake);

    let state = conflict_of(&fake, &mut forge, &sink, &[1, 2]);
    assert_eq!(state, conflict(2, ConflictWith::Trunk));
    assert_eq!(probes(&fake), 1);
}

#[test]
fn a_conflict_no_pair_reproduces_names_the_last_earlier_member() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 4);
    fake.state()
        .conflicts_with_all
        .push((heads[3].clone(), vec![heads[0].clone(), heads[1].clone()]));
    let (mut forge, sink) = observed(&fake);

    let state = conflict_of(&fake, &mut forge, &sink, &[1, 2, 3, 4]);
    assert_eq!(state, conflict(4, ConflictWith::Pr(PrNumber(3))));
}

#[test]
fn a_transient_failure_mid_build_is_cleaned_up_by_the_next_observe() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 2);
    let (mut forge, sink) = observed(&fake);
    fake.fail_on("merge", heads[1].as_str(), Failure::Transient);

    let error = forge
        .start_batch(&start(1, &fake.trunk(), &[1, 2]))
        .unwrap_err();
    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert_eq!(record_of(&fake, 1).state, RecordState::Started);
    assert!(fake.get(&branch("dorfq/build/1")).is_some());

    let observation = forge.observe().unwrap();
    assert!(observation.batches.is_empty());
    assert!(fake.state().refs.keys().all(|r| !r.contains("dorfq")));
    assert!(sink.lines().is_empty());

    forge
        .start_batch(&start(1, &fake.trunk(), &[1, 2]))
        .unwrap();
    assert!(fake.get(&batch_ref(1)).is_some());
}

#[test]
fn a_start_violating_rule_5_is_permanent_and_writes_nothing() {
    let fake = FakeGitHub::new();
    queued(&fake, 2);
    let (mut forge, _) = observed(&fake);
    let trunk = fake.trunk();
    let stray = fake.commit(std::slice::from_ref(&trunk));

    for (bad, needle) in [
        (start(1, &trunk, &[1, 9]), "pr 9"),
        (start(1, &trunk, &[1, 1]), "twice"),
        (start(1, &stray, &[1]), "base"),
    ] {
        let error = forge.start_batch(&bad).unwrap_err();
        assert!(matches!(error, ForgeError::Permanent(_)), "{error}");
        assert!(error.to_string().contains(needle), "{needle}: {error}");
    }
    assert!(fake.writes().is_empty(), "{:?}", fake.writes());
}

#[test]
fn land_records_the_landing_before_it_touches_trunk() {
    let fake = FakeGitHub::new();
    let (mut forge, _, tip) = passed_batch(&fake, 2);
    let base = fake.trunk();
    fake.fail_on("update_ref", "refs/heads/main", Failure::Transient);

    let error = forge.land(BatchId(1)).unwrap_err();
    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert_eq!(fake.trunk(), base);
    assert_eq!(
        record_of(&fake, 1).state,
        RecordState::Landing { tip: tip.clone() }
    );

    let observation = forge.observe().unwrap();
    assert_eq!(
        observation.batches[0].state,
        BatchState::Testing {
            tip,
            ci: CiStatus::Passed
        }
    );
    assert_eq!(observation.prs.len(), 2);
}

#[test]
fn a_landing_fast_forwards_trunk_and_completes_every_member() {
    let fake = FakeGitHub::new();
    let (mut forge, sink, tip) = passed_batch(&fake, 2);

    forge.land(BatchId(1)).unwrap();

    assert_eq!(fake.trunk(), tip);
    assert_eq!(
        record_of(&fake, 1).state,
        RecordState::Landed { tip: tip.clone() }
    );
    assert!(fake.pr(1).labels.is_empty());
    assert!(fake.pr(2).labels.is_empty());
    assert_eq!(sink.lines(), landed_lines(&tip, &[1, 2]));
    assert!(
        fake.comments(1)
            .last()
            .unwrap()
            .contains(&format!("`main` is now {tip}"))
    );

    let observation = forge.observe().unwrap();
    assert!(observation.prs.is_empty());
    assert_eq!(
        observation.batches[0].state,
        BatchState::Landed { tip: tip.clone() }
    );
    let labels_before = fake.calls_to(&["remove_label"]).len();
    forge.retire(BatchId(1)).unwrap();
    forge.observe().unwrap();
    assert_eq!(fake.calls_to(&["remove_label"]).len(), labels_before);
    assert_eq!(sink.lines(), landed_lines(&tip, &[1, 2]), "never withdrawn");
}

#[test]
fn a_batch_with_nothing_to_merge_lands_its_empty_commit() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let base = fake.trunk();
    fake.pr(1).head = base.clone();
    passed(&fake, &base);
    let (mut forge, sink) = observed(&fake);
    forge.start_batch(&start(1, &base, &[1])).unwrap();
    let tip = fake.get(&batch_ref(1)).expect("published");
    passed(&fake, &tip);
    forge.observe().unwrap();
    sink.0.borrow_mut().clear();

    forge.land(BatchId(1)).unwrap();

    assert_eq!(fake.trunk(), tip);
    assert_eq!(fake.state().commits[&tip].parents, [base]);
    assert!(fake.pr(1).labels.is_empty());
    assert_eq!(sink.lines(), landed_lines(&tip, &[1]));
}

fn refused(fake: &FakeGitHub, forge: &mut GitHubForge, sink: &Buffer, tip: &Sha) -> LandRefusal {
    forge.land(BatchId(1)).unwrap();
    assert_ne!(&fake.trunk(), tip, "a refusal does not land");
    let RecordState::Refused {
        tip: recorded,
        reason,
    } = record_of(fake, 1).state
    else {
        panic!("not refused: {:?}", record_of(fake, 1).state);
    };
    assert_eq!(&recorded, tip);
    assert!(sink.lines().is_empty(), "{:?}", sink.lines());
    let observation = forge.observe().unwrap();
    assert_eq!(
        observation.batches[0].state,
        BatchState::LandRefused {
            tip: tip.clone(),
            reason: reason.clone()
        }
    );
    reason
}

#[test]
fn land_refuses_when_trunk_moved_before_the_check() {
    let fake = FakeGitHub::new();
    let (mut forge, sink, tip) = passed_batch(&fake, 2);
    let moved = fake.advance_trunk();

    let reason = refused(&fake, &mut forge, &sink, &tip);
    assert_eq!(reason, LandRefusal::TrunkMoved { trunk: moved });
    assert!(
        fake.calls_to(&["update_ref"])
            .iter()
            .all(|c| !c.starts_with("update_ref refs/heads/main "))
    );
    assert_eq!(fake.pr(1).labels, ["queue"]);
}

#[test]
fn land_refuses_when_trunk_moves_under_the_fast_forward() {
    let fake = FakeGitHub::new();
    let (mut forge, sink, tip) = passed_batch(&fake, 2);
    let moved = fake.commit(&[fake.trunk()]);
    fake.state().interpose = Some((branch("main"), moved.clone()));

    let reason = refused(&fake, &mut forge, &sink, &tip);
    assert_eq!(reason, LandRefusal::TrunkMoved { trunk: moved });
}

#[test]
fn land_refuses_when_a_member_is_gone() {
    for how in ["closed", "unlabelled", "rebased", "retargeted"] {
        let fake = FakeGitHub::new();
        let (mut forge, sink, tip) = passed_batch(&fake, 3);
        match how {
            "closed" => fake.pr(2).open = false,
            "unlabelled" => fake.pr(2).labels.clear(),
            "rebased" => {
                let head = fake.commit(&[fake.trunk()]);
                fake.pr(2).head = head;
            }
            _ => fake.pr(2).base = "release".to_owned(),
        }

        let reason = refused(&fake, &mut forge, &sink, &tip);
        assert_eq!(reason, LandRefusal::MemberGone { pr: PrNumber(2) }, "{how}");
    }
}

#[test]
fn a_land_violating_rule_6_is_permanent_and_writes_nothing() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _) = observed(&fake);
    forge.start_batch(&start(1, &fake.trunk(), &[1])).unwrap();
    forge.observe().unwrap();
    let writes = fake.writes().len();

    for (batch, needle) in [(9, "batch 9 is not known"), (1, "not reported")] {
        let error = forge.land(BatchId(batch)).unwrap_err();
        assert!(matches!(error, ForgeError::Permanent(_)), "{error}");
        assert!(error.to_string().contains(needle), "{needle}: {error}");
    }
    assert_eq!(fake.writes().len(), writes);
}

#[test]
fn a_landing_cut_short_is_told_landed_to_members_github_closed() {
    let fake = FakeGitHub::new();
    let (mut forge, sink, tip) = passed_batch(&fake, 2);
    fake.fail_on("create_commit", "state: landed", Failure::Transient);

    assert!(forge.land(BatchId(1)).is_err());
    assert_eq!(fake.trunk(), tip);
    fake.pr(1).open = false;
    fake.pr(2).open = false;

    let observation = forge.observe().unwrap();
    assert!(observation.prs.is_empty());
    assert_eq!(
        observation.batches[0].state,
        BatchState::Landed { tip: tip.clone() }
    );
    forge.retire(BatchId(1)).unwrap();
    assert!(
        fake.calls_to(&["remove_label"]).is_empty(),
        "none still known"
    );
    forge.observe().unwrap();
    assert_eq!(sink.lines(), landed_lines(&tip, &[1, 2]));
    assert_eq!(marks(&fake, 1), ["queued", "testing", "landed"]);
}

#[test]
fn a_landing_cut_short_is_completed_by_retire_for_members_still_open() {
    for close_before_retire in [false, true] {
        let fake = FakeGitHub::new();
        let (mut forge, sink, tip) = passed_batch(&fake, 2);
        fake.fail_on("create_commit", "state: landed", Failure::Transient);

        assert!(forge.land(BatchId(1)).is_err());
        let observation = forge.observe().unwrap();
        assert_eq!(observation.prs.len(), 2);
        assert_eq!(
            observation.batches[0].state,
            BatchState::Landed { tip: tip.clone() }
        );
        forge.observe().unwrap();
        assert!(sink.lines().is_empty(), "nothing said while still labelled");

        if close_before_retire {
            fake.pr(1).open = false;
        }
        forge.retire(BatchId(1)).unwrap();
        assert!(fake.pr(1).labels.is_empty());
        assert!(fake.pr(2).labels.is_empty());
        assert_eq!(fake.calls_to(&["remove_label"]).len(), 2);
        forge.observe().unwrap();
        forge.observe().unwrap();
        assert_eq!(
            sink.lines(),
            landed_lines(&tip, &[1, 2]),
            "close before retire: {close_before_retire}"
        );
    }
}

#[test]
fn a_label_removal_cut_short_is_finished_by_retire() {
    let fake = FakeGitHub::new();
    let (mut forge, sink, tip) = passed_batch(&fake, 2);
    fake.fail_on("remove_label", "2 queue", Failure::Transient);

    assert!(forge.land(BatchId(1)).is_err());
    assert_eq!(
        record_of(&fake, 1).state,
        RecordState::Landed { tip: tip.clone() }
    );
    let observation = forge.observe().unwrap();
    assert_eq!(
        observation
            .prs
            .iter()
            .map(|p| p.number.0)
            .collect::<Vec<_>>(),
        [2]
    );
    forge.retire(BatchId(1)).unwrap();
    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(sink.lines(), landed_lines(&tip, &[1, 2]));
}

#[test]
fn a_restart_after_a_landing_cut_short_completes_only_the_open_members() {
    let fake = FakeGitHub::new();
    let (mut first, _, tip) = passed_batch(&fake, 2);
    fake.fail_on("create_commit", "state: landed", Failure::Transient);
    assert!(first.land(BatchId(1)).is_err());
    drop(first);
    fake.pr(1).open = false;

    let (mut restarted, sink) = forge(&fake);
    let observation = restarted.observe().unwrap();
    assert_eq!(
        observation.batches[0].state,
        BatchState::Landed { tip: tip.clone() }
    );
    restarted.retire(BatchId(1)).unwrap();
    restarted.observe().unwrap();
    assert_eq!(sink.lines(), landed_lines(&tip, &[2]));
    assert_eq!(
        marks(&fake, 1),
        ["queued", "testing"],
        "the process-death gap"
    );
    assert_eq!(
        marks(&fake, 2),
        ["queued", "testing", "landed"],
        "not greeted again"
    );
}

#[test]
fn retire_deletes_the_batch_branch_and_the_record() {
    let fake = FakeGitHub::new();
    let (mut forge, _, _) = passed_batch(&fake, 2);

    forge.retire(BatchId(1)).unwrap();

    assert!(fake.get(&batch_ref(1)).is_none());
    assert!(fake.get("refs/dorfq/batches/1").is_none());
    assert!(fake.calls_to(&["remove_label"]).is_empty());
    assert!(forge.observe().unwrap().batches.is_empty());
}

#[test]
fn a_retire_cut_short_at_the_record_leaves_the_batch_to_retire_again() {
    let fake = FakeGitHub::new();
    let (mut forge, _, tip) = passed_batch(&fake, 2);
    fake.fail_on("delete_ref", "refs/dorfq/batches/1", Failure::Transient);

    let error = forge.retire(BatchId(1)).unwrap_err();
    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert_eq!(fake.get(&batch_ref(1)), Some(tip.clone()));

    let observation = forge.observe().unwrap();
    assert_eq!(
        observation.batches[0].state,
        BatchState::Testing {
            tip,
            ci: CiStatus::Passed
        }
    );
    forge.retire(BatchId(1)).unwrap();
    assert!(fake.get(&batch_ref(1)).is_none());
    assert!(fake.get("refs/dorfq/batches/1").is_none());
}

#[test]
fn a_retire_cut_short_at_the_batch_branch_reads_back_as_retired() {
    let fake = FakeGitHub::new();
    let (mut forge, _, tip) = passed_batch(&fake, 2);
    fake.fail_on("delete_ref", &batch_ref(1), Failure::Transient);

    let error = forge.retire(BatchId(1)).unwrap_err();
    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert!(fake.get("refs/dorfq/batches/1").is_none());
    assert_eq!(fake.get(&batch_ref(1)), Some(tip));

    assert!(forge.observe().unwrap().batches.is_empty());
}

#[test]
fn retire_tolerates_refs_already_gone_and_touches_no_labels() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 2);
    fake.state().conflicts_with_trunk.insert(heads[0].clone());
    let (mut forge, _) = observed(&fake);
    forge
        .start_batch(&start(1, &fake.trunk(), &[1, 2]))
        .unwrap();
    forge.observe().unwrap();
    fake.state().refs.remove("refs/dorfq/batches/1");

    forge.retire(BatchId(1)).unwrap();
    assert!(fake.calls_to(&["remove_label"]).is_empty());

    let fake = FakeGitHub::new();
    let (mut forge, sink, tip) = passed_batch(&fake, 1);
    fake.advance_trunk();
    refused(&fake, &mut forge, &sink, &tip);
    forge.retire(BatchId(1)).unwrap();
    assert!(fake.calls_to(&["remove_label"]).is_empty());
}

#[test]
fn retire_of_an_unknown_batch_is_permanent_and_writes_nothing() {
    let fake = FakeGitHub::new();
    let (mut forge, _) = observed(&fake);
    let error = forge.retire(BatchId(4)).unwrap_err();
    assert!(is_permanent(&error, "batch 4 is not known"), "{error}");
    assert!(fake.writes().is_empty());
}

#[test]
fn eject_removes_the_label_and_tells_the_reason() {
    let reasons = [
        (
            CONFLICTS_WITH_TRUNK,
            "conflicts with trunk. Merge or rebase onto `main`",
            "conflicts with trunk",
        ),
        (
            EjectReason::Conflicts {
                with: ConflictWith::Pr(PrNumber(2)),
            },
            "conflicts with #2",
            "conflicts with #2",
        ),
        (
            EjectReason::BatchFailed { batch: BatchId(5) },
            "batch 5 failed. The batch failed its checks on `dorfq/batch/5`",
            "batch 5 failed",
        ),
    ];
    for (reason, body, line) in reasons {
        let fake = FakeGitHub::new();
        queued(&fake, 2);
        let (mut forge, sink) = observed(&fake);

        forge.eject(PrNumber(1), &reason).unwrap();

        assert!(fake.pr(1).labels.is_empty());
        let last = fake.comments(1).last().unwrap().clone();
        assert!(last.starts_with("<!-- dorfq: ejected -->"), "{last}");
        assert!(last.contains(body), "{last}");
        forge.observe().unwrap();
        forge.observe().unwrap();
        assert_eq!(sink.lines(), [format!("message to pr 1: ejected: {line}")]);
    }
}

#[test]
fn eject_tolerates_a_label_already_removed() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, sink) = observed(&fake);
    fake.pr(1).labels.clear();

    forge.eject(PrNumber(1), &CONFLICTS_WITH_TRUNK).unwrap();
    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(
        sink.lines(),
        ["message to pr 1: ejected: conflicts with trunk"]
    );
}

#[test]
fn eject_of_an_unknown_pull_request_is_permanent_and_writes_nothing() {
    let fake = FakeGitHub::new();
    let (mut forge, _) = observed(&fake);
    let error = forge.eject(PrNumber(3), &CONFLICTS_WITH_TRUNK).unwrap_err();
    assert!(is_permanent(&error, "pr 3"), "{error}");
    assert!(fake.writes().is_empty());
}

const BROKEN: NoticeReason = NoticeReason::Broken { batch: BatchId(5) };

#[test]
fn notify_tells_the_reason_and_removes_nothing() {
    let reasons = [
        (
            BROKEN,
            "probably broken against `main`. Batch 5 (`dorfq/batch/5`) failed",
            "probably broken against trunk (batch 5)",
        ),
        (
            NoticeReason::InteractsWith {
                pr: PrNumber(2),
                batch: BatchId(5),
            },
            "fails together with #2, last in batch 5 (`dorfq/batch/5`)",
            "probably fails together with #2 (batch 5)",
        ),
    ];
    for (reason, body, line) in reasons {
        let fake = FakeGitHub::new();
        queued(&fake, 2);
        let (mut forge, sink) = observed(&fake);

        forge.notify(PrNumber(1), &reason).unwrap();

        assert_eq!(fake.pr(1).labels, ["queue"]);
        let last = fake.comments(1).last().unwrap().clone();
        assert!(last.starts_with("<!-- dorfq: notice -->"), "{last}");
        assert!(last.contains(body), "{last}");
        assert_eq!(sink.lines(), [format!("message to pr 1: notice: {line}")]);
        let observation = forge.observe().unwrap();
        assert_eq!(observation.prs[0].number, PrNumber(1));
        assert!(observation.departed.is_empty());
        forge.observe().unwrap();
        assert_eq!(
            sink.lines().len(),
            1,
            "a notice is not followed by a greeting"
        );
    }
}

#[test]
fn notify_of_an_unknown_pull_request_is_permanent_and_writes_nothing() {
    let fake = FakeGitHub::new();
    let (mut forge, _) = observed(&fake);
    let error = forge.notify(PrNumber(3), &BROKEN).unwrap_err();
    assert!(is_permanent(&error, "notify: pr 3 is not known"), "{error}");
    assert!(fake.writes().is_empty());
}

#[test]
fn notify_of_a_member_landed_in_this_plan_is_permanent() {
    let fake = FakeGitHub::new();
    let (mut forge, _, _) = passed_batch(&fake, 1);
    forge.land(BatchId(1)).unwrap();
    let writes = fake.writes().len();
    let error = forge.notify(PrNumber(1), &BROKEN).unwrap_err();
    assert!(
        is_permanent(
            &error,
            "notify: pr 1 is a member of a batch this plan lands"
        ),
        "{error}"
    );
    assert_eq!(fake.writes().len(), writes);
}

#[test]
fn notify_of_the_same_pull_request_twice_in_one_plan_is_permanent_and_posts_once() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, sink) = observed(&fake);
    forge.notify(PrNumber(1), &BROKEN).unwrap();
    let error = forge.notify(PrNumber(1), &BROKEN).unwrap_err();
    assert!(
        is_permanent(&error, "notify: pr 1 was notified in this plan"),
        "{error}"
    );
    assert_eq!(fake.calls_to(&["create_comment"]).len(), 1);
    assert_eq!(sink.lines().len(), 1);
}

#[test]
fn eject_of_a_pull_request_notified_in_this_plan_is_permanent_and_writes_nothing() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _) = observed(&fake);
    forge.notify(PrNumber(1), &BROKEN).unwrap();
    let writes = fake.writes().len();
    let error = forge.eject(PrNumber(1), &CONFLICTS_WITH_TRUNK).unwrap_err();
    assert!(
        is_permanent(&error, "eject: pr 1 was notified in this plan"),
        "{error}"
    );
    assert_eq!(fake.writes().len(), writes);
    assert_eq!(fake.pr(1).labels, ["queue"]);
}

#[test]
fn a_notice_posts_through_the_outbox() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, sink) = observed(&fake);

    fake.fail("create_comment", Failure::Transient);
    forge.notify(PrNumber(1), &BROKEN).unwrap();
    assert_eq!(fake.comments(1).len(), 1);
    assert!(sink.lines().is_empty());

    forge.observe().unwrap();
    assert_eq!(marks(&fake, 1), ["queued", "notice"]);
    assert_eq!(
        sink.lines(),
        ["message to pr 1: notice: probably broken against trunk (batch 5)"]
    );
}

#[test]
fn an_ejected_pull_request_relabelled_is_greeted_again() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, sink) = observed(&fake);
    forge.eject(PrNumber(1), &CONFLICTS_WITH_TRUNK).unwrap();
    assert!(forge.observe().unwrap().prs.is_empty());

    fake.pr(1).labels.push("queue".to_owned());
    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(
        sink.lines(),
        [
            "message to pr 1: ejected: conflicts with trunk",
            "message to pr 1: queued"
        ]
    );
}

/// Opens pull request `number` stacked on `on`, passing and already greeted;
/// its head.
fn stacked(fake: &FakeGitHub, number: u64, on: u64) -> Sha {
    let head = fake.stack_pr(number, on);
    passed(fake, &head);
    fake.state().comments.insert(number, vec![marker("queued")]);
    head
}

/// Each pull request a new observation reports, by number and base.
fn bases(forge: &mut GitHubForge) -> Vec<(u64, PrBase)> {
    bases_of(&forge.observe().unwrap())
}

/// Each pull request `observation` reports, by number and base.
fn bases_of(observation: &Observation) -> Vec<(u64, PrBase)> {
    observation
        .prs
        .iter()
        .map(|p| (p.number.0, p.base))
        .collect()
}

fn on(n: u64) -> PrBase {
    PrBase::Pr(PrNumber(n))
}

#[test]
fn a_labelled_child_of_a_labelled_base_is_enqueued_on_it() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 1);
    let child = stacked(&fake, 2, 1);
    let (mut forge, _) = forge(&fake);

    let observation = forge.observe().unwrap();
    let got: Vec<(u64, Sha, PrBase, PrStatus)> = observation
        .prs
        .into_iter()
        .map(|p| (p.number.0, p.head, p.base, p.status))
        .collect();
    assert_eq!(
        got,
        [
            (1, heads[0].clone(), PrBase::Trunk, PrStatus::Ready),
            (2, child, on(1), PrStatus::Ready),
        ]
    );
}

#[test]
fn a_deep_stack_and_a_tree_are_reported_closed_and_sorted() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    stacked(&fake, 5, 1);
    stacked(&fake, 3, 5);
    stacked(&fake, 2, 1);
    stacked(&fake, 4, 3);
    let (mut forge, _) = forge(&fake);

    assert_eq!(
        bases(&mut forge),
        [
            (1, PrBase::Trunk),
            (2, on(1)),
            (3, on(5)),
            (4, on(3)),
            (5, on(1)),
        ]
    );
}

#[test]
fn a_child_of_an_unlabelled_base_or_of_a_plain_branch_is_not_enqueued() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    fake.pr(1).labels.clear();
    stacked(&fake, 2, 1);
    stacked(&fake, 3, 2);
    let plain = ready_pr(&fake, 4);
    fake.set_ref(&branch("release"), &plain);
    fake.pr(4).base = "release".to_owned();
    let (mut forge, _) = forge(&fake);

    assert_eq!(bases(&mut forge), []);
}

#[test]
fn a_base_cycle_enqueues_neither_side() {
    let fake = FakeGitHub::new();
    queued(&fake, 3);
    fake.pr(1).base = "pr-2".to_owned();
    fake.pr(2).base = "pr-1".to_owned();
    let (mut forge, _) = forge(&fake);

    assert_eq!(bases(&mut forge), [(3, PrBase::Trunk)]);
}

#[test]
fn a_forks_pull_request_stacks_on_a_local_base_but_nothing_stacks_on_it() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    stacked(&fake, 2, 1);
    stacked(&fake, 3, 2);
    fake.pr(2).head_branch = None;
    let (mut forge, _) = forge(&fake);

    assert_eq!(bases(&mut forge), [(1, PrBase::Trunk), (2, on(1))]);
}

#[test]
fn a_shared_head_branch_bases_a_child_on_the_lowest_numbered_holder() {
    let fake = FakeGitHub::new();
    queued(&fake, 2);
    fake.pr(1).head_branch = Some("pr-2".to_owned());
    stacked(&fake, 3, 2);
    let (mut forge, _) = forge(&fake);

    assert_eq!(
        bases(&mut forge),
        [(1, PrBase::Trunk), (2, PrBase::Trunk), (3, on(1))]
    );
}

#[test]
fn a_child_that_does_not_merge_into_its_base_conflicts_with_it() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    stacked(&fake, 2, 1);
    fake.pr(2).mergeable = Some(false);
    let (mut forge, _) = forge(&fake);

    assert_eq!(
        statuses(&mut forge),
        [
            PrStatus::Ready,
            PrStatus::Conflicting(ConflictWith::Pr(PrNumber(1)))
        ]
    );
}

#[test]
fn a_child_whose_base_moved_waits_on_it_until_rebased() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 1);
    stacked(&fake, 2, 1);
    let (mut forge, sink) = observed(&fake);
    assert!(sink.lines().is_empty(), "{:?}", sink.lines());

    let moved = fake.commit(std::slice::from_ref(&heads[0]));
    passed(&fake, &moved);
    fake.push(1, &moved);
    assert_eq!(
        statuses(&mut forge),
        [
            PrStatus::Ready,
            waiting(&[Hold::AncestorMoved { pr: PrNumber(1) }])
        ]
    );
    forge.observe().unwrap();
    assert_eq!(sink.lines(), ["message to pr 2: held: ancestor #1 moved"]);
    assert!(fake.comments(2)[1].contains("ancestor #1 moved"));

    let rebased = fake.commit(std::slice::from_ref(&moved));
    passed(&fake, &rebased);
    fake.push(2, &rebased);
    assert_eq!(statuses(&mut forge), [PrStatus::Ready, PrStatus::Ready]);
}

/// Asserts that, among the calls from index `since` on, `pr` was moved onto
/// trunk before trunk's ref was updated.
fn moved_before_the_fast_forward(fake: &FakeGitHub, pr: u64, since: usize) {
    let calls = fake.state().calls[since..].to_vec();
    let moved = calls
        .iter()
        .position(|c| *c == format!("set_base {pr} main"))
        .unwrap_or_else(|| panic!("#{pr} never moved: {calls:?}"));
    let fast_forward = calls
        .iter()
        .position(|c| c.starts_with("update_ref refs/heads/main "))
        .unwrap_or_else(|| panic!("trunk never updated: {calls:?}"));
    assert!(moved < fast_forward, "{calls:?}");
}

#[test]
fn a_stack_in_one_batch_builds_tests_and_lands() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 1);
    let child = stacked(&fake, 2, 1);
    let (mut forge, sink) = observed(&fake);

    forge
        .start_batch(&start(1, &fake.trunk(), &[1, 2]))
        .unwrap();
    let tip = fake.get(&batch_ref(1)).expect("published");
    assert_eq!(
        record_of(&fake, 1).members,
        [member(1, &heads[0]), member(2, &child)]
    );
    passed(&fake, &tip);
    forge.observe().unwrap();
    sink.0.borrow_mut().clear();

    forge.land(BatchId(1)).unwrap();

    assert_eq!(fake.trunk(), tip);
    assert!(fake.pr(1).labels.is_empty());
    assert!(fake.pr(2).labels.is_empty());
    assert_eq!(fake.calls_to(&["set_base"]), ["set_base 2 main"]);
    moved_before_the_fast_forward(&fake, 2, 0);
    assert_eq!(fake.pr(2).base, "main");
    assert_eq!(sink.lines(), landed_lines(&tip, &[1, 2]));
}

#[test]
fn a_failed_retarget_at_landing_leaves_the_member_landed() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    stacked(&fake, 2, 1);
    stacked(&fake, 3, 2);
    let (mut forge, sink, tip) = landed_with_the_bases_move_failed(&fake, &[1, 2, 3]);

    assert_eq!(
        record_of(&fake, 1).state,
        RecordState::Landed { tip: tip.clone() }
    );
    assert_eq!(fake.pr(3).base, "main", "the next member moved");
    moved_before_the_fast_forward(&fake, 3, 0);
    assert!(fake.pr(3).labels.is_empty());
    let observation = forge.observe().unwrap();
    assert_eq!(
        observation.batches[0].state,
        BatchState::Landed { tip: tip.clone() }
    );
    assert!(observation.prs.is_empty(), "{:?}", observation.prs);
    assert!(
        observation
            .departed
            .iter()
            .all(|d| d.how == Departed::Landed { batch: BatchId(1) }),
        "{:?}",
        observation.departed
    );
    forge.retire(BatchId(1)).unwrap();
    assert_eq!(fake.get(&batch_ref(1)), None);
    let mut expected: Vec<String> = [1, 2, 3]
        .map(|n| format!("message to pr {n}: testing in batch 1"))
        .into();
    expected.extend(landed_lines(&tip, &[1, 2, 3]));
    assert_eq!(sink.lines(), expected);
}

#[test]
fn a_transient_retarget_at_landing_fails_the_landing_and_the_next_lands() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    stacked(&fake, 2, 1);
    stacked(&fake, 3, 2);
    let (mut forge, sink) = observed(&fake);
    let base = fake.trunk();
    forge.start_batch(&start(1, &base, &[1, 2, 3])).unwrap();
    let tip = fake.get(&batch_ref(1)).expect("published");
    passed(&fake, &tip);
    forge.observe().unwrap();
    sink.0.borrow_mut().clear();
    fake.fail_on("set_base", "3", Failure::Transient);

    let error = forge.land(BatchId(1)).unwrap_err();
    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert_eq!(fake.trunk(), base);
    assert_eq!(
        record_of(&fake, 1).state,
        RecordState::Landing { tip: tip.clone() }
    );
    assert!(
        fake.calls_to(&["update_ref"])
            .iter()
            .all(|c| !c.starts_with("update_ref refs/heads/main "))
    );
    assert_eq!(fake.pr(2).base, "pr-1", "moved back");
    assert_eq!(fake.pr(3).base, "pr-2");
    let observation = forge.observe().unwrap();
    assert_eq!(
        bases_of(&observation),
        [(1, PrBase::Trunk), (2, on(1)), (3, on(2))]
    );
    assert_eq!(
        observation.batches[0].state,
        BatchState::Testing {
            tip: tip.clone(),
            ci: CiStatus::Passed
        }
    );

    forge.land(BatchId(1)).unwrap();

    assert_eq!(fake.trunk(), tip);
    assert_eq!(
        fake.calls_to(&["set_base"]),
        [
            "set_base 2 main",
            "set_base 3 main",
            "set_base 2 pr-1",
            "set_base 3 pr-2",
            "set_base 2 main",
            "set_base 3 main"
        ]
    );
    let observation = forge.observe().unwrap();
    assert!(observation.prs.is_empty(), "{:?}", observation.prs);
    assert_eq!(
        observation.departed,
        [1, 2, 3].map(|n| Departure::new(
            PrNumber(n),
            fake.pr(n).head.clone(),
            Departed::Landed { batch: BatchId(1) }
        ))
    );
    assert_eq!(sink.lines(), landed_lines(&tip, &[1, 2, 3]));
}

#[test]
fn a_refused_fast_forward_after_the_moves_puts_the_stack_back() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    stacked(&fake, 2, 1);
    let (mut forge, sink) = observed(&fake);
    forge
        .start_batch(&start(1, &fake.trunk(), &[1, 2]))
        .unwrap();
    let tip = fake.get(&batch_ref(1)).expect("published");
    passed(&fake, &tip);
    forge.observe().unwrap();
    sink.0.borrow_mut().clear();
    let moved = fake.commit(&[fake.trunk()]);
    fake.state().interpose = Some((branch("main"), moved.clone()));

    forge.land(BatchId(1)).unwrap();

    let reason = LandRefusal::TrunkMoved { trunk: moved };
    assert_eq!(
        record_of(&fake, 1).state,
        RecordState::Refused {
            tip: tip.clone(),
            reason: reason.clone()
        }
    );
    assert_eq!(
        fake.calls_to(&["set_base"]),
        ["set_base 2 main", "set_base 2 pr-1"]
    );
    moved_before_the_fast_forward(&fake, 2, 0);
    assert_eq!(fake.pr(2).base, "pr-1");
    assert_eq!(fake.pr(1).labels, ["queue"]);
    assert_eq!(fake.pr(2).labels, ["queue"]);
    let observation = forge.observe().unwrap();
    assert_eq!(bases_of(&observation), [(1, PrBase::Trunk), (2, on(1))]);
    assert_eq!(
        observation.batches[0].state,
        BatchState::LandRefused { tip, reason }
    );
}

/// A forge that has observed the stack 2 on 1 tested in batch 1, passed.
fn a_stack_ready_to_land(fake: &FakeGitHub) -> (GitHubForge, Sha) {
    queued(fake, 1);
    stacked(fake, 2, 1);
    let (mut forge, _) = observed(fake);
    forge
        .start_batch(&start(1, &fake.trunk(), &[1, 2]))
        .unwrap();
    let tip = fake.get(&batch_ref(1)).expect("published");
    passed(fake, &tip);
    forge.observe().unwrap();
    (forge, tip)
}

#[test]
fn a_child_moved_back_after_a_failed_fast_forward_waits_on_its_base_moving() {
    let fake = FakeGitHub::new();
    let (mut forge, _) = a_stack_ready_to_land(&fake);
    let base = fake.trunk();
    fake.fail_on("update_ref", "refs/heads/main ", Failure::Transient);

    let error = forge.land(BatchId(1)).unwrap_err();

    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert_eq!(fake.trunk(), base);
    assert_eq!(fake.pr(2).base, "pr-1", "moved back");
    let head = fake.pr(1).head.clone();
    let pushed = fake.commit(&[head]);
    passed(&fake, &pushed);
    fake.push(1, &pushed);
    let observation = forge.observe().unwrap();
    assert_eq!(bases_of(&observation), [(1, PrBase::Trunk), (2, on(1))]);
    assert_eq!(
        observation.prs[1].status,
        waiting(&[
            Hold::MergeabilityUnknown,
            Hold::AncestorMoved { pr: PrNumber(1) }
        ])
    );
}

#[test]
fn a_move_whose_response_is_lost_is_put_back() {
    let fake = FakeGitHub::new();
    let (mut forge, _) = a_stack_ready_to_land(&fake);
    let base = fake.trunk();
    fake.state().lose_set_base = Some(2);

    let error = forge.land(BatchId(1)).unwrap_err();

    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert_eq!(fake.trunk(), base);
    assert_eq!(
        fake.calls_to(&["set_base"]),
        ["set_base 2 main", "set_base 2 pr-1"]
    );
    assert_eq!(fake.pr(2).base, "pr-1", "moved back");
    let observation = forge.observe().unwrap();
    assert_eq!(bases_of(&observation), [(1, PrBase::Trunk), (2, on(1))]);
}

#[test]
fn a_fast_forward_whose_response_is_lost_leaves_the_moves_in_place() {
    let fake = FakeGitHub::new();
    let (mut forge, tip) = a_stack_ready_to_land(&fake);
    fake.state().lose_update = Some(branch("main"));

    let error = forge.land(BatchId(1)).unwrap_err();

    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert_eq!(fake.trunk(), tip);
    assert_eq!(fake.calls_to(&["set_base"]), ["set_base 2 main"]);
    assert_eq!(fake.pr(2).base, "main");
    let observation = forge.observe().unwrap();
    assert_eq!(observation.batches[0].state, BatchState::Landed { tip });
}

#[test]
fn land_refuses_a_child_whose_base_changed_since_the_observation() {
    for how in ["onto trunk", "base renamed"] {
        let fake = FakeGitHub::new();
        queued(&fake, 1);
        stacked(&fake, 2, 1);
        let (mut forge, sink) = observed(&fake);
        forge
            .start_batch(&start(1, &fake.trunk(), &[1, 2]))
            .unwrap();
        let tip = fake.get(&batch_ref(1)).expect("published");
        passed(&fake, &tip);
        forge.observe().unwrap();
        sink.0.borrow_mut().clear();
        match how {
            "onto trunk" => fake.pr(2).base = "main".to_owned(),
            _ => fake.pr(2).base = "renamed".to_owned(),
        }

        forge.land(BatchId(1)).unwrap();

        assert_ne!(fake.trunk(), tip, "{how}");
        let RecordState::Refused { reason, .. } = record_of(&fake, 1).state else {
            panic!("{how}: not refused: {:?}", record_of(&fake, 1).state);
        };
        assert_eq!(reason, LandRefusal::MemberGone { pr: PrNumber(2) }, "{how}");
        assert!(fake.calls_to(&["set_base"]).is_empty(), "{how}");
    }
}

#[test]
fn a_child_is_based_only_on_a_pull_request_enqueued_before_it() {
    // 1 and 2 each target the other's head branch, which 4 and 3 also carry
    // from trunk: basing each on the lowest-numbered holder of its base
    // branch overall would make 1 and 2 each other's base.
    let fake = FakeGitHub::new();
    queued(&fake, 4);
    fake.pr(1).head_branch = Some("x".to_owned());
    fake.pr(4).head_branch = Some("x".to_owned());
    fake.pr(2).head_branch = Some("y".to_owned());
    fake.pr(3).head_branch = Some("y".to_owned());
    fake.pr(1).base = "y".to_owned();
    fake.pr(2).base = "x".to_owned();
    let (mut forge, _) = forge(&fake);

    assert_eq!(
        bases(&mut forge),
        [
            (1, on(3)),
            (2, on(4)),
            (3, PrBase::Trunk),
            (4, PrBase::Trunk)
        ]
    );
}

#[test]
fn a_blocked_pull_request_is_told_why_once() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    fake.pr(1).labels.clear();
    stacked(&fake, 2, 1);
    let plain = ready_pr(&fake, 3);
    fake.set_ref(&branch("release"), &plain);
    fake.pr(3).base = "release".to_owned();
    let (mut forge, sink) = observed(&fake);

    forge.observe().unwrap();
    assert_eq!(
        sink.lines(),
        [
            "message to pr 2: blocked on #1",
            "message to pr 3: blocked on branch release",
        ]
    );
    assert!(fake.comments(2)[1].contains("its base #1 is not queued"));
    assert!(fake.comments(3)[0].contains("it targets `release`, which is neither `main`"));
    forge.observe().unwrap();
    assert_eq!(sink.lines().len(), 2, "{:?}", sink.lines());
    assert!(fake.calls_to(&["set_base"]).is_empty());

    // Its base closed unmerged, its branch left in place: blocked on the
    // branch now, which is told once.
    fake.pr(1).open = false;
    forge.observe().unwrap();
    forge.observe().unwrap();
    assert_eq!(
        sink.lines()[2..],
        ["message to pr 2: blocked on branch pr-1"]
    );
    assert!(fake.comments(2)[2].contains("it targets `pr-1`"));
}

#[test]
fn a_retarget_does_not_classify_a_blocked_pull_request_again() {
    let (fake, mut forge, _) = base_merged_into_trunk("merged");
    let plain = ready_pr(&fake, 3);
    fake.set_ref(&branch("release"), &plain);
    fake.pr(3).base = "release".to_owned();
    let before = fake.calls_to(&["merged_pr_heads"]).len();

    forge.observe().unwrap();
    assert_eq!(fake.calls_to(&["set_base"]), ["set_base 2 main"]);
    assert_eq!(
        fake.calls_to(&["merged_pr_heads"])[before..],
        ["merged_pr_heads pr-1 main"]
    );
}

#[test]
fn a_blocked_pull_request_that_enters_the_queue_is_greeted_again() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    fake.pr(1).labels.clear();
    stacked(&fake, 2, 1);
    let (mut forge, sink) = observed(&fake);
    forge.observe().unwrap();

    fake.pr(1).labels.push("queue".to_owned());
    assert_eq!(bases(&mut forge), [(1, PrBase::Trunk), (2, on(1))]);
    forge.observe().unwrap();
    assert_eq!(
        sink.lines(),
        ["message to pr 2: blocked on #1", "message to pr 2: queued"]
    );
}

/// Pull request 1 against trunk and 2 stacked on it, both queued, with 1
/// gone from the queue by `how`, its content in trunk: open, unlabelled and
/// fast-forwarded into trunk; closed and merged into trunk; or squashed into
/// trunk, which leaves its branch outside trunk's history.
fn base_merged_into_trunk(how: &str) -> (FakeGitHub, GitHubForge, Buffer) {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 1);
    stacked(&fake, 2, 1);
    let (forge, sink) = observed(&fake);
    match how {
        "open" => {
            fake.pr(1).labels.clear();
            fake.set_ref(&branch("main"), &heads[0]);
        }
        "merged" => {
            let mut one = fake.pr(1);
            one.open = false;
            one.merged = true;
            drop(one);
            fake.set_ref(&branch("main"), &heads[0]);
        }
        _ => {
            let mut one = fake.pr(1);
            one.open = false;
            one.merged = true;
            drop(one);
            fake.advance_trunk();
        }
    }
    (fake, forge, sink)
}

#[test]
fn a_child_whose_base_merged_into_trunk_is_moved_onto_trunk() {
    for how in ["open", "merged", "squashed"] {
        let (fake, mut forge, _) = base_merged_into_trunk(how);

        let observation = forge.observe().unwrap();
        assert_eq!(fake.calls_to(&["set_base"]), ["set_base 2 main"], "{how}");
        assert_eq!(fake.pr(2).base, "main", "{how}");
        let got: Vec<(u64, PrBase, PrStatus)> = observation
            .prs
            .into_iter()
            .map(|p| (p.number.0, p.base, p.status))
            .collect();
        assert_eq!(
            got,
            [(2, PrBase::Trunk, waiting(&[Hold::MergeabilityUnknown]))],
            "{how}"
        );
        assert!(
            observation.departed.iter().all(|d| d.pr == PrNumber(1)),
            "{how}: {:?}",
            observation.departed
        );

        fake.pr(2).mergeable = Some(true);
        assert_eq!(statuses(&mut forge), [PrStatus::Ready], "{how}");
        assert_eq!(fake.calls_to(&["set_base"]).len(), 1, "{how}");
    }
}

#[test]
fn a_branch_no_trunk_merge_left_behind_is_never_retargeted() {
    for how in [
        "never a head",
        "moved on",
        "merged elsewhere",
        "open elsewhere, head in trunk, child never queued on it",
    ] {
        let fake = FakeGitHub::new();
        let heads = queued(&fake, 1);
        let child = stacked(&fake, 2, 1);
        let expected = match how {
            "never a head" => {
                fake.state().prs.remove(&1);
                fake.state().refs.remove(&branch("pr-1"));
                fake.set_ref(&branch("level"), &fake.trunk());
                fake.pr(2).base = "level".to_owned();
                "message to pr 2: blocked on branch level"
            }
            "moved on" => {
                let mut one = fake.pr(1);
                one.open = false;
                one.merged = true;
                drop(one);
                fake.set_ref(&branch("main"), &heads[0]);
                let moved = fake.commit(std::slice::from_ref(&child));
                fake.set_ref(&branch("pr-1"), &moved);
                "message to pr 2: blocked on branch pr-1"
            }
            "merged elsewhere" => {
                let mut one = fake.pr(1);
                one.open = false;
                one.merged = true;
                one.base = "release".to_owned();
                drop(one);
                "message to pr 2: blocked on branch pr-1"
            }
            _ => {
                // The base is open against another branch with its head in
                // trunk, and this forge never reported the child stacked on
                // it.
                let mut one = fake.pr(1);
                one.labels.clear();
                one.base = "release".to_owned();
                drop(one);
                fake.set_ref(&branch("main"), &heads[0]);
                "message to pr 2: blocked on branch pr-1"
            }
        };
        let (mut forge, sink) = forge(&fake);

        assert_eq!(bases(&mut forge), [], "{how}");
        forge.observe().unwrap();
        assert_eq!(sink.lines(), [expected], "{how}");
        assert!(fake.calls_to(&["set_base"]).is_empty(), "{how}");
    }
}

#[test]
fn a_child_queued_on_a_base_open_elsewhere_with_its_head_in_trunk_is_blocked_on_it() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 1);
    let child = stacked(&fake, 2, 1);
    let (mut forge, sink) = observed(&fake);
    let mut one = fake.pr(1);
    one.labels.clear();
    one.base = "release".to_owned();
    drop(one);
    fake.set_ref(&branch("main"), &heads[0]);

    let observation = forge.observe().unwrap();
    assert_eq!(bases_of(&observation), []);
    assert!(observation.departed.contains(&Departure::new(
        PrNumber(2),
        child,
        Departed::Withdrawn(Withdrawal::Blocked { on: PrNumber(1) })
    )));
    assert!(fake.calls_to(&["set_base"]).is_empty());
    assert_eq!(fake.pr(2).base, "pr-1");
    forge.observe().unwrap();
    assert_eq!(lines_for(&sink, 2), ["message to pr 2: blocked on #1"]);
}

#[test]
fn a_transient_retarget_fails_the_observation_and_the_next_one_retargets() {
    let (fake, mut forge, _) = base_merged_into_trunk("merged");
    fake.fail("set_base", Failure::Transient);

    let error = forge.observe().unwrap_err();
    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert_eq!(bases(&mut forge), [(2, PrBase::Trunk)]);
}

#[test]
fn a_refused_retarget_withdraws_the_child_once_and_tells_its_author() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    stacked(&fake, 2, 1);
    let (mut forge, sink) = observed(&fake);
    forge.start_batch(&start(1, &fake.trunk(), &[1])).unwrap();
    let tip = fake.get(&batch_ref(1)).expect("published");
    passed(&fake, &tip);
    forge.observe().unwrap();
    forge.land(BatchId(1)).unwrap();
    sink.0.borrow_mut().clear();
    fake.fail("set_base", Failure::Permanent);
    let set_base_before = fake.calls_to(&["set_base"]).len();

    let observation = forge.observe().unwrap();
    assert!(observation.prs.is_empty(), "{:?}", observation.prs);
    let child = fake.pr(2).head.clone();
    assert!(observation.departed.contains(&Departure::new(
        PrNumber(2),
        child,
        Departed::Withdrawn(Withdrawal::Retargeted)
    )));
    let observation = forge.observe().unwrap();
    assert!(
        observation.departed.iter().all(|d| d.pr != PrNumber(2)),
        "{:?}",
        observation.departed
    );
    forge.observe().unwrap();
    assert_eq!(sink.lines(), ["message to pr 2: blocked on branch pr-1"]);
    assert_eq!(fake.pr(2).base, "pr-1");
    assert_eq!(
        fake.calls_to(&["set_base"])[set_base_before..],
        ["set_base 2 main"]
    );
}

#[test]
fn a_child_never_reported_on_a_base_that_merged_into_trunk_is_never_retargeted() {
    // A release branch back-merged into trunk and left in place: the
    // labelled pull request on it was never reported stacked on anything.
    for how in ["closed", "open"] {
        let fake = FakeGitHub::new();
        let heads = queued(&fake, 1);
        fake.pr(1).labels.clear();
        stacked(&fake, 2, 1);
        let (mut forge, sink) = observed(&fake);
        if how == "closed" {
            let mut one = fake.pr(1);
            one.open = false;
            one.merged = true;
        }
        fake.set_ref(&branch("main"), &heads[0]);

        assert_eq!(bases(&mut forge), [], "{how}");
        forge.observe().unwrap();
        assert_eq!(
            sink.lines(),
            [
                "message to pr 2: blocked on #1",
                "message to pr 2: blocked on branch pr-1"
            ],
            "{how}"
        );
        assert!(fake.calls_to(&["set_base"]).is_empty(), "{how}");
        assert!(fake.calls_to(&["merged_pr_heads"]).is_empty(), "{how}");
    }
}

#[test]
fn a_child_whose_base_merged_before_a_restart_is_told_to_retarget() {
    let (fake, _, _) = base_merged_into_trunk("merged");
    let (mut forge, sink) = forge(&fake);

    assert_eq!(bases(&mut forge), []);
    forge.observe().unwrap();
    assert_eq!(sink.lines(), ["message to pr 2: blocked on branch pr-1"]);
    assert!(fake.calls_to(&["set_base"]).is_empty());
}

/// The lines `sink` holds for pull request `n`.
fn lines_for(sink: &Buffer, n: u64) -> Vec<String> {
    let prefix = format!("message to pr {n}: ");
    sink.lines()
        .into_iter()
        .filter(|l| l.starts_with(&prefix))
        .collect()
}

#[test]
fn a_child_whose_bases_branch_was_rewound_onto_trunk_is_not_moved() {
    for how in ["one tick", "observed first"] {
        let fake = FakeGitHub::new();
        queued(&fake, 1);
        stacked(&fake, 2, 1);
        let child = stacked(&fake, 3, 2);
        let (mut forge, sink) = observed(&fake);
        let two = fake.pr(2).head.clone();
        fake.push(2, &fake.trunk());
        if how == "observed first" {
            assert_eq!(
                bases(&mut forge),
                [(1, PrBase::Trunk), (2, on(1)), (3, on(2))]
            );
        }
        fake.pr(2).labels.clear();
        let rewound = fake.pr(2).head.clone();

        let observation = forge.observe().unwrap();
        assert!(fake.calls_to(&["set_base"]).is_empty(), "{how}");
        assert_eq!(fake.pr(3).base, "pr-2", "{how}");
        assert_eq!(bases_of(&observation), [(1, PrBase::Trunk)], "{how}");
        let reported_two = if how == "one tick" { two } else { rewound };
        assert_eq!(
            observation.departed,
            [
                Departure::new(
                    PrNumber(2),
                    reported_two,
                    Departed::Withdrawn(Withdrawal::IntentCleared)
                ),
                Departure::new(
                    PrNumber(3),
                    child.clone(),
                    Departed::Withdrawn(Withdrawal::Blocked { on: PrNumber(2) })
                ),
            ],
            "{how}"
        );
        forge.observe().unwrap();
        assert_eq!(
            lines_for(&sink, 3),
            ["message to pr 3: blocked on #2"],
            "{how}"
        );
    }
}

/// Batch 1 of `members`, among them 1 and 2 (stacked on 1), built, passed,
/// observed and landed, with GitHub refusing to move 2 onto trunk; the
/// forge, its sink and the tip. Pull requests the caller opened beforehand
/// are observed too.
fn landed_with_the_bases_move_failed(
    fake: &FakeGitHub,
    members: &[u64],
) -> (GitHubForge, Buffer, Sha) {
    let (mut forge, sink) = observed(fake);
    forge
        .start_batch(&start(1, &fake.trunk(), members))
        .unwrap();
    let tip = fake.get(&batch_ref(1)).expect("published");
    passed(fake, &tip);
    forge.observe().unwrap();
    fake.fail_on("set_base", "2", Failure::Permanent);
    forge.land(BatchId(1)).unwrap();
    assert_eq!(fake.trunk(), tip);
    assert_eq!(fake.pr(2).base, "pr-1");
    assert!(fake.pr(2).labels.is_empty());
    (forge, sink, tip)
}

#[test]
fn a_child_outside_the_batch_whose_bases_move_failed_is_moved_onto_trunk() {
    for how in ["permanent", "re-pushed"] {
        let fake = FakeGitHub::new();
        queued(&fake, 1);
        stacked(&fake, 2, 1);
        stacked(&fake, 3, 2);
        let (mut forge, sink, tip) = landed_with_the_bases_move_failed(&fake, &[1, 2]);
        let landed_heads = [1, 2].map(|n| fake.pr(n).head.clone());
        if how == "re-pushed" {
            let next = fake.commit(std::slice::from_ref(&landed_heads[1]));
            fake.push(2, &next);
        }
        let set_base_before = fake.calls_to(&["set_base"]).len();
        let ancestry_before = fake.calls_to(&["is_ancestor"]).len();

        let observation = forge.observe().unwrap();
        assert_eq!(
            fake.calls_to(&["set_base"])[set_base_before..],
            ["set_base 3 main"],
            "{how}"
        );
        assert_eq!(fake.pr(3).base, "main", "{how}");
        let got: Vec<(u64, PrBase, PrStatus)> = observation
            .prs
            .iter()
            .map(|p| (p.number.0, p.base, p.status.clone()))
            .collect();
        assert_eq!(
            got,
            [(3, PrBase::Trunk, waiting(&[Hold::MergeabilityUnknown]))],
            "{how}"
        );
        assert!(
            fake.calls_to(&["is_ancestor"])[ancestry_before..].is_empty(),
            "{how}"
        );
        assert_eq!(
            observation.departed,
            [1, 2].map(|n| Departure::new(
                PrNumber(n),
                landed_heads[n as usize - 1].clone(),
                Departed::Landed { batch: BatchId(1) }
            )),
            "{how}"
        );

        forge.retire(BatchId(1)).unwrap();
        fake.pr(3).mergeable = Some(true);
        forge.observe().unwrap();
        forge.start_batch(&start(2, &tip, &[3])).unwrap();
        let second = fake.get(&batch_ref(2)).expect("published");
        passed(&fake, &second);
        forge.observe().unwrap();
        forge.land(BatchId(2)).unwrap();

        assert_eq!(fake.trunk(), second, "{how}");
        let lines = sink.lines();
        let landed: Vec<String> = lines
            .iter()
            .filter(|l| l.contains("landed"))
            .cloned()
            .collect();
        let mut expected = landed_lines(&tip, &[1, 2]);
        expected.extend(landed_lines(&second, &[3]));
        assert_eq!(landed, expected, "{how}");
        assert!(
            !lines.iter().any(|l| l.contains("blocked")),
            "{how}: {lines:?}"
        );
    }
}

#[test]
fn a_child_never_queued_on_a_landed_base_whose_move_failed_is_told_to_retarget() {
    for how in ["restart", "labelled after"] {
        let fake = FakeGitHub::new();
        queued(&fake, 1);
        stacked(&fake, 2, 1);
        if how == "restart" {
            stacked(&fake, 3, 2);
        }
        let (landed, sink, _) = landed_with_the_bases_move_failed(&fake, &[1, 2]);
        let (mut forge, sink) = if how == "restart" {
            forge(&fake)
        } else {
            stacked(&fake, 3, 2);
            (landed, sink)
        };
        let set_base_before = fake.calls_to(&["set_base"]).len();

        let observation = forge.observe().unwrap();
        assert_eq!(fake.calls_to(&["set_base"]).len(), set_base_before, "{how}");
        assert_eq!(fake.pr(3).base, "pr-2", "{how}");
        assert_eq!(bases_of(&observation), [], "{how}");
        let departed: Vec<Departure> = if how == "restart" {
            vec![]
        } else {
            [1, 2]
                .map(|n| {
                    Departure::new(
                        PrNumber(n),
                        fake.pr(n).head.clone(),
                        Departed::Landed { batch: BatchId(1) },
                    )
                })
                .to_vec()
        };
        assert_eq!(observation.departed, departed, "{how}");
        forge.observe().unwrap();
        assert_eq!(
            lines_for(&sink, 3),
            ["message to pr 3: blocked on branch pr-2"],
            "{how}"
        );
    }
}

/// Batch 1 of 1 and 2 on trunk and 3 stacked on 1, passed and observed, its
/// landing cut short at 2's label after GitHub refused to move 3 onto trunk;
/// the forge that landed it, its sink, and the tip.
fn landing_cut_short_at_a_trunk_member() -> (FakeGitHub, GitHubForge, Buffer, Sha) {
    let fake = FakeGitHub::new();
    queued(&fake, 2);
    stacked(&fake, 3, 1);
    let (mut forge, sink) = observed(&fake);
    forge
        .start_batch(&start(1, &fake.trunk(), &[1, 2, 3]))
        .unwrap();
    let tip = fake.get(&batch_ref(1)).expect("published");
    passed(&fake, &tip);
    forge.observe().unwrap();
    sink.0.borrow_mut().clear();
    fake.fail_on("remove_label", "2", Failure::Transient);
    fake.fail_on("set_base", "3", Failure::Permanent);
    assert!(forge.land(BatchId(1)).is_err());
    assert_eq!(fake.trunk(), tip);
    assert_eq!(fake.calls_to(&["set_base"]), ["set_base 3 main"]);
    (fake, forge, sink, tip)
}

#[test]
fn a_landed_member_still_labelled_is_reported_on_trunk_and_completed_at_retire() {
    for restart in [false, true] {
        let (fake, landed, sink, tip) = landing_cut_short_at_a_trunk_member();
        let (mut forge, sink) = if restart {
            forge(&fake)
        } else {
            (landed, sink)
        };

        let observation = forge.observe().unwrap();
        assert_eq!(
            bases_of(&observation),
            [(2, PrBase::Trunk), (3, PrBase::Trunk)],
            "restart: {restart}"
        );
        assert_eq!(
            observation.batches[0].state,
            BatchState::Landed { tip: tip.clone() },
            "restart: {restart}"
        );
        let departed: &[Departure] = if restart {
            &[]
        } else {
            &[Departure::new(
                PrNumber(1),
                fake.pr(1).head.clone(),
                Departed::Landed { batch: BatchId(1) },
            )]
        };
        assert_eq!(observation.departed, departed, "restart: {restart}");

        forge.retire(BatchId(1)).unwrap();
        assert!(fake.pr(2).labels.is_empty(), "restart: {restart}");
        assert!(fake.pr(3).labels.is_empty(), "restart: {restart}");
        assert_eq!(fake.pr(3).base, "pr-1", "restart: {restart}");
        assert_eq!(
            fake.calls_to(&["set_base"]),
            ["set_base 3 main"],
            "restart: {restart}: the one move is the refused one at landing"
        );
        let observation = forge.observe().unwrap();
        assert_eq!(
            observation.departed,
            [2, 3].map(|n| Departure::new(
                PrNumber(n),
                fake.pr(n).head.clone(),
                Departed::Landed { batch: BatchId(1) }
            )),
            "restart: {restart}"
        );
        // A restart loses the first process's outbox, 1's message with it.
        let told: &[u64] = if restart { &[2, 3] } else { &[1, 2, 3] };
        assert_eq!(sink.lines(), landed_lines(&tip, told), "restart: {restart}");
    }
}

#[test]
fn a_child_landed_after_its_base_is_moved_onto_trunk_and_lands_as_usual() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    stacked(&fake, 2, 1);
    stacked(&fake, 3, 2);
    let (mut forge, sink) = observed(&fake);
    forge.start_batch(&start(1, &fake.trunk(), &[1])).unwrap();
    let tip = fake.get(&batch_ref(1)).expect("published");
    passed(&fake, &tip);
    forge.observe().unwrap();
    forge.land(BatchId(1)).unwrap();

    let observation = forge.observe().unwrap();
    assert_eq!(fake.calls_to(&["set_base"]), ["set_base 2 main"]);
    let got: Vec<(u64, PrBase)> = observation
        .prs
        .iter()
        .map(|p| (p.number.0, p.base))
        .collect();
    assert_eq!(got, [(2, PrBase::Trunk), (3, on(2))]);
    assert_eq!(observation.departed.len(), 1, "{:?}", observation.departed);
    forge.retire(BatchId(1)).unwrap();

    fake.pr(2).mergeable = Some(true);
    forge.observe().unwrap();
    forge.start_batch(&start(2, &tip, &[2, 3])).unwrap();
    let second = fake.get(&batch_ref(2)).expect("published");
    passed(&fake, &second);
    forge.observe().unwrap();
    sink.0.borrow_mut().clear();
    let calls_before = fake.state().calls.len();
    forge.land(BatchId(2)).unwrap();

    assert_eq!(fake.trunk(), second);
    let calls = fake.state().calls[calls_before..].to_vec();
    let moves: Vec<&String> = calls
        .iter()
        .filter(|c| c.starts_with("set_base "))
        .collect();
    assert_eq!(moves, ["set_base 3 main"]);
    moved_before_the_fast_forward(&fake, 3, calls_before);
    assert_eq!(sink.lines(), landed_lines(&second, &[2, 3]));
    assert!(
        !sink.lines().iter().any(|l| l.contains("blocked")),
        "{:?}",
        sink.lines()
    );
}

#[test]
fn a_child_whose_base_was_ejected_is_withdrawn_blocked_on_it() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 1);
    let child = stacked(&fake, 2, 1);
    let (mut forge, sink) = observed(&fake);
    forge.eject(PrNumber(1), &CONFLICTS_WITH_TRUNK).unwrap();
    let asked_before = fake.calls_to(&["get_pr"]).len();

    let observation = forge.observe().unwrap();
    assert_eq!(
        observation.departed,
        [
            Departure::new(PrNumber(1), heads[0].clone(), Departed::Ejected),
            Departure::new(
                PrNumber(2),
                child,
                Departed::Withdrawn(Withdrawal::Blocked { on: PrNumber(1) })
            ),
        ]
    );
    assert!(fake.calls_to(&["get_pr"])[asked_before..].is_empty());
    forge.observe().unwrap();
    assert_eq!(
        sink.lines(),
        [
            "message to pr 1: ejected: conflicts with trunk",
            "message to pr 2: blocked on #1",
        ]
    );
}

#[test]
fn a_child_the_listing_left_out_is_classified_from_its_detail() {
    for how in [
        "base enqueued",
        "merged base",
        "base unlabelled",
        "plain branch",
    ] {
        let fake = FakeGitHub::new();
        let heads = queued(&fake, 1);
        let child = stacked(&fake, 2, 1);
        let (mut forge, _) = observed(&fake);
        if how != "base enqueued" {
            fake.pr(1).labels.clear();
        }
        let expected = match how {
            "base enqueued" => None,
            "merged base" => {
                fake.set_ref(&branch("main"), &heads[0]);
                None
            }
            "base unlabelled" => Some(Withdrawal::Blocked { on: PrNumber(1) }),
            _ => {
                fake.set_ref(&branch("release"), &fake.trunk());
                fake.pr(2).base = "release".to_owned();
                Some(Withdrawal::Retargeted)
            }
        };
        fake.state().unlisted.insert(2);
        let writes_before = fake.writes().len();

        let outcome = forge.observe();
        match expected {
            None => {
                let error = outcome.unwrap_err();
                assert!(matches!(error, ForgeError::Transient(_)), "{error}");
                assert!(error.to_string().contains("#2"), "{how}: {error}");
                assert_eq!(fake.writes().len(), writes_before, "{how}");
            }
            Some(withdrawal) => {
                let departed = outcome.unwrap().departed;
                assert!(
                    departed.contains(&Departure::new(
                        PrNumber(2),
                        child,
                        Departed::Withdrawn(withdrawal)
                    )),
                    "{how}: {departed:?}"
                );
            }
        }
    }
}

#[test]
fn a_child_the_listing_left_out_on_a_landed_base_waits_for_the_listing() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    stacked(&fake, 2, 1);
    stacked(&fake, 3, 2);
    let (mut forge, _, _) = landed_with_the_bases_move_failed(&fake, &[1, 2]);
    fake.state().unlisted.insert(3);
    let writes_before = fake.writes().len();

    let error = forge.observe().unwrap_err();
    assert!(matches!(error, ForgeError::Transient(_)), "{error}");
    assert!(error.to_string().contains("#3"), "{error}");
    assert_eq!(fake.writes().len(), writes_before);
    assert_eq!(fake.pr(3).base, "pr-2");

    fake.state().unlisted.clear();
    let set_base_before = fake.calls_to(&["set_base"]).len();
    let observation = forge.observe().unwrap();
    assert_eq!(
        fake.calls_to(&["set_base"])[set_base_before..],
        ["set_base 3 main"]
    );
    assert_eq!(bases_of(&observation), [(3, PrBase::Trunk)]);
}

#[test]
fn an_ejected_child_is_told_to_rebase_onto_its_base() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    stacked(&fake, 2, 1);
    let (mut forge, _) = observed(&fake);

    let reason = EjectReason::Conflicts {
        with: ConflictWith::Pr(PrNumber(1)),
    };
    forge.eject(PrNumber(2), &reason).unwrap();

    let last = fake.comments(2).last().unwrap().clone();
    assert!(
        last.contains("It does not merge into its base #1. Rebase onto #1's branch"),
        "{last}"
    );
}

fn behind(observation: &Observation) -> Vec<(u64, u32)> {
    observation
        .prs
        .iter()
        .map(|p| (p.number.0, p.behind))
        .collect()
}

/// The `compare` calls made since `since` of them had been.
fn compares_since(fake: &FakeGitHub, since: usize) -> Vec<String> {
    fake.calls_to(&["compare"]).split_off(since)
}

fn compares(fake: &FakeGitHub) -> usize {
    fake.calls_to(&["compare"]).len()
}

/// Starts batch `id` over `prs` on trunk, passes it and lands it; trunk is
/// then its tip, which is returned.
fn land_batch(fake: &FakeGitHub, forge: &mut GitHubForge, id: u64, prs: &[u64]) -> Sha {
    forge.start_batch(&start(id, &fake.trunk(), prs)).unwrap();
    let tip = fake.get(&batch_ref(id)).expect("published");
    passed(fake, &tip);
    forge.observe().unwrap();
    forge.land(BatchId(id)).unwrap();
    assert_eq!(fake.trunk(), tip);
    tip
}

#[test]
fn behind_counts_the_queue_s_landings_by_pull_request_and_the_trunk_position_rises_with_them() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    fake.advance_trunk();
    fake.advance_trunk();
    let head = fake.open_pr(2);
    passed(&fake, &head);
    fake.state().comments.insert(2, vec![marker("queued")]);
    let (mut forge, _) = forge(&fake);
    let before = compares(&fake);
    let observation = forge.observe().unwrap();
    assert_eq!(observation.trunk_position, 0);
    // 1 forked before the first trunk counted: commits since its merge base.
    assert_eq!(behind(&observation), [(1, 2), (2, 0)]);
    assert_eq!(compares_since(&fake, before).len(), 2);

    // The queue lands 2: trunk gains its head and the batch's merge, one
    // pull request.
    land_batch(&fake, &mut forge, 1, &[2]);
    let before = compares(&fake);
    let observation = forge.observe().unwrap();
    assert_eq!(observation.trunk_position, 1);
    assert_eq!(behind(&observation), [(1, 3)]);
    assert_eq!(compares_since(&fake, before), Vec::<String>::new());

    // A re-push onto today's trunk covers it.
    let pushed = fake.commit(&[fake.trunk()]);
    fake.push(1, &pushed);
    passed(&fake, &pushed);
    let before = compares(&fake);
    assert_eq!(behind(&forge.observe().unwrap()), [(1, 0)]);
    assert_eq!(compares_since(&fake, before).len(), 1);
}

#[test]
fn a_head_forked_from_a_trunk_the_process_counted_is_covered_exactly() {
    let fake = FakeGitHub::new();
    queued(&fake, 2);
    // 3's branch forks from the first trunk, and is labelled later.
    let head = fake.open_pr(3);
    passed(&fake, &head);
    fake.pr(3).labels.clear();
    let (mut forge, _) = observed(&fake);
    land_batch(&fake, &mut forge, 1, &[2]);
    fake.pr(3).labels.push("queue".to_owned());
    let observation = forge.observe().unwrap();
    assert_eq!(observation.trunk_position, 1);
    assert_eq!(behind(&observation), [(1, 1), (3, 1)]);
}

#[test]
fn two_stacked_landings_between_reads_are_counted_by_members_and_each_tip_is_a_counted_trunk() {
    let fake = FakeGitHub::new();
    queued(&fake, 4);
    let (mut forge, _) = observed(&fake);
    forge.start_batch(&start(1, &fake.trunk(), &[1])).unwrap();
    let first = fake.get(&batch_ref(1)).expect("published");
    forge.observe().unwrap();
    forge.start_batch(&start(2, &first, &[2, 3])).unwrap();
    let second = fake.get(&batch_ref(2)).expect("published");
    passed(&fake, &first);
    passed(&fake, &second);
    forge.observe().unwrap();
    forge.land(BatchId(1)).unwrap();
    forge.land(BatchId(2)).unwrap();
    assert_eq!(fake.trunk(), second);
    // 4 is re-pushed onto the first landing's tip, one pull request behind
    // the second tip; trunk has four commits its head does not.
    let pushed = fake.commit(std::slice::from_ref(&first));
    fake.push(4, &pushed);
    passed(&fake, &pushed);

    let before = compares(&fake);
    let observation = forge.observe().unwrap();
    assert_eq!(observation.trunk_position, 3);
    assert_eq!(behind(&observation), [(4, 2)]);
    assert_eq!(
        compares_since(&fake, before),
        [format!("compare {pushed} {second}")]
    );
}

#[test]
fn a_push_to_trunk_by_another_hand_raises_the_position_and_every_behind_with_no_departure() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _) = forge(&fake);
    let observation = forge.observe().unwrap();
    assert_eq!(
        (observation.trunk_position, behind(&observation)),
        (0, vec![(1, 0)])
    );
    let from = fake.trunk();
    fake.advance_trunk();
    let to = fake.advance_trunk();
    let before = compares(&fake);
    let observation = forge.observe().unwrap();
    assert_eq!(observation.trunk_position, 2);
    assert_eq!(behind(&observation), [(1, 2)]);
    assert_eq!(observation.departed, []);
    assert_eq!(
        compares_since(&fake, before),
        [format!("compare {from} {to}")]
    );
    let before = compares(&fake);
    assert_eq!(forge.observe().unwrap().trunk_position, 2);
    assert_eq!(compares_since(&fake, before), Vec::<String>::new());
}

#[test]
fn a_landing_followed_by_a_push_is_counted_by_members_then_by_compare() {
    let fake = FakeGitHub::new();
    queued(&fake, 2);
    let (mut forge, _) = observed(&fake);
    let tip = land_batch(&fake, &mut forge, 1, &[1, 2]);
    let to = fake.advance_trunk();
    let before = compares(&fake);
    let observation = forge.observe().unwrap();
    assert_eq!(observation.trunk_position, 3);
    assert_eq!(
        compares_since(&fake, before),
        [format!("compare {tip} {to}")]
    );
}

#[test]
fn a_landing_record_recovered_as_landed_is_counted_by_members() {
    // A first read finds the record already landed: position 0.
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    let tip = tip_over(&fake, &[1]);
    put_record(&fake, 6, &[1], RecordState::Landing { tip: tip.clone() });
    fake.set_ref(&batch_ref(6), &tip);
    fake.set_ref(&branch("main"), &tip);
    let (mut forge, _) = forge(&fake);
    assert_eq!(forge.observe().unwrap().trunk_position, 0);

    // A forge that counted the trunk before the record and the fast-forward
    // were put in place, as a crashed process leaves them, counts the
    // members; with trunk pushed on past the tip, the push by `compare`.
    for trunk_moved_on in [false, true] {
        let fake = FakeGitHub::new();
        fake.open_pr(1);
        fake.open_pr(2);
        let (mut forge, _) = observed(&fake);
        let tip = tip_over(&fake, &[1, 2]);
        put_record(&fake, 6, &[1, 2], RecordState::Landing { tip: tip.clone() });
        fake.set_ref(&batch_ref(6), &tip);
        fake.set_ref(&branch("main"), &tip);
        let to = if trunk_moved_on {
            fake.advance_trunk()
        } else {
            tip.clone()
        };
        let before = compares(&fake);
        let observation = forge.observe().unwrap();
        assert_eq!(
            observation.batches[0].state,
            BatchState::Landed { tip: tip.clone() }
        );
        if trunk_moved_on {
            assert_eq!(observation.trunk_position, 3);
            assert_eq!(
                compares_since(&fake, before),
                [format!("compare {tip} {to}")]
            );
        } else {
            assert_eq!(observation.trunk_position, 2);
            assert_eq!(compares_since(&fake, before), Vec::<String>::new());
        }
    }
}

#[test]
fn a_restarted_forge_starts_its_position_at_zero_and_reads_each_head_once() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    fake.advance_trunk();
    let head = fake.open_pr(2);
    passed(&fake, &head);
    fake.state().comments.insert(2, vec![marker("queued")]);
    let (mut first, _) = observed(&fake);
    fake.advance_trunk();
    assert_eq!(first.observe().unwrap().trunk_position, 1);

    let (mut forge, _) = forge(&fake);
    let before = compares(&fake);
    let observation = forge.observe().unwrap();
    assert_eq!(observation.trunk_position, 0);
    assert_eq!(behind(&observation), [(1, 2), (2, 1)]);
    assert_eq!(compares_since(&fake, before).len(), 2);
    let before = compares(&fake);
    assert_eq!(behind(&forge.observe().unwrap()), [(1, 2), (2, 1)]);
    assert_eq!(compares_since(&fake, before), Vec::<String>::new());
}

#[test]
fn a_failed_first_read_commits_no_count() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, _) = forge(&fake);
    let before = compares(&fake);
    fake.fail("get_pr", Failure::Transient);
    assert!(matches!(forge.observe(), Err(ForgeError::Transient(_))));
    let to = fake.advance_trunk();
    let observation = forge.observe().unwrap();
    // A first read again: the walk the failed tick made counted nothing.
    assert_eq!(observation.trunk_position, 0);
    assert_eq!(behind(&observation), [(1, 1)]);
    let head = fake.pr(1).head.clone();
    assert_eq!(
        compares_since(&fake, before),
        [format!("compare {head} {to}")]
    );
}

#[test]
fn a_failed_read_commits_no_covered_cache() {
    let fake = FakeGitHub::new();
    queued(&fake, 2);
    let (mut forge, _) = forge(&fake);
    fake.fail_on("get_pr", "2", Failure::Transient);
    assert!(matches!(forge.observe(), Err(ForgeError::Transient(_))));
    fake.advance_trunk();
    let before = compares(&fake);
    let observation = forge.observe().unwrap();
    assert_eq!(observation.trunk_position, 0);
    // Both heads read again against the new first trunk, 1's included.
    assert_eq!(behind(&observation), [(1, 1), (2, 1)]);
    assert_eq!(compares_since(&fake, before).len(), 2);
}

#[test]
fn a_failed_tick_s_walk_is_discarded_and_the_retry_walks_from_the_committed_trunk() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let first = fake.trunk();
    let (mut forge, _) = forge(&fake);
    let before = compares(&fake);
    assert_eq!(forge.observe().unwrap().trunk_position, 0);
    assert_eq!(compares_since(&fake, before).len(), 1);
    let second = fake.advance_trunk();
    fake.fail("get_pr", Failure::Transient);
    let before = compares(&fake);
    assert!(matches!(forge.observe(), Err(ForgeError::Transient(_))));
    let third = fake.advance_trunk();
    let observation = forge.observe().unwrap();
    assert_eq!(observation.trunk_position, 2);
    assert_eq!(behind(&observation), [(1, 2)]);
    assert_eq!(
        compares_since(&fake, before),
        [
            format!("compare {first} {second}"),
            format!("compare {first} {third}")
        ]
    );
}
