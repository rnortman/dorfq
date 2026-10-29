//! The simulator's world as the forge, driven by the application's driver
//! and journaled tick by tick: the summary checked against the world's own
//! metrics where the truth is known, and the journal replayed into the
//! plans the scheduler made.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use dorfq_core::{BatchId, Forge, NoticeReason, Scheduler};
use dorfq_sim::{Admission, End, Streams, World, WorldConfig};

use super::super::events::{Fold, derive};
use super::super::summary::summarise;
use super::*;
use crate::driver::Driver;

/// The host clock the harness journals: the world's clock past this.
const EPOCH: u64 = 1_000_000_000;

/// A busy world with arrivals for `hours` and `p_conflict` given. The tests'
/// non-vacuity assertions and the replay's failed-eject tick depend on its
/// fixed rates.
fn world(hours: u64, p_conflict: f64) -> WorldConfig {
    let text = format!(
        "hours = {hours}\ndrain_hours = 1000\nstall_hours = 168\narrivals_per_hour = 5.0\n\
         ci_minutes = {{ min = 45, max = 120 }}\npr_ci_minutes = {{ min = 45, max = 120 }}\n\
         rebase_minutes = {{ min = 15, max = 120 }}\np_heed = 0.75\n\
         pr_ci_on_base_landed = true\np_interact = 0.001\np_flake = 0.03\np_miss = 0.02\n\
         trunk_fix_minutes = {{ min = 30, max = 180 }}\noutages = []\n\
         p_conflict = {p_conflict:?}\nrepush_per_hour = 0.02\nwithdraw_per_hour = 0.005\n\
         stack_sizes = [16, 16, 15, 13, 11, 9, 7, 5, 3, 2, 2, 1]\np_stack_grows = 0.3\n\
         stack_growth_sizes = [3, 2, 1]\nstack_growth_minutes = {{ min = 30, max = 480 }}\n\
         p_atomic = 0.2\n"
    );
    let config: WorldConfig = toml::from_str(&text).unwrap();
    config.validate().unwrap();
    config
}

/// The world as a forge, failing with `Transient` the first `eject` at or
/// after tick `fail_from` — with `after_a_start`, the first in a tick whose
/// `start_batch` already ran.
struct Harnessed {
    world: Rc<RefCell<World>>,
    ticks: u64,
    started: bool,
    fail_from: Option<u64>,
    after_a_start: bool,
    failed: Rc<RefCell<Option<u64>>>,
}

impl Forge for Harnessed {
    fn observe(&mut self) -> Result<Observation, ForgeError> {
        self.ticks += 1;
        self.started = false;
        self.world.borrow_mut().observe()
    }

    fn start_batch(&mut self, batch: &BatchStart) -> Result<(), ForgeError> {
        self.world.borrow_mut().start_batch(batch)?;
        self.started = true;
        Ok(())
    }

    fn land(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        self.world.borrow_mut().land(batch)
    }

    fn retire(&mut self, batch: BatchId) -> Result<(), ForgeError> {
        self.world.borrow_mut().retire(batch)
    }

    fn eject(&mut self, pr: PrNumber, reason: &EjectReason) -> Result<(), ForgeError> {
        let tick = self.ticks - 1;
        let mut failed = self.failed.borrow_mut();
        if (self.started || !self.after_a_start)
            && failed.is_none()
            && self.fail_from.is_some_and(|from| tick >= from)
        {
            *failed = Some(tick);
            return Err(ForgeError::Transient(
                "the harness failed this eject".into(),
            ));
        }
        self.world.borrow_mut().eject(pr, reason)
    }

    fn notify(&mut self, pr: PrNumber, reason: &NoticeReason) -> Result<(), ForgeError> {
        self.world.borrow_mut().notify(pr, reason)
    }
}

/// A journaled run of a world to its draining.
struct Run {
    /// Every tick's report, the failed effect's included; index is tick.
    reports: Vec<TickReport>,
    world: World,
    /// The instant the world last emptied.
    end: End,
    /// The journal's segments, in order.
    segments: Vec<PathBuf>,
    /// The events file's segments, in order.
    events: Vec<PathBuf>,
    /// The tick whose eject the harness failed.
    failed: Option<u64>,
}

