//! The format-1 fixture: written once by [`write_the_format_1_fixture`],
//! checked in, and never regenerated. Every build reads it, so a contract
//! change that would make a format-1 journal unreadable fails here.

use std::collections::BTreeSet;

use dorfq_core::{
    BatchMember, BatchState, BatchStatus, CheckName, CiStatus, ConflictWith, Failure, Hold,
    LandRefusal, Notice, NoticeReason, StackPolicy, Withdrawal,
};
use serde_json::Value;

use super::super::events::{Fold, derive};
use super::*;

const FIXTURE: &str = include_str!("../../../testdata/journal-1.jsonl");

/// An observation holding every variant of every enum it can carry, at
/// `now`.
fn every_variant_observation(now: u64) -> Observation {
    let every_hold = BTreeSet::from([
        Hold::OwnCiPending,
        Hold::OwnCiFailed,
        Hold::MergeabilityUnknown,
        Hold::Protection,
        Hold::Draft,
        Hold::AncestorMoved { pr: PrNumber(1) },
    ]);
    let prs = vec![
        PrSnapshot::new(PrNumber(1), sha(1), PrBase::Trunk, PrStatus::Ready, 0),
        PrSnapshot::new(
            PrNumber(2),
            sha(2),
            PrBase::Pr(PrNumber(1)),
            PrStatus::Waiting(every_hold),
            3,
        )
        .with_stack(StackPolicy::Atomic),
        PrSnapshot::new(
            PrNumber(3),
            sha(3),
            PrBase::Trunk,
            PrStatus::Conflicting(ConflictWith::Trunk),
            1,
        ),
        PrSnapshot::new(
            PrNumber(4),
            sha(4),
            PrBase::Pr(PrNumber(1)),
            PrStatus::Conflicting(ConflictWith::Pr(PrNumber(1))),
            2,
        ),
    ];
    let member = |pr: u64| BatchMember::new(PrNumber(pr), sha(pr));
    let testing = |ci: CiStatus| BatchState::Testing { tip: sha(99), ci };
    let refused = |id: u64, reason: LandRefusal| {
        BatchStatus::new(
            BatchId(id),
            sha(id + 10),
            vec![member(1)],
            BatchState::LandRefused {
                tip: sha(id + 30),
                reason,
            },
        )
    };
    let batches = vec![
        BatchStatus::new(
            BatchId(1),
            sha(10),
            vec![member(1), member(2)],
            BatchState::Conflict {
                pr: PrNumber(2),
                with: ConflictWith::Pr(PrNumber(1)),
            },
        ),
        BatchStatus::new(
            BatchId(2),
            sha(10),
            vec![member(1)],
            BatchState::Conflict {
                pr: PrNumber(1),
                with: ConflictWith::Trunk,
            },
        ),
        BatchStatus::new(BatchId(3), sha(10), vec![], testing(CiStatus::Pending)),
        BatchStatus::new(
            BatchId(4),
            sha(11),
            vec![member(1)],
            testing(CiStatus::Passed),
        ),
        BatchStatus::new(
            BatchId(5),
            sha(12),
            vec![member(1)],
            testing(CiStatus::Failed(Failure::new(BTreeSet::new()))),
        ),
        BatchStatus::new(
            BatchId(6),
            sha(13),
            vec![member(1)],
            testing(CiStatus::Failed(Failure::new(BTreeSet::from([
                CheckName("lint".into()),
                CheckName("build".into()),
            ])))),
        ),
        BatchStatus::new(
            BatchId(7),
            sha(14),
            vec![member(1)],
            BatchState::Landed { tip: sha(15) },
        ),
        refused(8, LandRefusal::TrunkMoved { trunk: sha(18) }),
        refused(9, LandRefusal::MemberGone { pr: PrNumber(1) }),
        refused(10, LandRefusal::MemberHeld { pr: PrNumber(1) }),
    ];
    let departed = [
        Departed::Landed { batch: BatchId(7) },
        Departed::Merged,
        Departed::Ejected,
        Departed::Withdrawn(Withdrawal::IntentCleared),
        Departed::Withdrawn(Withdrawal::Closed),
        Departed::Withdrawn(Withdrawal::Blocked { on: PrNumber(5) }),
        Departed::Withdrawn(Withdrawal::Retargeted),
    ]
    .into_iter()
    .zip(20u64..)
    .map(|(how, pr)| Departure::new(PrNumber(pr), sha(pr), how))
    .collect();
    Observation::new(Timestamp(now), sha(10), 42, prs, batches, departed)
}

