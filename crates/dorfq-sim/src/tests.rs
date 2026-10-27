use std::collections::{BTreeMap, BTreeSet};

use dorfq_core::{
    BatchId, BatchStart, ConflictWith, EjectReason, Ejection, PrNumber, PrStatus, SchedulerError,
    Sha, Timestamp,
};

use super::*;
use dorfq_scheduler::{BayesConfig, Speculation};

/// The shipped example configuration.
const EXAMPLE: &str = include_str!("../examples/sim.toml");

/// A world of a few hours and a handful of pull requests.
fn small(p_interact: f64, p_flake: f64, p_conflict: f64) -> WorldConfig {
    WorldConfig {
        hours: 6,
        drain_hours: 48,
        stall_hours: 48,
        arrivals_per_hour: 3.0,
        ci_minutes: Minutes { min: 20, max: 40 },
        pr_ci_minutes: Minutes { min: 5, max: 15 },
        rebase_minutes: Minutes { min: 15, max: 60 },
        pr_ci_on_base_landed: true,
        p_interact,
        p_flake,
        p_miss: 0.0,
        trunk_fix_minutes: Minutes { min: 0, max: 0 },
        outages: vec![],
        p_conflict,
        repush_per_hour: 0.0,
        withdraw_per_hour: 0.0,
        stack_sizes: vec![1.0],
        p_stack_grows: 0.0,
        stack_growth_sizes: vec![1.0],
        stack_growth_minutes: Minutes { min: 0, max: 0 },
        p_atomic: 0.0,
    }
}

/// [`small`] with every queued pull request re-pushed and withdrawn on
/// clocks.
fn churning(p_interact: f64, p_flake: f64) -> WorldConfig {
    WorldConfig {
        repush_per_hour: 0.3,
        withdraw_per_hour: 0.1,
        ..small(p_interact, p_flake, 0.05)
    }
}

fn bisect() -> SchedulerConfig {
    SchedulerConfig::Bisect {
        lanes: 2,
        max_batch: 8,
    }
}

/// `Bayes` at the example's parameters.
fn bayes() -> SchedulerConfig {
    SchedulerConfig::Bayes(BayesConfig {
        lanes: 2,
        max_batch: 24,
        p_interact: 0.002,
        p_flake: 0.02,
        p_miss: 0.02,
        p_trunk: 0.001,
        run_cost: 0.5,
        w_reject: 1.0,
        w_age: 0.0,
        eject_above: 0.9,
        others_below: 0.1,
        speculate_above: 0.0,
        speculation: Speculation::Chain,
    })
}

/// [`bayes`] under `speculation = "tree"`.
fn bayes_tree() -> SchedulerConfig {
    let SchedulerConfig::Bayes(config) = bayes() else {
        unreachable!()
    };
    SchedulerConfig::Bayes(BayesConfig {
        speculation: Speculation::Tree,
        ..config
    })
}

/// Runs to the end, asserting no failure, the world's invariants and the
/// count identity; returns the world and its metrics.
fn replicate_cleanly(
    config: &WorldConfig,
    admission: Admission,
    scheduler: &SchedulerConfig,
    seed: u64,
) -> (World, Metrics) {
    let (world, outcome) = replicate(config, admission, scheduler, seed, 0, None);
    let end = outcome.unwrap_or_else(|e| panic!("{scheduler:?} under {admission:?}: {e}"));
    assert_eq!(
        world.check_invariants(),
        Ok(()),
        "{scheduler:?} under {admission:?}"
    );
    let m = world.metrics(end);
    let ejected = m.ejected_bad + m.ejected_good;
    let left = m.landed + ejected + m.withdrawn;
    assert_eq!(m.arrived, left + m.unfinished);
    assert!(m.left_with_base <= left, "{m:?}");
    assert!(m.ejected_bad <= m.ever_bad, "{m:?}");
    assert!(m.arrived > 5, "too few arrivals to say anything: {m:?}");
    (world, m)
}

/// A forge that records the effects it is asked for, failing on one.
#[derive(Default)]
struct Recorder {
    calls: Vec<&'static str>,
    fail_on: Option<&'static str>,
}

impl Recorder {
    fn call(&mut self, name: &'static str) -> Result<(), ForgeError> {
        self.calls.push(name);
        if self.fail_on == Some(name) {
            return Err(ForgeError::Transient("refused".into()));
        }
        Ok(())
    }
}

impl Forge for Recorder {
    fn observe(&mut self) -> Result<Observation, ForgeError> {
        unreachable!("execute never observes")
    }

    fn start_batch(&mut self, _: &BatchStart) -> Result<(), ForgeError> {
        self.call("start")
    }

    fn land(&mut self, _: BatchId) -> Result<(), ForgeError> {
        self.call("land")
    }

    fn retire(&mut self, _: BatchId) -> Result<(), ForgeError> {
        self.call("retire")
    }

    fn eject(&mut self, _: PrNumber, _: &EjectReason) -> Result<(), ForgeError> {
        self.call("eject")
    }
}

