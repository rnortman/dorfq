use std::collections::{BTreeSet, VecDeque};

use dorfq_core::{
    BatchId, BatchMember, BatchStart, BatchStatus, CheckName, ConflictWith, Departed, Departure,
    EjectReason, Ejection, Failure, Hold, PrNumber, PrSnapshot, Sha, Timestamp, Withdrawal,
};

use super::*;

fn sha(n: u64) -> Sha {
    Sha::parse(&format!("{n:040x}")).unwrap()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Transient,
    Permanent,
}

/// Records every call and fails the first call named in `fail`, once.
struct FakeForge {
    observation: Observation,
    calls: Vec<String>,
    fail: Option<(&'static str, Kind)>,
}

impl FakeForge {
    fn new(observation: Observation) -> FakeForge {
        FakeForge {
            observation,
            calls: Vec::new(),
            fail: None,
        }
    }

    fn call(&mut self, name: &'static str, detail: String) -> Result<(), ForgeError> {
        self.calls.push(format!("{name} {detail}"));
        match self.fail {
            Some((failing, kind)) if failing == name => {
                self.fail = None;
                Err(match kind {
                    Kind::Transient => ForgeError::Transient(format!("{name} flaked").into()),
                    Kind::Permanent => ForgeError::Permanent(format!("{name} refused").into()),
                })
            }
            _ => Ok(()),
        }
    }
}

impl Forge for FakeForge {
    fn observe(&mut self) -> Result<Observation, ForgeError> {
        self.call("observe", String::new())?;
        Ok(self.observation.clone())
    }

    fn start_batch(&mut self, batch: &BatchStart) -> Result<(), ForgeError> {
        self.call("start", batch.id.0.to_string())
    }

    fn land(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        self.call("land", batch.0.to_string())
    }

    fn retire(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        self.call("retire", batch.0.to_string())
    }