/// A plan holding every effect and every reason.
fn every_variant_plan() -> Plan {
    Plan {
        start: vec![
            BatchStart::new(BatchId(11), sha(10), vec![PrNumber(1), PrNumber(2)]),
            BatchStart::new(BatchId(12), sha(99), vec![]),
        ],
        land: vec![BatchId(4)],
        retire: vec![BatchId(5), BatchId(7)],
        eject: vec![
            Ejection::new(
                PrNumber(3),
                EjectReason::Conflicts {
                    with: ConflictWith::Trunk,
                },
            ),
            Ejection::new(
                PrNumber(4),
                EjectReason::Conflicts {
                    with: ConflictWith::Pr(PrNumber(1)),
                },
            ),
            Ejection::new(PrNumber(2), EjectReason::BatchFailed { batch: BatchId(6) }),
        ],
        notify: vec![
            Notice::new(PrNumber(1), NoticeReason::Broken { batch: BatchId(6) }),
            Notice::new(
                PrNumber(2),
                NoticeReason::InteractsWith {
                    pr: PrNumber(1),
                    batch: BatchId(6),
                },
            ),
        ],
    }
}

/// Writes the fixture to the path `DORFQ_WRITE_FIXTURE` names: a `start`
/// header, a `report` of every variant, a `report` with `error`, a `same`, a
/// fatal `error` line, then a second process's `start` header, its first
/// `report` and an idle one, which closes the segment; the fresh segment's
/// `rotate` header and a `report` after it. The closed segment and the
/// fresh one are written to the path one after the other, as the readers
/// take them in order. Run once, by hand, outside the sandbox; the file it
/// wrote is the checked-in fixture.
#[test]
#[ignore = "writes the format-1 fixture, which is never regenerated"]
fn write_the_format_1_fixture() {
    let fixture = PathBuf::from(std::env::var("DORFQ_WRITE_FIXTURE").unwrap());
    let path = fixture.with_extension("writing");
    let scheduler = every_scheduler().remove(2);
    let start = 1_761_520_000;
    let (mut writer, _) = Writer::open(
        &path,
        Header::start("mock", &scheduler, start),
        None,
        None,
        &mut |_, _| {},
    )
    .unwrap();
    writer
        .record(
            0,
            start + 1,
            &Ok(report(
                0,
                every_variant_observation(start),
                every_variant_plan(),
            )),
        )
        .unwrap();
    let effect = DriverError::Effect {
        report: Box::new(report(1, observation(start + 60, &[1, 2]), start_plan())),
        error: ForgeError::Transient("rate limited".into()),
    };
    writer.record(1, start + 61, &Err(effect)).unwrap();
    writer
        .record(
            2,
            start + 121,
            &Ok(report(
                2,
                observation(start + 120, &[1, 2]),
                Plan::default(),
            )),
        )
        .unwrap();
    let observe = DriverError::Observe(ForgeError::Permanent("bad credentials".into()));
    writer.record(3, start + 181, &Err(observe)).unwrap();
    drop(writer);
    let restart = start + 600;
    let (mut writer, _) = Writer::open(
        &path,
        Header::start("mock", &scheduler, restart),
        None,
        None,
        &mut |_, _| {},
    )
    .unwrap();
    writer
        .record(
            0,
            restart + 1,
            &Ok(report(0, observation(restart, &[2]), Plan::default())),
        )
        .unwrap();
    writer
        .record(
            1,
            restart + 61,
            &Ok(report(1, observation(restart + 60, &[]), Plan::default())),
        )
        .unwrap();
    writer
        .record(
            2,
            restart + 121,
            &Ok(report(2, observation(restart + 120, &[3]), Plan::default())),
        )
        .unwrap();
    drop(writer);
    let closed = PathBuf::from(format!("{}.{start}", path.display()));
    let mut bytes = fs::read(&closed).unwrap();
    bytes.extend(fs::read(&path).unwrap());
    fs::write(&fixture, bytes).unwrap();
    fs::remove_file(closed).unwrap();
    fs::remove_file(path).unwrap();
}