fn full_plan(trunk: Sha) -> Plan {
    let reason = EjectReason::Conflicts {
        with: ConflictWith::Trunk,
    };
    let mut plan = Plan {
        start: vec![BatchStart::new(BatchId(3), trunk, vec![PrNumber(1)])],
        land: vec![BatchId(1)],
        retire: vec![BatchId(2)],
        ..Plan::default()
    };
    plan.eject.push(Ejection::new(PrNumber(2), reason));
    plan
}

fn trunk() -> Sha {
    Sha::parse(&format!("{:040x}", 1)).expect("hex")
}

#[test]
fn a_plan_executes_in_rule_seven_order_and_stops_at_the_first_error() {
    let mut forge = Recorder::default();
    execute(&mut forge, &full_plan(trunk())).unwrap();
    assert_eq!(forge.calls, ["land", "start", "eject", "retire"]);
    let mut forge = Recorder {
        fail_on: Some("start"),
        ..Recorder::default()
    };
    assert!(execute(&mut forge, &full_plan(trunk())).is_err());
    assert_eq!(forge.calls, ["land", "start"]);
}

/// Retires whatever it sees and starts the first queued pull request again.
struct Restless {
    next: u64,
}

impl Scheduler for Restless {
    fn step(&mut self, o: &Observation) -> Result<Plan, SchedulerError> {
        let mut plan = Plan {
            retire: o.batches.iter().map(|b| b.id).collect(),
            ..Plan::default()
        };
        if let Some(pr) = o.prs.iter().find(|p| p.status == PrStatus::Ready) {
            self.next += 1;
            plan.start.push(BatchStart::new(
                BatchId(self.next),
                o.trunk.clone(),
                vec![pr.number],
            ));
        }
        Ok(plan)
    }
}

/// Never plans anything.
struct Idle;

impl Scheduler for Idle {
    fn step(&mut self, _: &Observation) -> Result<Plan, SchedulerError> {
        Ok(Plan::default())
    }
}

#[test]
fn a_scheduler_that_never_settles_is_livelocked() {
    let config = small(0.0, 0.0, 0.0);
    let mut world = World::new(config, Admission::Label, Streams::for_replication(1, 0));
    let outcome = drive(
        &mut world,
        &mut Restless { next: 0 },
        u64::MAX,
        u64::MAX,
        None,
    );
    let Err(SimError::Livelock { at }) = outcome else {
        panic!("expected a livelock, got {outcome:?}");
    };
    assert!(at > 0, "the first pull request arrives after t=0");
}

#[test]
fn a_scheduler_that_never_acts_on_a_conflicting_pull_request_is_stuck() {
    let config = small(0.0, 0.0, 1.0);
    let mut world = World::new(config, Admission::Label, Streams::for_replication(1, 0));
    let outcome = drive(&mut world, &mut Idle, u64::MAX, u64::MAX, None);
    let Err(SimError::Stuck {
        queued, tracked, ..
    }) = outcome
    else {
        panic!("expected stuck, got {outcome:?}");
    };
    assert!(queued > 0);
    assert_eq!(tracked, 0);
}

/// Starts a batch naming a pull request that never arrived.
struct Rogue;

impl Scheduler for Rogue {
    fn step(&mut self, o: &Observation) -> Result<Plan, SchedulerError> {
        Ok(Plan {
            start: vec![BatchStart::new(
                BatchId(1),
                o.trunk.clone(),
                vec![PrNumber(999)],
            )],
            ..Plan::default()
        })
    }
}

/// Cannot decide once anything is queued.
struct Stumped;

impl Scheduler for Stumped {
    fn step(&mut self, o: &Observation) -> Result<Plan, SchedulerError> {
        if o.prs.is_empty() {
            Ok(Plan::default())
        } else {
            Err(SchedulerError::new("no verdict"))
        }
    }
}

#[test]
fn a_scheduler_error_ends_the_replication_as_its_failure() {
    let config = small(0.0, 0.0, 0.0);
    let mut world = World::new(config, Admission::Label, Streams::for_replication(1, 0));
    let outcome = drive(&mut world, &mut Stumped, u64::MAX, u64::MAX, None);
    let error = outcome.unwrap_err();
    let SimError::Scheduler { at, message } = &error else {
        panic!("expected a scheduler failure, got {error:?}");
    };
    assert!(*at > 0, "the first pull request arrives after t=0");
    assert_eq!(message, "scheduler error: no verdict");
    assert_eq!(
        error.to_string(),
        format!("scheduler failed at t={at}: scheduler error: no verdict")
    );
}

#[test]
fn observations_carry_the_world_clock() {
    let config = small(0.0, 0.0, 0.0);
    let mut world = World::new(config, Admission::Label, Streams::for_replication(1, 0));
    assert_eq!(world.observe().unwrap().now, Timestamp(0));
    let next = world.next_event().expect("an arrival is scheduled");
    world.advance_to(next);
    assert_eq!(world.observe().unwrap().now, Timestamp(next));
}