/// Runs `scheduler` against `world` under `label` admission as the
/// simulator's loop does — tick while the plan is non-empty, then once
/// more, then advance to the next event — until the world drains,
/// journaling every tick under `dir`: compacted as the writer does when
/// `compact`, else every tick a `report`.
fn run(
    dir: &Path,
    world: WorldConfig,
    scheduler: &SchedulerConfig,
    compact: bool,
    fail_from: Option<u64>,
) -> Run {
    let world = Rc::new(RefCell::new(World::new(
        world,
        Admission::Label,
        Streams::for_replication(1, 0),
    )));
    let failed = Rc::new(RefCell::new(None));
    let mut forge = Harnessed {
        world: Rc::clone(&world),
        ticks: 0,
        started: false,
        fail_from,
        // `all-at-once` ejects a failed batch's members a tick before it
        // starts the next batch, so no tick of its does both.
        after_a_start: !matches!(scheduler, SchedulerConfig::AllAtOnce {}),
        failed: Rc::clone(&failed),
    };
    let path = dir.join("q.journal");
    let (mut writer, _) = Writer::open(
        &path,
        Header::start("sim", scheduler, EPOCH),
        Some(&dir.join("q.events")),
        &mut |_, _| {},
    )
    .unwrap();
    let mut built = scheduler.build();
    let mut driver = Driver::new(&mut forge, built.as_mut());
    let mut reports = Vec::new();
    let mut emptied = None;
    let mut quiet = false;
    loop {
        let outcome = driver.tick();
        let now = world.borrow().now();
        if !compact {
            writer.last = None;
        }
        writer
            .record(driver.ticks() - 1, EPOCH + now, &outcome)
            .unwrap();
        let report = match outcome {
            Ok(report) => report,
            Err(DriverError::Effect { report, .. }) => *report,
            Err(error) => panic!("tick {}: {error}", driver.ticks() - 1),
        };
        let empty = report.observation.prs.is_empty()
            && report.observation.batches.is_empty()
            && world.borrow().out() == 0;
        emptied = if empty { emptied.or(Some(now)) } else { None };
        let planned = !report.plan.is_empty();
        reports.push(report);
        assert!(reports.len() < 50_000, "the world never drained");
        if planned {
            quiet = false;
            continue;
        }
        // One more tick at the same instant, as a real forge's refresh
        // with nothing due: the ticks the writer compacts to `same`.
        if !quiet {
            quiet = true;
            continue;
        }
        quiet = false;
        let next = world.borrow().next_event();
        match next {
            Some(t) => world.borrow_mut().advance_to(t),
            None => break,
        }
    }
    drop(forge);
    let segments_of = |stem: &str| {
        let mut segments: Vec<PathBuf> = names(dir)
            .into_iter()
            .filter(|name| name.starts_with(&format!("{stem}.")))
            .map(|name| dir.join(name))
            .collect();
        segments.push(dir.join(stem));
        segments
    };
    let segments = segments_of("q.journal");
    let events = segments_of("q.events");
    let failed = *failed.borrow();
    Run {
        reports,
        world: Rc::into_inner(world).unwrap().into_inner(),
        end: End::drained(emptied.expect("the world drained")),
        segments,
        events,
        failed,
    }
}

/// Steps `scheduler` through every `report` observation the journal holds
/// before tick `until`, in order — `same`, header and `error`-only lines
/// skipped — and compares each plan with the journaled one; the first
/// difference names its segment and line.
fn replay(segments: &[PathBuf], until: u64, scheduler: &mut dyn Scheduler) -> Result<(), String> {
    for path in segments {
        let read = read(std::slice::from_ref(path)).map_err(|e| e.to_string())?;
        for (index, entry) in read.entries.iter().enumerate() {
            let Entry::Tick(Line {
                tick,
                kind: LineKind::Report { report, .. },
                ..
            }) = entry
            else {
                continue;
            };
            if *tick >= until {
                return Ok(());
            }
            let at = || format!("{}:{}", path.display(), index + 1);
            let plan = scheduler
                .step(&report.observation)
                .map_err(|e| format!("{}: {e}", at()))?;
            if plan != report.plan {
                return Err(format!(
                    "{}: replayed {plan:?}, journaled {:?}",
                    at(),
                    report.plan
                ));
            }
        }
    }
    Ok(())
}

fn close(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() < 1e-9,
        (a, b) => a == b,
    }
}

#[test]
fn a_simulated_journal_sums_to_the_worlds_metrics() {
    // With no trunk conflict the world sends no number out and back, and
    // the summary's definitions and the world's coincide.
    let config = &every_scheduler()[2];
    let run = run(&dir("harness-metrics"), world(72, 0.0), config, true, None);
    let read = read(&run.segments).unwrap();
    let summary = summarise(&derive(&read.entries));
    let metrics = run.world.metrics(run.end);
    assert!(summary.ejected > 0, "{summary:?}");
    assert!(summary.withdrawn > 0, "{summary:?}");
    assert!(summary.left_with_base > 0, "{summary:?}");
    assert_eq!(summary.arrived, metrics.arrived);
    assert_eq!(summary.landed, metrics.landed);
    assert_eq!(summary.runs, metrics.runs);
    assert_eq!(summary.cancelled_runs, metrics.cancelled_runs);
    assert_eq!(summary.withdrawn, metrics.withdrawn);
    assert_eq!(summary.left_with_base, metrics.left_with_base);
    assert_eq!(summary.ejected, metrics.ejected_bad + metrics.ejected_good);
    for (name, ours, theirs) in [
        (
            "mean_batch_size",
            summary.mean_batch_size,
            metrics.mean_batch_size,
        ),
        (
            "wasted_fraction",
            summary.wasted_fraction,
            metrics.wasted_fraction,
        ),
        (
            "land_p50_hours",
            summary.land_p50_hours,
            metrics.land_p50_hours,
        ),
        (
            "land_p95_hours",
            summary.land_p95_hours,
            metrics.land_p95_hours,
        ),
    ] {
        assert!(close(ours, theirs), "{name}: {ours:?} != {theirs:?}");
    }
}

