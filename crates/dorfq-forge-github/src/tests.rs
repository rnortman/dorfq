use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

use dorfq_core::{
    BatchId, BatchMember, BatchStart, BatchState, CiStatus, ConflictWith, EjectReason, Forge,
    ForgeError, LandRefusal, Mergeable, PrNumber, Sha,
};

use crate::api::{ApiError, CheckOutcome, CheckResult};
use crate::fake::{Failure, FakeGitHub};
use crate::record::{Record, RecordState};
use crate::{GitHubForge, Settings, branch};

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

fn forge(fake: &FakeGitHub) -> (GitHubForge, Buffer) {
    let sink = Buffer::default();
    let forge = GitHubForge::new(Box::new(fake.clone()), settings(), Box::new(sink.clone()))
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
            .map(|&n| BatchMember {
                pr: PrNumber(n),
                head: fake.pr(n).head.clone(),
            })
            .collect(),
        state,
    };
    let sha = fake.commit(std::slice::from_ref(&base));
    fake.state().commits.get_mut(&sha).unwrap().message = record.render();
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
    let error = GitHubForge::new(Box::new(fake.clone()), settings(), Box::new(io::sink()))
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
    let error = GitHubForge::new(Box::new(fake.clone()), settings(), Box::new(io::sink()))
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
        let error = GitHubForge::new(Box::new(fake.clone()), bad, Box::new(io::sink()))
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

#[test]
fn observe_maps_mergeable_including_unknown() {
    let fake = FakeGitHub::new();
    for n in 1..=3 {
        fake.open_pr(n);
    }
    fake.pr(2).mergeable = Some(false);
    fake.pr(3).mergeable = None;
    let (mut forge, _) = forge(&fake);

    let mergeable: Vec<Mergeable> = forge
        .observe()
        .unwrap()
        .prs
        .iter()
        .map(|p| p.mergeable)
        .collect();
    assert_eq!(
        mergeable,
        [Mergeable::Clean, Mergeable::Conflicting, Mergeable::Unknown]
    );
}

#[test]
fn observe_reads_head_ci_from_the_required_names() {
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

    let ci: Vec<CiStatus> = forge
        .observe()
        .unwrap()
        .prs
        .iter()
        .map(|p| p.head_ci)
        .collect();
    assert_eq!(
        ci,
        [
            CiStatus::Passed,
            CiStatus::Pending,
            CiStatus::Failed,
            CiStatus::Passed
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
            tip,
            ci: CiStatus::Passed
        }
    );
}

#[test]
fn a_testing_batch_whose_tip_is_trunk_is_still_testing() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    let tip = fake.trunk();
    put_record(&fake, 1, &[1], RecordState::Testing { tip: tip.clone() });
    fake.set_ref(&batch_ref(1), &tip);
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
fn a_conflict_record_is_a_conflict_and_leftover_branches_are_deleted() {
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
    assert_eq!(fake.get(&branch("dorfq/build/5")), None);
    assert_eq!(fake.get(&branch("dorfq/probe/5")), None);
    assert!(fake.get("refs/dorfq/batches/5").is_some());

    // With nothing left over, a later observation deletes nothing.
    let deletes = fake.calls_to(&["delete_ref"]).len();
    forge.observe().unwrap();
    assert_eq!(fake.calls_to(&["delete_ref"]).len(), deletes);
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
    let tip = tip_over(&fake, &[1]);
    let moved = fake.commit(&[fake.trunk()]);
    put_record(&fake, 1, &[1], RecordState::Landed { tip: tip.clone() });
    put_record(
        &fake,
        2,
        &[1],
        RecordState::Refused {
            tip: tip.clone(),
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
            tip: tip.clone(),
            reason: LandRefusal::MemberGone { pr: PrNumber(1) },
        },
    );
    fake.set_ref(&batch_ref(2), &tip);
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
            BatchState::Landed { tip: tip.clone() },
            BatchState::LandRefused {
                tip: tip.clone(),
                reason: LandRefusal::TrunkMoved { trunk: moved }
            },
            BatchState::LandRefused {
                tip,
                reason: LandRefusal::MemberGone { pr: PrNumber(1) }
            },
        ]
    );
}