#[test]
fn a_violating_plan_is_the_last_step_of_the_trace() {
    let config = small(0.0, 0.0, 0.0);
    let mut world = World::new(config, Admission::Label, Streams::for_replication(1, 0)).traced();
    let mut events = Vec::new();
    let outcome = drive(
        &mut world,
        &mut Rogue,
        u64::MAX,
        u64::MAX,
        Some(&mut events),
    );
    let Err(SimError::Violation { at: 0, message }) = outcome else {
        panic!("expected a violation at t=0, got {outcome:?}");
    };
    assert!(message.contains("pr 999"), "{message}");
    let last = events.last().expect("a step").to_string();
    assert!(
        last.starts_with("t=0 ") && last.contains("start 1[999] "),
        "{last}"
    );
}

#[test]
fn a_replication_cut_off_at_the_deadline_counts_its_unfinished_work() {
    // Long runs leave work queued when the drain is 0.
    let config = WorldConfig {
        drain_hours: 0,
        ci_minutes: Minutes { min: 90, max: 120 },
        ..small(0.0, 0.0, 0.0)
    };
    let deadline = config.hours * HOUR;
    let (world, m) = replicate_cleanly(&config, Admission::Label, &bisect(), 1);
    assert!(m.unfinished > 0, "{m:?}");
    assert!(m.cut_off && !m.stalled, "{m:?}");
    assert_eq!(m.ended_hours, config.hours as f64, "{m:?}");
    assert_eq!(world.now(), deadline);
    assert!(world.runs().iter().all(|r| r.started_at <= deadline));
    // Seed-pinned: a run is still running at the deadline, and its elapsed
    // time is charged.
    let finished: u64 = world.runs().iter().map(|r| r.consumed).sum();
    assert!(m.runner_hours * HOUR as f64 > finished as f64, "{m:?}");
}

#[test]
fn a_replication_ends_when_its_world_empties() {
    let config = small(0.0, 0.0, 0.0);
    let (_, m) = replicate_cleanly(&config, Admission::Label, &bisect(), 1);
    assert_eq!(
        (m.cut_off, m.stalled, m.unfinished),
        (false, false, 0),
        "{m:?}"
    );
    let deadline = (config.hours + config.drain_hours) as f64;
    assert!(m.ended_hours > 0.0 && m.ended_hours < deadline, "{m:?}");
}

#[test]
fn a_drained_replication_ends_at_its_last_departure_not_its_last_pr_ci_run() {
    let config = small(0.0, 0.0, 0.0);
    let (world, outcome) = replicate(&config, Admission::Label, &bisect(), 1, 0, None);
    let end = outcome.unwrap();
    let last_departure = world
        .pr_records()
        .iter()
        .filter_map(|p| p.left.as_ref().map(|&(at, _)| at))
        .max()
        .expect("a pull request left");
    assert_eq!(end, End::drained(last_departure));
    // Every pull request ejected at once leaves its own CI run running.
    let mut world = World::new(config, Admission::Label, Streams::for_replication(1, 0));
    let end = drive(&mut world, &mut Ejector, u64::MAX, HOUR, None).unwrap();
    let last_departure = world
        .pr_records()
        .iter()
        .filter_map(|p| p.left.as_ref().map(|&(at, _)| at))
        .max()
        .expect("a pull request left");
    assert_eq!(end, End::drained(last_departure));
    assert!(world.now() > last_departure, "no run outlived the world");
    assert_eq!(
        world.metrics(end).ended_hours,
        last_departure as f64 / HOUR as f64
    );
    // The runs that outlived it ran to completion.
    assert!(world.pr_ci_runs().iter().all(|r| r.consumed > 0));
}

/// Ejects every pull request it sees.
struct Ejector;

impl Scheduler for Ejector {
    fn step(&mut self, o: &Observation) -> Result<Plan, SchedulerError> {
        let mut plan = Plan::default();
        for pr in &o.prs {
            let reason = EjectReason::Conflicts {
                with: ConflictWith::Trunk,
            };
            plan.eject.push(Ejection::new(pr.number, reason));
        }
        Ok(plan)
    }
}

/// Runs one batch at a time on the first ready pull request, and retires it
/// once its run completes: it never lands or ejects.
struct Rerunner {
    next: u64,
}

impl Scheduler for Rerunner {
    fn step(&mut self, o: &Observation) -> Result<Plan, SchedulerError> {
        let mut plan = Plan::default();
        let pending = |b: &&dorfq_core::BatchStatus| {
            matches!(
                b.state,
                BatchState::Testing {
                    ci: CiStatus::Pending,
                    ..
                }
            )
        };
        if o.batches.iter().any(|b| pending(&b)) {
            return Ok(plan);
        }
        plan.retire = o.batches.iter().map(|b| b.id).collect();
        if !plan.retire.is_empty() {
            return Ok(plan);
        }
        if let Some(pr) = o.prs.iter().find(|p| p.status == PrStatus::Ready) {
            self.next += 1;
            plan.start.push(BatchStart::new(
                BatchId(self.next),
                o.trunk.clone(),
                vec![pr.number],
            ));
        }
        Ok(plan)
    }
}