    fn eject(&mut self, pr: PrNumber, _reason: &EjectReason) -> Result<(), ForgeError> {
        self.call("eject", pr.0.to_string())
    }
}

/// Returns its scripted plans in order, then empty plans.
struct ScriptedScheduler {
    plans: VecDeque<Plan>,
    seen: usize,
}

impl ScriptedScheduler {
    fn new(plans: Vec<Plan>) -> ScriptedScheduler {
        ScriptedScheduler {
            plans: plans.into(),
            seen: 0,
        }
    }
}

impl Scheduler for ScriptedScheduler {
    fn step(&mut self, _observation: &Observation) -> Result<Plan, SchedulerError> {
        self.seen += 1;
        Ok(self.plans.pop_front().unwrap_or_default())
    }
}

/// Never decides.
struct Stumped;

impl Scheduler for Stumped {
    fn step(&mut self, _observation: &Observation) -> Result<Plan, SchedulerError> {
        Err(SchedulerError::new("posterior underflow"))
    }
}

fn busy_observation() -> Observation {
    let pr = PrSnapshot::new(PrNumber(1), sha(2), PrBase::Trunk, PrStatus::Ready);
    let holds = BTreeSet::from([Hold::OwnCiPending, Hold::MergeabilityUnknown]);
    let held = PrSnapshot::new(
        PrNumber(2),
        sha(4),
        PrBase::Pr(PrNumber(1)),
        PrStatus::Waiting(holds),
    );
    let conflicting = PrSnapshot::new(
        PrNumber(3),
        sha(5),
        PrBase::Trunk,
        PrStatus::Conflicting(ConflictWith::Trunk),
    );
    let batch = BatchStatus::new(
        BatchId(1),
        sha(1),
        vec![BatchMember::new(PrNumber(1), sha(2))],
        BatchState::Testing {
            tip: sha(3),
            ci: CiStatus::Passed,
        },
    );
    let checks = BTreeSet::from([CheckName("lint".into()), CheckName("build".into())]);
    let failed = BatchStatus::new(
        BatchId(2),
        sha(3),
        vec![BatchMember::new(PrNumber(9), sha(9))],
        BatchState::Testing {
            tip: sha(6),
            ci: CiStatus::Failed(Failure::new(checks)),
        },
    );
    let silent = BatchStatus::new(
        BatchId(3),
        sha(6),
        vec![BatchMember::new(PrNumber(10), sha(10))],
        BatchState::Testing {
            tip: sha(7),
            ci: CiStatus::Failed(Failure::new(BTreeSet::new())),
        },
    );
    Observation::new(
        Timestamp(42),
        sha(1),
        vec![pr, held, conflicting],
        vec![batch, failed, silent],
        vec![
            Departure::new(PrNumber(4), sha(8), Departed::Merged),
            Departure::new(PrNumber(5), sha(9), Departed::Withdrawn(Withdrawal::Closed)),
        ],
    )
}

fn empty_observation() -> Observation {
    Observation::new(Timestamp(7), sha(1), Vec::new(), Vec::new(), Vec::new())
}

/// A plan with two items of every kind.
fn full_plan() -> Plan {
    let mut plan = Plan {
        start: vec![
            BatchStart::new(BatchId(5), sha(1), vec![PrNumber(1)]),
            BatchStart::new(BatchId(6), sha(1), vec![PrNumber(2)]),
        ],
        land: vec![BatchId(1), BatchId(2)],
        retire: vec![BatchId(3), BatchId(4)],
        ..Plan::default()
    };
    let trunk = EjectReason::Conflicts {
        with: ConflictWith::Trunk,
    };
    plan.eject.push(Ejection::new(PrNumber(7), trunk));
    let failed = EjectReason::BatchFailed { batch: BatchId(3) };
    plan.eject.push(Ejection::new(PrNumber(8), failed));
    plan
}

#[test]
fn plan_executes_land_start_eject_retire() {
    let mut forge = FakeForge::new(busy_observation());
    let mut scheduler = ScriptedScheduler::new(vec![full_plan()]);
    let report = Driver::new(&mut forge, &mut scheduler).tick().unwrap();
    assert_eq!(
        forge.calls,
        [
            "observe ", "land 1", "land 2", "start 5", "start 6", "eject 7", "eject 8", "retire 3",
            "retire 4",
        ]
    );
    assert_eq!(report.tick, 0);
    assert_eq!(report.observation, busy_observation());
    assert_eq!(report.plan, full_plan());
    assert!(!report.idle);
}

#[test]
fn transient_on_start_abandons_the_plan_and_the_next_tick_proceeds() {
    let mut forge = FakeForge::new(busy_observation());
    forge.fail = Some(("start", Kind::Transient));
    let mut scheduler = ScriptedScheduler::new(vec![full_plan(), full_plan()]);
    {
        let mut driver = Driver::new(&mut forge, &mut scheduler);
        let error = driver.tick().unwrap_err();
        assert!(matches!(
            error,
            DriverError::Forge(ForgeError::Transient(_))
        ));
        let report = driver.tick().unwrap();
        assert_eq!(report.tick, 1);
    }
    assert_eq!(
        forge.calls,
        [
            "observe ", "land 1", "land 2", "start 5", // abandoned here
            "observe ", "land 1", "land 2", "start 5", "start 6", "eject 7", "eject 8", "retire 3",
            "retire 4",
        ]
    );
}

#[test]
fn transient_on_eject_leaves_retire_uncalled() {
    let mut forge = FakeForge::new(busy_observation());
    forge.fail = Some(("eject", Kind::Transient));
    let mut scheduler = ScriptedScheduler::new(vec![full_plan()]);
    let error = Driver::new(&mut forge, &mut scheduler).tick().unwrap_err();
    assert!(matches!(
        error,
        DriverError::Forge(ForgeError::Transient(_))
    ));
    assert_eq!(
        forge.calls,
        [
            "observe ", "land 1", "land 2", "start 5", "start 6", "eject 7"
        ]
    );
}

#[test]
fn permanent_is_returned() {
    let mut forge = FakeForge::new(busy_observation());
    forge.fail = Some(("land", Kind::Permanent));
    let mut scheduler = ScriptedScheduler::new(vec![full_plan()]);
    let error = Driver::new(&mut forge, &mut scheduler).tick().unwrap_err();
    assert!(matches!(
        error,
        DriverError::Forge(ForgeError::Permanent(_))
    ));
    assert_eq!(error.to_string(), "permanent forge error: land refused");
    assert_eq!(forge.calls, ["observe ", "land 1"]);
}

#[test]
fn failed_observation_skips_the_scheduler() {
    let mut forge = FakeForge::new(busy_observation());
    forge.fail = Some(("observe", Kind::Transient));
    let mut scheduler = ScriptedScheduler::new(vec![full_plan()]);
    let error = Driver::new(&mut forge, &mut scheduler).tick().unwrap_err();
    assert!(matches!(
        error,
        DriverError::Forge(ForgeError::Transient(_))
    ));
    assert_eq!(scheduler.seen, 0);
    assert_eq!(forge.calls, ["observe "]);
}

#[test]
fn a_scheduler_error_is_returned_and_nothing_is_executed() {
    let mut forge = FakeForge::new(busy_observation());
    let error = Driver::new(&mut forge, &mut Stumped).tick().unwrap_err();
    assert!(matches!(error, DriverError::Scheduler(_)));
    assert_eq!(error.to_string(), "scheduler error: posterior underflow");
    assert_eq!(forge.calls, ["observe "]);
}

#[test]
fn idle_needs_no_prs_no_batches_and_an_empty_plan() {
    let mut forge = FakeForge::new(empty_observation());
    let mut scheduler = ScriptedScheduler::new(vec![]);
    assert!(Driver::new(&mut forge, &mut scheduler).tick().unwrap().idle);

    let mut forge = FakeForge::new(busy_observation());
    let mut scheduler = ScriptedScheduler::new(vec![]);
    assert!(!Driver::new(&mut forge, &mut scheduler).tick().unwrap().idle);

    let mut forge = FakeForge::new(empty_observation());
    let mut scheduler = ScriptedScheduler::new(vec![full_plan()]);
    assert!(!Driver::new(&mut forge, &mut scheduler).tick().unwrap().idle);
}

#[test]
fn report_renders_one_line_per_tick_and_per_effect() {
    let report = TickReport {
        tick: 3,
        observation: busy_observation(),
        plan: full_plan(),
        idle: false,
    };
    let text = report.to_string();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 9, "{text}");
    assert_eq!(
        lines[0],
        format!(
            "tick 3 now 42 trunk {} | pr 1 {} ready \
             | pr 2 {} on #1 waiting:own CI pending,mergeability unknown \
             | pr 3 {} conflicting:trunk | batch 1 [1] on {} testing {} ci passed \
             | batch 2 [9] on {} testing {} ci failed:build,lint \
             | batch 3 [10] on {} testing {} ci failed \
             | departed pr 4 {} merged outside the queue \
             | departed pr 5 {} withdrawn: closed",
            sha(1),
            sha(2),
            sha(4),
            sha(5),
            sha(1),
            sha(3),
            sha(3),
            sha(6),
            sha(6),
            sha(7),
            sha(8),
            sha(9)
        )
    );
    assert_eq!(lines[1], "  land batch 1");
    assert_eq!(lines[3], format!("  start batch 5 [1] on {}", sha(1)));
    assert_eq!(lines[5], "  eject pr 7: conflicts with trunk");
    assert_eq!(lines[6], "  eject pr 8: batch 3 failed");
    assert_eq!(lines[8], "  retire batch 4");

    let idle = TickReport {
        tick: 0,
        observation: empty_observation(),
        plan: Plan::default(),
        idle: true,
    };
    assert_eq!(
        idle.to_string(),
        format!("tick 0 now 7 trunk {} | idle", sha(1))
    );
}