/// Under every scheduler kind: runs the busy world, replays a fresh
/// scheduler through the journal up to a tick in the middle, then steps it
/// on the observations the run's scheduler saw after; every plan is equal.
fn replays_into_the_same_plans(name: &str, compact: bool, fail_from: Option<u64>) {
    for (i, config) in every_scheduler().iter().enumerate() {
        let dir = dir(&format!("replay-{name}-{i}"));
        let run = run(&dir, world(24, 0.02), config, compact, fail_from);
        let mut k = run.reports.len() as u64 / 2;
        if fail_from.is_some() {
            let Some(failed) = run.failed else {
                panic!("{config:?}: the harness failed no eject");
            };
            k = k.max(failed + 1);
            let read = read(&run.segments).unwrap();
            assert!(
                read.entries.iter().any(|entry| matches!(
                    entry,
                    Entry::Tick(Line {
                        tick,
                        kind: LineKind::Report {
                            failure: Some(_),
                            ..
                        },
                        ..
                    }) if *tick == failed
                )),
                "{config:?}: tick {failed} is not a report with its error"
            );
        }
        if compact {
            let read = read(&run.segments).unwrap();
            assert!(
                read.entries.iter().any(|entry| matches!(
                    entry,
                    Entry::Tick(Line {
                        tick,
                        kind: LineKind::Same { .. },
                        ..
                    }) if *tick < k
                )),
                "{config:?}: the replayed ticks hold no `same` line"
            );
        }
        let mut fresh = config.build();
        if let Err(difference) = replay(&run.segments, k, fresh.as_mut()) {
            panic!("{config:?}: {difference}");
        }
        for report in &run.reports[k as usize..] {
            let plan = fresh.step(&report.observation).unwrap();
            assert_eq!(plan, report.plan, "{config:?} at tick {}", report.tick);
        }
    }
}

#[test]
fn a_compacted_journal_replays_into_the_same_plans() {
    replays_into_the_same_plans("compact", true, None);
}

#[test]
fn a_journal_of_every_tick_replays_into_the_same_plans() {
    replays_into_the_same_plans("every", false, None);
}

#[test]
fn a_journal_with_a_failed_effect_replays_into_the_same_plans() {
    replays_into_the_same_plans("failed", true, Some(10));
}

#[test]
fn the_events_file_is_derive_over_the_journal_and_the_fold_stays_bounded() {
    let dir = dir("harness-events");
    let run = run(&dir, world(24, 0.02), &every_scheduler()[2], true, None);
    assert!(run.segments.len() > 2, "{:?}", run.segments);
    // Each closed events segment carries its journal segment's name.
    let stems: Vec<String> = run
        .events
        .iter()
        .map(|p| p.display().to_string().replace("q.events", "q.journal"))
        .collect();
    let journal: Vec<String> = run
        .segments
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert_eq!(stems, journal);
    let entries = read(&run.segments).unwrap().entries;
    let derived = derive(&entries);
    let written: Vec<String> = run
        .events
        .iter()
        .flat_map(|path| {
            fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    let expected: Vec<String> = derived
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect();
    assert_eq!(written, expected);

    let mut fold = Fold::default();
    let mut events = Vec::new();
    let mut recent: [BTreeSet<u64>; 2] = Default::default();
    let mut ejected = false;
    for entry in &entries {
        fold.entry(entry);
        events.extend(fold.take());
        let Entry::Tick(Line {
            kind: LineKind::Report { report, .. },
            ..
        }) = entry
        else {
            continue;
        };
        let o = &report.observation;
        recent = [
            recent[1].clone(),
            o.batches.iter().map(|b| b.id.0).collect(),
        ];
        let kept = |id: &u64| recent.iter().any(|ids| ids.contains(id));
        assert!(fold.tips.values().all(kept), "tick {}", report.tick);
        assert!(fold.landed.iter().all(kept), "tick {}", report.tick);
        assert!(
            fold.ejections
                .keys()
                .all(|pr| o.prs.iter().any(|s| s.number.0 == *pr)),
            "tick {}",
            report.tick
        );
        ejected |= !fold.ejections.is_empty();
    }
    assert!(ejected, "no ejection reason was ever held");
    events.extend(fold.finish());
    assert_eq!(events, derived);
}