#[test]
fn a_scheduler_that_never_lands_or_ejects_is_cut_off_as_stalled_with_its_metrics() {
    let config = small(0.0, 0.0, 0.0);
    for idle in [true, false] {
        let mut world = World::new(
            config.clone(),
            Admission::Label,
            Streams::for_replication(1, 0),
        );
        let first = world.next_event().expect("an arrival is scheduled");
        let end = if idle {
            drive(&mut world, &mut Idle, u64::MAX, HOUR, None)
        } else {
            drive(&mut world, &mut Rerunner { next: 0 }, u64::MAX, HOUR, None)
        }
        .unwrap();
        assert_eq!(
            end,
            End {
                at: first + HOUR,
                cut_off: true,
                stalled: true
            },
            "idle: {idle}"
        );
        // The clock starts at the first arrival, the end of the empty world.
        assert_eq!(world.now(), first + HOUR, "idle: {idle}");
        let m = world.metrics(end);
        assert!(m.unfinished > 0 && m.cut_off && m.stalled, "{m:?}");
        assert_eq!(m.ended_hours, (first + HOUR) as f64 / HOUR as f64);
        assert_eq!(m.landed, 0, "{m:?}");
        if !idle {
            assert!(m.runs > 1, "{m:?}");
        }
    }
}

#[test]
fn a_scheduler_that_keeps_ejecting_does_not_stall() {
    let config = small(0.0, 0.0, 0.0);
    let mut world = World::new(config, Admission::Label, Streams::for_replication(1, 0));
    let end = drive(&mut world, &mut Ejector, u64::MAX, HOUR, None).unwrap();
    assert_eq!((end.cut_off, end.stalled), (false, false));
    let m = world.metrics(end);
    assert_eq!((m.unfinished, m.ejected_good), (0, m.arrived), "{m:?}");
}

#[test]
fn a_stall_after_the_deadline_is_a_deadline_cut_off() {
    let config = small(0.0, 0.0, 0.0);
    let mut world = World::new(config, Admission::Label, Streams::for_replication(1, 0));
    let first = world.next_event().expect("an arrival is scheduled");
    let deadline = first + HOUR / 2;
    let end = drive(&mut world, &mut Idle, deadline, HOUR, None).unwrap();
    assert_eq!(
        end,
        End {
            at: deadline,
            cut_off: true,
            stalled: false
        }
    );
    assert_eq!(world.now(), deadline);
}

#[test]
fn an_empty_world_drains_at_once() {
    let mut config = small(0.0, 0.0, 0.0);
    config.arrivals_per_hour = 0.0;
    let metrics = run(&config, Admission::Label, &bisect(), 1, 0, None).unwrap();
    assert_eq!(metrics.arrived, 0);
    assert_eq!(metrics.runner_hours_per_landed, None);
}

#[test]
fn one_seed_and_one_policy_give_identical_metrics_records_and_traces() {
    let config = small(0.01, 0.05, 0.05);
    let traced = || {
        let mut events = Vec::new();
        let (world, outcome) = replicate(
            &config,
            Admission::Label,
            &bisect(),
            7,
            0,
            Some(&mut events),
        );
        let end = outcome.unwrap();
        let records = (
            world.metrics(end),
            world.pr_records(),
            world.runs().to_vec(),
        );
        (records, events)
    };
    let (first, first_trace) = traced();
    let (second, second_trace) = traced();
    assert_eq!(first, second);
    assert_eq!(first_trace, second_trace);
    assert!(matches!(first_trace[0], TraceEvent::Step { t: 0, .. }));
    let arrive = |e: &TraceEvent| matches!(e, TraceEvent::Arrive { .. });
    assert!(first_trace.iter().any(arrive));
    let run_end = |e: &TraceEvent| matches!(e, TraceEvent::RunEnd { .. });
    assert!(first_trace.iter().any(run_end));
}

#[test]
fn two_policies_at_one_seed_see_identical_arrivals_truths_and_stacks() {
    let config = stacking();
    let floor = SchedulerConfig::AllAtOnce {};
    let mut truths = Vec::new();
    let mut stacked = false;
    for admission in [Admission::Label, Admission::Github] {
        for scheduler in [floor.clone(), bisect(), bayes()] {
            let (world, _) = replicate(&config, admission, &scheduler, 5, 0, None);
            let all = world.truths();
            stacked |= all.iter().any(|t| t.3.is_some());
            // Where a growth lands is the policy's doing; nothing else is.
            truths.push(
                all.into_iter()
                    .map(|(at, conflicts, key, _)| (at, conflicts, key))
                    .collect::<Vec<_>>(),
            );
        }
    }
    assert!(truths[0].len() > 5);
    assert!(stacked);
    assert!(truths.iter().all(|t| *t == truths[0]));
}