#[test]
fn batches_are_sorted_by_id() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
    let tip = tip_over(&fake, &[1]);
    for id in [10, 2, 33] {
        put_record(&fake, id, &[1], RecordState::Landed { tip: tip.clone() });
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
    fake.open_pr(1);
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
        fake.open_pr(1);
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
fn a_last_marker_of_landed_ejected_or_withdrawn_is_greeted() {
    for last in ["landed", "ejected", "withdrawn"] {
        let fake = FakeGitHub::new();
        fake.open_pr(1);
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
    for how in ["unlabelled", "closed", "retargeted"] {
        let fake = FakeGitHub::new();
        fake.open_pr(1);
        let (mut forge, sink) = forge(&fake);
        forge.observe().unwrap();

        match how {
            "unlabelled" => fake.pr(1).labels.clear(),
            "closed" => fake.pr(1).open = false,
            _ => fake.pr(1).base = "release".to_owned(),
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
fn a_withdrawn_pull_request_relabelled_is_greeted_again() {
    let fake = FakeGitHub::new();
    fake.open_pr(1);
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
    fake.open_pr(1);
    fake.open_pr(2);
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
    fake.open_pr(1);
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
    fake.open_pr(1);
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
    fake.open_pr(1);
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
    fake.open_pr(1);
    fake.open_pr(2);
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
    fake.open_pr(1);
    fake.open_pr(2);
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

fn start(id: u64, base: &Sha, prs: &[u64]) -> BatchStart {
    BatchStart {
        id: BatchId(id),
        base: base.clone(),
        prs: prs.iter().map(|&n| PrNumber(n)).collect(),
    }
}

fn member(pr: u64, head: &Sha) -> BatchMember {
    BatchMember {
        pr: PrNumber(pr),
        head: head.clone(),
    }
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
fn a_batch_with_nothing_to_merge_is_published_at_its_base() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let base = fake.trunk();
    fake.pr(1).head = base.clone();
    let (mut forge, _) = observed(&fake);

    forge.start_batch(&start(1, &base, &[1])).unwrap();

    assert_eq!(fake.get(&batch_ref(1)), Some(base.clone()));
    assert_eq!(
        record_of(&fake, 1).state,
        RecordState::Testing { tip: base }
    );
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
fn a_later_member_observed_conflicting_is_with_trunk_without_a_probe() {
    let fake = FakeGitHub::new();
    let heads = queued(&fake, 2);
    fake.pr(2).mergeable = Some(false);
    fake.state().conflicts_with_trunk.insert(heads[1].clone());
    let (mut forge, sink) = observed(&fake);

    let state = conflict_of(&fake, &mut forge, &sink, &[1, 2]);
    assert_eq!(state, conflict(2, ConflictWith::Trunk));
    assert_eq!(probes(&fake), 0);
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
        (start(1, &trunk, &[]), "no members"),
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
fn a_batch_whose_tip_is_its_base_lands_as_a_no_op_update() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let base = fake.trunk();
    fake.pr(1).head = base.clone();
    passed(&fake, &base);
    let (mut forge, sink) = observed(&fake);
    forge.start_batch(&start(1, &base, &[1])).unwrap();
    forge.observe().unwrap();
    sink.0.borrow_mut().clear();

    forge.land(BatchId(1)).unwrap();

    assert_eq!(
        fake.calls_to(&["update_ref"])
            .iter()
            .filter(|c| c.starts_with("update_ref refs/heads/main "))
            .collect::<Vec<_>>(),
        [&format!("update_ref refs/heads/main {base} force=false")]
    );
    assert_eq!(fake.trunk(), base);
    assert!(fake.pr(1).labels.is_empty());
    assert_eq!(sink.lines(), landed_lines(&base, &[1]));
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
            EjectReason::ConflictsWithTrunk,
            "conflicts with `main`",
            "conflicts with trunk",
        ),
        (
            EjectReason::ConflictsWith { pr: PrNumber(2) },
            "conflicts with #2",
            "conflicts with pr 2",
        ),
        (
            EjectReason::BatchFailed { batch: BatchId(5) },
            "batch 5 failed its checks",
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

    forge
        .eject(PrNumber(1), &EjectReason::ConflictsWithTrunk)
        .unwrap();
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
    let error = forge
        .eject(PrNumber(3), &EjectReason::ConflictsWithTrunk)
        .unwrap_err();
    assert!(is_permanent(&error, "pr 3"), "{error}");
    assert!(fake.writes().is_empty());
}

#[test]
fn an_ejected_pull_request_relabelled_is_greeted_again() {
    let fake = FakeGitHub::new();
    queued(&fake, 1);
    let (mut forge, sink) = observed(&fake);
    forge
        .eject(PrNumber(1), &EjectReason::ConflictsWithTrunk)
        .unwrap();
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