/// The fixture's entries, read by this build's reader from a copy in the
/// directory `name`.
fn fixture(name: &str) -> Vec<Entry> {
    let path = dir(name).join("journal-1.jsonl");
    fs::write(&path, FIXTURE).unwrap();
    let read = read(&[path]).unwrap();
    assert!(read.notes.is_empty(), "{:?}", read.notes);
    read.entries
}

#[test]
fn every_line_of_the_format_1_fixture_reads() {
    let entries = fixture("fixture-reads");
    assert_eq!(entries.len(), FIXTURE.lines().count());
    let shape: Vec<&str> = entries
        .iter()
        .map(|entry| match entry {
            Entry::Header(header) if header.cause == Cause::Start => "start",
            Entry::Header(_) => "rotate",
            Entry::Tick(line) => match &line.kind {
                LineKind::Report { failure: None, .. } => "report",
                LineKind::Report {
                    failure: Some(_), ..
                } => "report+error",
                LineKind::Same { .. } => "same",
                LineKind::Error(_) => "error",
            },
        })
        .collect();
    assert_eq!(
        shape,
        [
            "start",
            "report",
            "report+error",
            "same",
            "error",
            "start",
            "report",
            "report",
            "rotate",
            "report"
        ]
    );
    let [Entry::Header(second), Entry::Header(rotate)] = [&entries[5], &entries[8]] else {
        unreachable!("checked above")
    };
    assert_eq!(rotate.started, second.started);
    assert!(rotate.opened > second.opened);
    let Entry::Tick(Line {
        kind: LineKind::Report { report, .. },
        ..
    }) = &entries[1]
    else {
        unreachable!("checked above")
    };
    let start = 1_761_520_000;
    assert_eq!(report.observation, every_variant_observation(start));
    assert_eq!(report.plan, every_variant_plan());
}

#[test]
fn the_fixtures_observations_step_through_every_scheduler() {
    let entries = fixture("fixture-steps");
    for config in every_scheduler() {
        let mut scheduler = config.build();
        for entry in &entries {
            match entry {
                // A `start` header is a new process, with a fresh scheduler.
                Entry::Header(header) if header.cause == Cause::Start => {
                    scheduler = config.build();
                }
                Entry::Tick(Line {
                    kind: LineKind::Report { report, .. },
                    ..
                }) => {
                    if let Err(error) = scheduler.step(&report.observation) {
                        panic!("{config:?} at tick {}: {error}", report.tick);
                    }
                }
                _ => {}
            }
        }
    }
}

#[test]
fn the_fixture_derives_its_events() {
    let events = derive(&fixture("fixture-derives"));
    let kinds: Vec<String> = events
        .iter()
        .map(|event| {
            let value = serde_json::to_value(event).unwrap();
            match &value["kind"] {
                Value::String(kind) => kind.clone(),
                other => panic!("kind {other}"),
            }
        })
        .collect();
    let expected = [
        // The first observation, as the within-tick order puts it:
        // departures, arrivals, each batch's start then its endings, the
        // notices, the step.
        "merged eject withdraw withdraw blocked withdraw \
         arrive admit arrive admit held pr_ci arrive admit conflicting \
         arrive admit conflicting \
         start conflict start conflict start start run_end start run_end \
         start run_end start run_end land start run_end refused \
         start run_end refused start run_end refused \
         notice notice step",
        // The `report` with `error`: 3 and 4 gone unexplained, every batch
        // retired, trunk renumbered, 2's holds cleared; then its error.
        "lost lost retire retire retire retire retire retire retire retire \
         retire retire trunk held pr_ci_end step error",
        // The `same`, the fatal `error`, the second process; its idle tick
        // ends the closed segment, and the `rotate` header after it is no
        // restart.
        "step error restart lost step lost step arrive admit step",
    ];
    let expected: Vec<&str> = expected.iter().flat_map(|s| s.split_whitespace()).collect();
    assert_eq!(kinds, expected);
}

#[test]
fn the_fixture_folded_entry_by_entry_and_taken_each_time_is_derive() {
    let entries = fixture("fixture-fold");
    let mut fold = Fold::default();
    let mut events = Vec::new();
    for entry in &entries {
        fold.entry(entry);
        events.extend(fold.take());
    }
    events.extend(fold.finish());
    assert_eq!(events, derive(&entries));
}