/// [`churning`] with stacks of one to four pull requests, half of them
/// growing later and half `Atomic`.
fn stacking() -> WorldConfig {
    WorldConfig {
        stack_sizes: vec![3.0, 2.0, 1.0, 1.0],
        p_stack_grows: 0.5,
        stack_growth_sizes: vec![1.0, 1.0],
        stack_growth_minutes: Minutes { min: 15, max: 90 },
        p_atomic: 0.5,
        ..churning(0.02, 0.05)
    }
}

/// Asserts that every billed PR CI run follows a landing of its pull
/// request's direct base — at that landing's instant under `label`, at the
/// push of the rebase it forced under `github` — and that no pull request
/// has more than one. Returns how many billed runs it checked.
fn assert_billed_runs_follow_their_direct_base_landing(events: &[TraceEvent]) -> usize {
    let mut base = BTreeMap::new();
    let mut landed_at = BTreeMap::new();
    let mut rebased_for_landing = BTreeSet::new();
    let mut billed = BTreeSet::new();
    for event in events {
        match event {
            TraceEvent::Arrive { pr, base: b, .. } => {
                base.insert(*pr, *b);
            }
            TraceEvent::Land { t, members, .. } => {
                landed_at.extend(members.iter().map(|&m| (m, *t)));
            }
            TraceEvent::Rebase { pr, why, .. } => {
                if *why == Rebase::BaseLanded {
                    rebased_for_landing.insert(*pr);
                } else {
                    rebased_for_landing.remove(pr);
                }
            }
            TraceEvent::PrCi {
                t,
                pr,
                billed: true,
            } => {
                let b = base[pr].unwrap_or_else(|| panic!("{event}: billed on trunk"));
                let at = landed_at.get(&b);
                let forced = at == Some(t) || (at.is_some() && rebased_for_landing.remove(pr));
                assert!(forced, "{event}: its base {b} landed at {at:?}");
                assert!(billed.insert(*pr), "{event}: billed twice");
            }
            _ => {}
        }
    }
    billed.len()
}

#[test]
fn every_policy_drains_a_stacking_world_under_churn_under_either_admission() {
    let mut left_with_base = 0;
    for (admission, scheduler) in [Admission::Label, Admission::Github]
        .into_iter()
        .flat_map(|a| {
            [
                (a, SchedulerConfig::AllAtOnce {}),
                (a, bisect()),
                (a, bayes()),
                (a, bayes_tree()),
            ]
        })
    {
        // Seeds at which a pull request that arrived stacked landed.
        let mut landed_stacked = 0;
        let mut billed = 0;
        let mut rebases = 0;
        // A batch started after a member's own CI passed, at a later instant.
        let mut gated = false;
        for seed in 1..=4 {
            let mut events = Vec::new();
            let (world, outcome) = replicate(
                &stacking(),
                admission,
                &scheduler,
                seed,
                0,
                Some(&mut events),
            );
            let what = format!("{scheduler:?} under {admission:?} at seed {seed}");
            let end = outcome.unwrap_or_else(|e| panic!("{what}: {e}"));
            assert!(!end.cut_off, "{what}: {end:?}");
            assert_eq!(world.check_invariants(), Ok(()), "{what}");
            assert_eq!(crate::trace::check_lives(&events), Ok(()), "{what}");
            billed += assert_billed_runs_follow_their_direct_base_landing(&events);
            let m = world.metrics(end);
            assert_eq!(
                m.billed_pr_ci_runs,
                world.pr_ci_runs().iter().filter(|r| r.billed).count(),
                "{what}"
            );
            let left = m.landed + m.ejected_bad + m.ejected_good + m.withdrawn;
            assert_eq!(m.arrived, left + m.unfinished, "{m:?}");
            assert_eq!(m.unfinished, 0, "{what}: {m:?}");
            left_with_base += m.left_with_base;
            rebases += m.rebases;
            if admission == Admission::Github {
                let stacked_admit = events
                    .iter()
                    .find(|e| matches!(e, TraceEvent::Admit { base: Some(_), .. }));
                assert_eq!(stacked_admit, None, "{what}");
            }
            let mut passed_at = BTreeMap::new();
            for event in &events {
                match event {
                    TraceEvent::PrCiEnd {
                        t,
                        pr,
                        cancelled: false,
                        failed: false,
                    } => {
                        passed_at.insert(*pr, *t);
                    }
                    TraceEvent::Start { t, members, .. } => {
                        gated |= members.iter().any(|m| passed_at.get(m) < Some(t));
                    }
                    _ => {}
                }
            }
            let stacked: Vec<u64> = events
                .iter()
                .filter_map(|e| match e {
                    TraceEvent::Arrive {
                        pr, base: Some(_), ..
                    } => Some(*pr),
                    _ => None,
                })
                .collect();
            let landed = world.pr_records().into_iter().any(|r| {
                stacked.contains(&r.number.0) && matches!(r.left, Some((_, Exit::Landed { .. })))
            });
            landed_stacked += usize::from(landed);
        }
        assert!(
            landed_stacked > 0,
            "{scheduler:?} under {admission:?} never landed a stacked pull request"
        );
        assert!(gated, "{scheduler:?} under {admission:?}");
        assert!(
            billed > 0,
            "{scheduler:?} under {admission:?} billed nothing"
        );
        if admission == Admission::Github {
            assert!(rebases > 0, "{scheduler:?} under {admission:?}");
        }
    }
    assert!(
        left_with_base > 0,
        "no pull request ever left with its base"
    );
}

#[test]
fn re_runs_keep_every_life_checked() {
    // A one-hour window inside six hours of arrivals, and misses that break
    // trunk: own CI fails for both reasons, and its authors wait to re-run.
    let world = WorldConfig {
        outages: vec![crate::config::Outage {
            start_hours: 2,
            hours: 1,
        }],
        p_miss: 0.5,
        trunk_fix_minutes: Minutes { min: 30, max: 90 },
        ..small(0.02, 0.05, 0.02)
    };
    for admission in [Admission::Label, Admission::Github] {
        let mut reruns = 0;
        for seed in 1..=2 {
            let mut events = Vec::new();
            let (w, outcome) = replicate(&world, admission, &bayes(), seed, 0, Some(&mut events));
            let what = format!("under {admission:?} at seed {seed}");
            let end = outcome.unwrap_or_else(|e| panic!("{what}: {e}"));
            assert!(!end.cut_off, "{what}: {end:?}");
            assert_eq!(w.check_invariants(), Ok(()), "{what}");
            assert_eq!(crate::trace::check_lives(&events), Ok(()), "{what}");
            reruns += events
                .iter()
                .filter(|e| matches!(e, TraceEvent::Rerun { .. }))
                .count();
        }
        assert!(reruns > 0, "no re-run under {admission:?}");
    }
}

#[test]
fn with_no_re_run_on_a_landed_base_nothing_is_billed_and_every_policy_drains() {
    let config = WorldConfig {
        pr_ci_on_base_landed: false,
        ..stacking()
    };
    for admission in [Admission::Label, Admission::Github] {
        for scheduler in [
            SchedulerConfig::AllAtOnce {},
            bisect(),
            bayes(),
            bayes_tree(),
        ] {
            for seed in 1..=2 {
                let mut events = Vec::new();
                let (world, outcome) =
                    replicate(&config, admission, &scheduler, seed, 0, Some(&mut events));
                let what = format!("{scheduler:?} under {admission:?} at seed {seed}");
                let end = outcome.unwrap_or_else(|e| panic!("{what}: {e}"));
                assert!(!end.cut_off, "{what}: {end:?}");
                assert_eq!(world.check_invariants(), Ok(()), "{what}");
                assert_eq!(crate::trace::check_lives(&events), Ok(()), "{what}");
                let billed = events
                    .iter()
                    .find(|e| matches!(e, TraceEvent::PrCi { billed: true, .. }));
                assert_eq!(billed, None, "{what}");
                let m = world.metrics(end);
                assert_eq!((m.billed_pr_ci_runs, m.unfinished), (0, 0), "{what}");
                if admission == Admission::Github {
                    let landed = |e: &TraceEvent| {
                        matches!(
                            e,
                            TraceEvent::Rebase {
                                why: Rebase::BaseLanded,
                                ..
                            }
                        )
                    };
                    assert!(!events.iter().any(landed), "{what}");
                }
            }
        }
    }
}

#[test]
fn simulate_reports_every_policy_at_every_replication() {
    let mut config = SimConfig::parse(EXAMPLE).unwrap();
    config.world = small(0.01, 0.02, 0.02);
    config.replications = 2;
    let report = simulate(&config, true);
    let names: Vec<&str> = report.policies.iter().map(|p| p.name.as_str()).collect();
    let expected: Vec<&str> = config.policy.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, expected);
    assert!(!report.failed(), "{report}");
    for policy in &report.policies {
        assert_eq!(policy.replications.len(), 2);
        assert!(matches!(policy.trace[0], TraceEvent::Step { t: 0, .. }));
    }
    let untraced = simulate(&config, false);
    assert!(untraced.policies.iter().all(|p| p.trace.is_empty()));
}

#[test]
fn all_at_once_ejects_every_member_of_a_failed_batch() {
    // Seed-pinned: at this seed some failing batch holds a pull request
    // that interacts with nothing, so the floor ejects a good one.
    let config = small(0.05, 0.0, 0.0);
    let (world, m) =
        replicate_cleanly(&config, Admission::Label, &SchedulerConfig::AllAtOnce {}, 3);
    let exits: BTreeMap<PrNumber, Option<(u64, Exit)>> = world
        .pr_records()
        .into_iter()
        .map(|r| (r.number, r.left))
        .collect();
    let failed: Vec<&RunRecord> = world
        .runs()
        .iter()
        .filter(|r| matches!(r.verdict, Some(CiStatus::Failed(_))))
        .collect();
    assert!(!failed.is_empty(), "no batch failed at this seed");
    for run in failed {
        // Every all-at-once batch is built on trunk, so its tree beyond
        // trunk is its own members.
        assert_eq!(run.tree.len(), run.members);
        for pr in &run.tree {
            assert!(
                matches!(exits[pr], Some((_, Exit::Ejected { .. }))),
                "pr {} of failed batch {} is {:?}",
                pr.0,
                run.batch.0,
                exits[pr]
            );
        }
    }
    assert!(m.ejected_good > 0, "{m:?}");
}

#[test]
fn bisect_ejects_only_bad_pull_requests_without_flakes() {
    // An invariant without flakes or churn: a singleton read as its own
    // failure, everything beneath it having passed, fails only when it is
    // bad against trunk. Draining is seed-specific: the 48-hour drain is
    // ample for six hours of arrivals.
    for seed in 1..=4 {
        let config = small(0.05, 0.0, 0.1);
        let (_, m) = replicate_cleanly(&config, Admission::Label, &bisect(), seed);
        assert_eq!(
            (m.ejected_good, m.unfinished, m.withdrawn),
            (0, 0, 0),
            "{m:?}"
        );
        assert!(m.landed > 0 && m.ejected_bad > 0, "{m:?}");
        assert!(m.ejected_bad <= m.ever_bad, "{m:?}");
    }
}

#[test]
fn every_policy_runs_cleanly_under_repushes_and_withdrawals() {
    for seed in 1..=4 {
        let config = churning(0.02, 0.05);
        for scheduler in [
            SchedulerConfig::AllAtOnce {},
            bisect(),
            bayes(),
            bayes_tree(),
        ] {
            for admission in [Admission::Label, Admission::Github] {
                let (_, m) = replicate_cleanly(&config, admission, &scheduler, seed);
                assert!(m.withdrawn > 0, "{m:?}");
            }
        }
    }
}

/// Asserts that every `BatchFailed` ejection names a run that had completed
/// `Failed` by the moment of the ejection and whose tree held the ejected
/// pull request. Returns how many ejections it checked.
fn assert_ejections_name_failed_runs_of_their_tree(world: &World) -> usize {
    let runs: BTreeMap<BatchId, &RunRecord> = world.runs().iter().map(|r| (r.batch, r)).collect();
    let mut checked = 0;
    for record in world.pr_records() {
        let Some((at, Exit::Ejected { reason, .. })) = &record.left else {
            continue;
        };
        let EjectReason::BatchFailed { batch } = reason else {
            continue;
        };
        let run = runs.get(batch).unwrap_or_else(|| {
            panic!(
                "pr {} names batch {}, which never ran",
                record.number.0, batch.0
            )
        });
        assert!(matches!(run.verdict, Some(CiStatus::Failed(_))), "{run:?}");
        assert!(
            run.started_at + run.consumed <= *at,
            "pr {} ejected at {at} naming {run:?}, which had not completed",
            record.number.0
        );
        assert!(
            run.tree.contains(&record.number),
            "pr {} is not in the tree of {run:?}",
            record.number.0
        );
        checked += 1;
    }
    checked
}

/// [`small`] with flakes and nothing else wrong.
fn flaky() -> WorldConfig {
    WorldConfig {
        hours: 24,
        ..small(0.0, 0.05, 0.0)
    }
}

#[test]
fn bayes_ejects_only_members_of_failed_runs_it_names_in_a_flake_free_world() {
    let config = small(0.05, 0.0, 0.1);
    let (world, m) = replicate_cleanly(&config, Admission::Label, &bayes(), 1);
    assert!(assert_ejections_name_failed_runs_of_their_tree(&world) > 0);
    assert!(m.landed > 0 && m.ejected_bad > 0, "{m:?}");
    // Seed-pinned, not an invariant of `Bayes`: a good pull request that
    // keeps failing beside distinct bad ones can be singled out.
    assert_eq!(m.ejected_good, 0, "{m:?}");
}

#[test]
fn bayes_ejects_no_good_pull_request_where_bisect_does_in_a_flaky_world() {
    let config = flaky();
    let (world, m) = replicate_cleanly(&config, Admission::Label, &bayes(), 1);
    assert_ejections_name_failed_runs_of_their_tree(&world);
    assert!(m.landed > 0, "{m:?}");
    // Seed-pinned comparison, not an invariant: `Bayes` ejects a good pull
    // request when two lone runs of it flake, `Bisect` when one does.
    assert_eq!(m.ejected_good, 0, "{m:?}");
    let (_, baseline) = replicate_cleanly(&config, Admission::Label, &bisect(), 1);
    assert!(baseline.ejected_good > 0, "{baseline:?}");
}

#[test]
fn every_policy_in_the_example_world_runs_one_replication_cleanly() {
    let config = SimConfig::parse(EXAMPLE).unwrap();
    for policy in &config.policy {
        let (world, outcome) = replicate(
            &config.world,
            policy.admission,
            &policy.scheduler,
            config.seed,
            0,
            None,
        );
        assert!(outcome.is_ok(), "policy {}: {outcome:?}", policy.name);
        assert_eq!(world.check_invariants(), Ok(()), "policy {}", policy.name);
    }
}

/// Every pull request an event names, by field.
fn named_prs(event: &serde_json::Value) -> Vec<u64> {
    let mut prs = Vec::new();
    for field in ["pr", "on"] {
        prs.extend(event[field].as_u64());
    }
    let members = event["members"].as_array().into_iter().flatten();
    prs.extend(members.filter_map(serde_json::Value::as_u64));
    for start in event["start"].as_array().into_iter().flatten() {
        let ids = start["members"].as_array().into_iter().flatten();
        prs.extend(ids.filter_map(serde_json::Value::as_u64));
    }
    for eject in event["eject"].as_array().into_iter().flatten() {
        prs.extend(eject["pr"].as_u64());
    }
    prs
}

#[test]
fn the_json_document_holds_every_policy_and_a_trace_from_time_zero() {
    let mut config = SimConfig::parse(EXAMPLE).unwrap();
    // Conflicts common enough that a traced replication breaks and fixes.
    config.world = WorldConfig {
        p_interact: 0.3,
        ..stacking()
    };
    config.replications = 2;
    let report = simulate(&config, true);
    assert!(!report.failed(), "{report}");
    let document: serde_json::Value =
        serde_json::from_str(&report.to_json("stacking", &config, EXAMPLE)).unwrap();
    assert_eq!(document["schema"], 4);
    assert_eq!(document["name"], "stacking");
    assert_eq!(document["config_text"], EXAMPLE);
    assert_eq!(document["config"]["world"]["p_stack_grows"], 0.5);
    assert_eq!(document["config"]["world"]["p_miss"], 0.0);
    assert!(document["config"]["world"].get("p_stacked").is_none());
    assert!(document["config"]["world"].get("runners").is_none());
    let policies = document["policies"].as_array().unwrap();
    assert_eq!(policies.len(), config.policy.len());
    let mut kinds = BTreeSet::new();
    for (policy, configured) in policies.iter().zip(&config.policy) {
        assert_eq!(policy["name"], configured.name.as_str());
        let replications = policy["replications"].as_array().unwrap();
        assert_eq!(replications.len(), 2);
        assert!(replications.iter().all(|r| r["metrics"].is_object()));
        assert!(
            replications
                .iter()
                .all(|r| r["metrics"]["landed_bad"].is_u64()
                    && r["metrics"]["trunk_broken_hours"].is_f64())
        );
        let trace = policy["trace"].as_array().unwrap();
        assert_eq!(
            trace[0],
            serde_json::json!({
                "kind": "step", "t": 0, "queued": 0, "in_flight": 0,
                "land": [], "start": [], "eject": [], "retire": []
            })
        );
        let mut arrived = BTreeSet::new();
        let mut started = BTreeSet::new();
        // The pull requests whose PR CI run is open.
        let mut pr_ci: BTreeSet<u64> = BTreeSet::new();
        for event in trace {
            let pr = event["pr"].as_u64();
            kinds.insert(event["kind"].as_str().unwrap());
            match event["kind"].as_str().unwrap() {
                "arrive" => {
                    assert!(event["conflicts"].is_boolean(), "{event}");
                    assert!(event.get("truth").is_none(), "{event}");
                }
                "blocked" => assert!(event["bad"].is_boolean(), "{event}"),
                "pr_ci" => {
                    assert!(event["billed"].is_boolean(), "{event}");
                    assert!(pr_ci.insert(pr.unwrap()), "{event}: a run already open");
                }
                "pr_ci_end" => {
                    assert!(event["failed"].is_boolean(), "{event}");
                    assert!(pr_ci.remove(&pr.unwrap()), "{event}: no run open");
                }
                "run_begin" | "pr_ci_begin" => panic!("{event}: a removed kind"),
                _ => {}
            }
            if event["kind"] == "arrive" {
                arrived.insert(event["pr"].as_u64().unwrap());
            }
            for pr in named_prs(event) {
                assert!(
                    arrived.contains(&pr),
                    "{event} names {pr} before it arrived"
                );
            }
            if event["kind"] == "start" {
                if let Some(base) = event["base"].as_u64() {
                    assert!(started.contains(&base), "{event}");
                }
                started.insert(event["batch"].as_u64().unwrap());
            }
        }
        assert!(started.len() > 1, "{}", configured.name);
        assert!(
            pr_ci.is_empty(),
            "{}: a PR CI run begun and never ended: {pr_ci:?}",
            configured.name
        );
    }
    for kind in ["broken", "fixed"] {
        assert!(kinds.contains(kind), "no {kind} event in {kinds:?}");
    }
}
