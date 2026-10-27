use super::*;
use crate::config::{Admission, Minutes, Outage};

fn config(p_interact: f64, p_flake: f64) -> WorldConfig {
    WorldConfig {
        hours: 1,
        drain_hours: 1,
        stall_hours: 1,
        arrivals_per_hour: 0.0,
        ci_minutes: Minutes { min: 10, max: 10 },
        pr_ci_minutes: Minutes { min: 0, max: 0 },
        rebase_minutes: Minutes { min: 0, max: 0 },
        p_heed: 0.0,
        pr_ci_on_base_landed: true,
        p_interact,
        p_flake,
        p_miss: 0.0,
        trunk_fix_minutes: Minutes { min: 0, max: 0 },
        outages: vec![],
        p_conflict: 0.0,
        repush_per_hour: 0.0,
        withdraw_per_hour: 0.0,
        stack_sizes: vec![1.0],
        p_stack_grows: 0.0,
        stack_growth_sizes: vec![1.0],
        stack_growth_minutes: Minutes { min: 0, max: 0 },
        p_atomic: 0.0,
    }
}

/// `stack_sizes` that always draws `size`.
fn always(size: usize) -> Vec<f64> {
    let mut weights = vec![0.0; size];
    weights[size - 1] = 1.0;
    weights
}

#[test]
fn drawn_stack_sizes_average_the_configured_mean() {
    let mut cfg = config(0.0, 0.0);
    cfg.stack_sizes = vec![0.5, 0.0, 0.3, 0.2];
    let mut rng = SplitMix64::new(17);
    let n = 100_000;
    let total: usize = (0..n).map(|_| draw_size(&mut rng, &cfg.stack_sizes)).sum();
    let mean = total as f64 / n as f64;
    let expected = cfg.mean_pull_requests_per_stack();
    assert_eq!(expected, 2.2);
    assert!((mean / expected - 1.0).abs() < 0.01, "{mean}");
}

#[test]
fn arrivals_per_hour_counts_pull_requests_not_stacks() {
    // Stacks of 3, half of them growing by 2 at once: 4 pull requests a
    // stack, so 2.5 stacks an hour bring the configured 10.
    let mut cfg = config(0.0, 0.0);
    cfg.hours = 400;
    cfg.arrivals_per_hour = 10.0;
    cfg.stack_sizes = always(3);
    cfg.p_stack_grows = 0.5;
    cfg.stack_growth_sizes = always(2);
    assert_eq!(cfg.mean_pull_requests_per_stack(), 4.0);
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(1, 0));
    while let Some(t) = w.next_event() {
        w.advance_to(t);
    }
    // About 1000 stacks: a 15% band is several standard deviations wide,
    // and excludes both the stack rate (×4) and the rate ignoring growth
    // (×4/3).
    let per_hour = w.pr_records().len() as f64 / 400.0;
    assert!((per_hour / 10.0 - 1.0).abs() < 0.15, "{per_hour}");
}

fn world(p_interact: f64, p_flake: f64) -> World {
    World::new(
        config(p_interact, p_flake),
        Admission::Label,
        Streams::for_replication(1, 0),
    )
    .traced()
}

/// A pull request arriving now with the given truth; its own CI passes
/// now when it is instant.
fn add(w: &mut World, conflicts: bool, key: u64) -> u64 {
    w.fire(Event::Arrival);
    let number = *w.prs.last_key_value().expect("arrived").0;
    let pr = w.prs.get_mut(&number).expect("arrived");
    pr.conflicts_with_trunk = conflicts;
    pr.key = key;
    settle(w);
    number.0
}

/// Breaks `pr` against trunk now, as a landing that conflicts with it
/// would.
fn break_now(w: &mut World, pr: u64) {
    let now = w.now();
    w.prs.get_mut(&PrNumber(pr)).expect("arrived").broke_at = Some(now);
}

/// Lands `prs` alone on trunk in one plan, its run cut to an instant.
fn land_now(w: &mut World, id: u64, prs: &[u64]) {
    let trunk = w.observe().unwrap().trunk;
    start(w, id, &trunk, prs).unwrap();
    let Run::Running { done, .. } = w.batches[&BatchId(id)].run else {
        panic!("batch {id} runs");
    };
    w.events.remove(&done);
    w.complete(BatchId(id));
    w.observe().unwrap();
    w.land(BatchId(id)).unwrap();
}

/// Fires every event due now.
fn settle(w: &mut World) {
    w.advance_to(w.now());
}

/// The events recorded since the last call, as `--trace` prints them,
/// without the pull requests' own CI.
fn queue_lines(w: &mut World) -> Vec<String> {
    let lines = lines(w).into_iter();
    lines.filter(|l| !l.starts_with("pr-ci ")).collect()
}

/// The events recorded since the last call, as `--trace` prints them.
fn lines(w: &mut World) -> Vec<String> {
    w.take_trace().iter().map(ToString::to_string).collect()
}

/// [`lines`], the events also appended to `kept`, so that a test can check
/// every life over the whole trace at its end.
fn kept_lines(w: &mut World, kept: &mut Vec<TraceEvent>) -> Vec<String> {
    let events = w.take_trace();
    let lines = events.iter().map(ToString::to_string).collect();
    kept.extend(events);
    lines
}

/// [`kept_lines`] without the pull requests' own CI, as [`queue_lines`].
fn kept_queue_lines(w: &mut World, kept: &mut Vec<TraceEvent>) -> Vec<String> {
    let lines = kept_lines(w, kept).into_iter();
    lines.filter(|l| !l.starts_with("pr-ci ")).collect()
}

fn start(w: &mut World, id: u64, base: &Sha, prs: &[u64]) -> Result<(), ForgeError> {
    w.start_batch(&BatchStart::new(
        BatchId(id),
        base.clone(),
        prs.iter().map(|&n| PrNumber(n)).collect(),
    ))
}

fn state(o: &Observation, id: u64) -> BatchState {
    let b = o.batches.iter().find(|b| b.id == BatchId(id));
    b.expect("tracked").state.clone()
}

fn tip(o: &Observation, id: u64) -> Sha {
    match state(o, id) {
        BatchState::Testing { tip, .. } => tip,
        other => panic!("batch {id} is {other:?}"),
    }
}

/// Fires every scheduled event.
fn finish(w: &mut World) {
    while let Some(t) = w.next_event() {
        w.advance_to(t);
    }
}

fn ci(w: &mut World, id: u64) -> CiStatus {
    let o = w.observe().unwrap();
    match state(&o, id) {
        BatchState::Testing { ci, .. } => ci,
        other => panic!("batch {id} is {other:?}"),
    }
}

/// Runs `prs` alone on trunk and returns its verdict.
fn verdict_of(w: &mut World, id: u64, prs: &[u64]) -> CiStatus {
    let trunk = w.observe().unwrap().trunk;
    start(w, id, &trunk, prs).unwrap();
    finish(w);
    ci(w, id)
}

fn violation_of(result: Result<(), ForgeError>) -> String {
    match result {
        Err(ForgeError::Permanent(e)) => e.to_string(),
        Err(ForgeError::Transient(e)) => panic!("transient: {e}"),
        Ok(()) => panic!("accepted"),
    }
}

/// Keys `(a, b, c)` such that only `a` and `b` conflict at `p = 0.5`.
fn only_first_two_conflict() -> (u64, u64, u64) {
    let inter = |x, y| unit(mix(x, y)) < 0.5;
    let b = (2..).find(|&b| inter(1, b)).unwrap();
    let c = (2..).find(|&c| !inter(1, c) && !inter(b, c)).unwrap();
    (1, b, c)
}

#[test]
fn an_arrival_appears_on_trunk_with_its_drawn_mergeability() {
    let mut cfg = config(0.0, 0.0);
    cfg.arrivals_per_hour = 100.0;
    cfg.p_conflict = 1.0;
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(1, 0)).traced();
    let t = w.next_event().expect("an arrival is scheduled");
    w.advance_to(t);
    let o = w.observe().unwrap();
    assert_eq!(o.prs.len(), 1);
    assert_eq!(o.prs[0].base, PrBase::Trunk);
    assert_eq!(o.prs[0].status, PrStatus::Conflicting(ConflictWith::Trunk));
    assert_eq!(
        lines(&mut w),
        ["arrive 1", "pr-ci 1", "admit 1", "pr-ci 1 passed"]
    );
}

#[test]
fn a_run_fails_for_a_bad_member_and_passes_without_one() {
    let mut w = world(0.0, 0.0);
    let good = add(&mut w, false, 1);
    let bad = add(&mut w, false, 2);
    break_now(&mut w, bad);
    assert_eq!(verdict_of(&mut w, 1, &[good]), CiStatus::Passed);
    assert!(matches!(
        verdict_of(&mut w, 2, &[good, bad]),
        CiStatus::Failed(_)
    ));
    let trace = [
        "arrive 1",
        "admit 1",
        "arrive 2",
        "admit 2",
        "start 1 [1] on trunk",
        "ci 1 passed",
        "start 2 [1,2] on trunk",
        "ci 2 failed broken",
    ];
    assert_eq!(queue_lines(&mut w), trace);
}

#[test]
fn a_run_fails_for_a_conflicting_own_member_pair() {
    let (a, b, c) = only_first_two_conflict();
    let mut w = world(0.5, 0.0);
    let pa = add(&mut w, false, a);
    let pb = add(&mut w, false, b);
    let pc = add(&mut w, false, c);
    assert_eq!(verdict_of(&mut w, 1, &[pa, pc]), CiStatus::Passed);
    assert!(matches!(
        verdict_of(&mut w, 2, &[pa, pb]),
        CiStatus::Failed(_)
    ));
}

#[test]
fn a_landing_breaks_a_present_conflicting_pull_request() {
    let mut w = world(1.0, 0.0);
    let first = add(&mut w, false, 1);
    let second = add(&mut w, false, 2);
    assert_eq!(verdict_of(&mut w, 1, &[first]), CiStatus::Passed);
    w.take_trace();
    w.land(BatchId(1)).unwrap();
    let landed_at = w.now();
    assert_eq!(lines(&mut w), ["land 1 [1]", "broken 2 by 1"]);
    assert_eq!(w.prs[&PrNumber(second)].broke_at, Some(landed_at));
    assert_eq!(w.bad_since(PrNumber(second)), Some(landed_at));
    assert!(matches!(
        verdict_of(&mut w, 2, &[second]),
        CiStatus::Failed(_)
    ));
    w.observe().unwrap();
    let reason = EjectReason::BatchFailed { batch: BatchId(2) };
    w.eject(PrNumber(second), &reason).unwrap();
    let left = w.prs[&PrNumber(second)].record.left.clone();
    let justified = Exit::Ejected {
        reason,
        bad_since: Some(landed_at),
    };
    assert_eq!(left, Some((w.now(), justified)));
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_pull_request_arriving_after_its_conflicting_partner_landed_is_not_broken_by_it() {
    let mut w = world(1.0, 0.0);
    let first = add(&mut w, false, 1);
    assert_eq!(verdict_of(&mut w, 1, &[first]), CiStatus::Passed);
    w.land(BatchId(1)).unwrap();
    let later = add(&mut w, false, 2);
    assert_eq!(w.bad_since(PrNumber(later)), None);
    assert_eq!(verdict_of(&mut w, 2, &[later]), CiStatus::Passed);
    w.observe().unwrap();
    w.land(BatchId(2)).unwrap();
    assert!(w.prs[&PrNumber(later)].landed());
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_landing_during_a_run_leaves_its_verdict_and_the_batch_is_refused() {
    let mut w = world(1.0, 0.0);
    let first = add(&mut w, false, 1);
    let second = add(&mut w, false, 2);
    let trunk = w.observe().unwrap().trunk;
    start(&mut w, 2, &trunk, &[second]).unwrap();
    land_now(&mut w, 1, &[first]);
    // Broken while batch 2 runs: its verdict is its tree's at its start.
    assert!(w.bad_since(PrNumber(second)).is_some());
    finish(&mut w);
    assert_eq!(ci(&mut w, 2), CiStatus::Passed);
    w.land(BatchId(2)).unwrap();
    let o = w.observe().unwrap();
    assert!(matches!(
        state(&o, 2),
        BatchState::LandRefused {
            reason: LandRefusal::TrunkMoved { .. },
            ..
        }
    ));
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_flake_fails_a_clean_run() {
    let mut w = world(0.0, 1.0);
    let good = add(&mut w, false, 1);
    assert!(matches!(
        verdict_of(&mut w, 1, &[good]),
        CiStatus::Failed(_)
    ));
}

#[test]
fn a_child_fails_with_its_parents_tree() {
    let (a, b, c) = only_first_two_conflict();
    for parent in [vec![(true, a)], vec![(false, a), (false, b)]] {
        let mut w = world(0.5, 0.0);
        let members: Vec<u64> = parent
            .iter()
            .map(|&(bad, key)| {
                let pr = add(&mut w, false, key);
                if bad {
                    break_now(&mut w, pr);
                }
                pr
            })
            .collect();
        let child = add(&mut w, false, c);
        let o = w.observe().unwrap();
        start(&mut w, 1, &o.trunk, &members).unwrap();
        let unobserved = tip(&w.snapshot().unwrap(), 1);
        start(&mut w, 2, &unobserved, &[child]).unwrap_err();
        // The child's base must be in the latest observation.
        let o = w.observe().unwrap();
        start(&mut w, 2, &tip(&o, 1), &[child]).unwrap();
        finish(&mut w);
        assert!(
            matches!(ci(&mut w, 2), CiStatus::Failed(_)),
            "parent {parent:?}"
        );
    }
}

#[test]
fn a_probe_runs_on_its_base_tree_alone() {
    let (a, b, c) = only_first_two_conflict();
    let mut w = world(0.5, 0.0);
    let pa = add(&mut w, false, a);
    let pb = add(&mut w, false, b);
    let pc = add(&mut w, false, c);
    let o = w.observe().unwrap();
    start(&mut w, 1, &o.trunk, &[pa, pb]).unwrap();
    start(&mut w, 2, &o.trunk, &[pa, pc]).unwrap();
    let o = w.observe().unwrap();
    start(&mut w, 3, &o.trunk, &[]).unwrap();
    start(&mut w, 4, &tip(&o, 1), &[]).unwrap();
    start(&mut w, 5, &tip(&o, 2), &[]).unwrap();
    let o = w.observe().unwrap();
    for id in [3, 4, 5] {
        let probe = o.batches.iter().find(|b| b.id == BatchId(id)).unwrap();
        assert!(probe.members.is_empty());
        assert_ne!(tip(&o, id), probe.base);
    }
    finish(&mut w);
    assert_eq!(ci(&mut w, 3), CiStatus::Passed);
    assert!(matches!(ci(&mut w, 4), CiStatus::Failed(_)));
    assert_eq!(ci(&mut w, 5), CiStatus::Passed);
    let probes: Vec<&RunRecord> = w.runs().iter().filter(|r| r.batch.0 >= 3).collect();
    assert!(probes.iter().all(|r| r.members == 0), "{probes:?}");
    assert_eq!(probes[1].tree, [pa, pb].map(PrNumber));
}

#[test]
fn a_passed_probe_cannot_land() {
    let mut w = world(0.0, 0.0);
    add(&mut w, false, 1);
    assert_eq!(verdict_of(&mut w, 1, &[]), CiStatus::Passed);
    assert!(violation_of(w.land(BatchId(1))).contains("is a probe"));
    w.retire(BatchId(1)).unwrap();
    assert!(w.observe().unwrap().batches.is_empty());
}

#[test]
fn retiring_a_running_batch_charges_its_elapsed_time() {
    let mut w = world(0.0, 0.0);
    let (p, q) = (add(&mut w, false, 1), add(&mut w, false, 2));
    let trunk = w.observe().unwrap().trunk;
    start(&mut w, 1, &trunk, &[p]).unwrap();
    start(&mut w, 2, &trunk, &[q]).unwrap();
    w.advance_to(240);
    w.observe().unwrap();
    w.retire(BatchId(1)).unwrap();
    w.retire(BatchId(2)).unwrap();
    let consumed: Vec<u64> = w.runs().iter().map(|r| r.consumed).collect();
    assert_eq!(consumed, [240, 240]);
    assert_eq!(w.next_event(), None);
    let m = w.metrics(End::drained(w.now()));
    assert_eq!(m.runner_hours, 480.0 / 3600.0);
    assert_eq!(m.wasted_fraction, Some(1.0));
    assert_eq!(m.cancelled_runs, 2);
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn land_refuses_trunk_moved_and_member_gone_and_lands_a_chain_in_order() {
    let mut w = world(0.0, 0.0);
    let prs: Vec<u64> = (0..4).map(|k| add(&mut w, false, k)).collect();
    let o = w.observe().unwrap();
    start(&mut w, 1, &o.trunk, &[prs[0]]).unwrap();
    start(&mut w, 2, &o.trunk, &[prs[1]]).unwrap();
    let o = w.observe().unwrap();
    start(&mut w, 3, &tip(&o, 1), &[prs[2]]).unwrap();
    finish(&mut w);
    w.observe().unwrap();
    w.land(BatchId(1)).unwrap();
    w.land(BatchId(3)).unwrap();
    w.land(BatchId(2)).unwrap();
    let o = w.observe().unwrap();
    let BatchState::Landed { tip: chain_tip } = state(&o, 3) else {
        panic!("batch 3 landed");
    };
    assert_eq!(o.trunk, chain_tip);
    assert!(matches!(
        state(&o, 2),
        BatchState::LandRefused {
            reason: LandRefusal::TrunkMoved { .. },
            ..
        }
    ));
    start(&mut w, 4, &o.trunk, &[prs[3]]).unwrap();
    w.fire(Event::Repush(PrNumber(prs[3])));
    finish(&mut w);
    w.observe().unwrap();
    w.land(BatchId(4)).unwrap();
    let o = w.observe().unwrap();
    let gone = LandRefusal::MemberGone {
        pr: PrNumber(prs[3]),
    };
    assert!(matches!(state(&o, 4), BatchState::LandRefused { reason, .. } if reason == gone));
    assert_eq!(w.check_invariants(), Ok(()));
    assert!(w.runs()[0].landed && !w.runs()[1].landed && w.runs()[2].landed);
}

#[test]
fn departed_matches_each_exit_and_each_exit_is_traced() {
    let mut w = world(0.0, 0.0);
    let prs: Vec<u64> = (0..4).map(|k| add(&mut w, false, k)).collect();
    stack(&mut w, prs[3], prs[1], false);
    break_now(&mut w, prs[1]);
    let o = w.observe().unwrap();
    assert!(o.departed.is_empty());
    let heads: Vec<Sha> = o.prs.iter().map(|p| p.head.clone()).collect();
    start(&mut w, 1, &o.trunk, &[prs[0]]).unwrap();
    finish(&mut w);
    w.observe().unwrap();
    w.land(BatchId(1)).unwrap();
    let reason = EjectReason::BatchFailed { batch: BatchId(1) };
    w.eject(PrNumber(prs[1]), &reason).unwrap();
    w.fire(Event::Withdraw(PrNumber(prs[2])));
    let trace = lines(&mut w);
    assert_eq!(
        trace[trace.len() - 4..],
        [
            "land 1 [1]",
            "eject 2 justified: batch 1 failed",
            "blocked 4 on 2 bad",
            "withdraw 3"
        ]
    );
    let o = w.observe().unwrap();
    let how = [
        Departed::Landed { batch: BatchId(1) },
        Departed::Ejected,
        Departed::Withdrawn(Withdrawal::IntentCleared),
        Departed::Withdrawn(Withdrawal::Blocked {
            on: PrNumber(prs[1]),
        }),
    ];
    let expected: Vec<dorfq_core::Departure> = (0..4)
        .map(|i| dorfq_core::Departure::new(PrNumber(prs[i]), heads[i].clone(), how[i].clone()))
        .collect();
    assert_eq!(o.departed, expected);
    assert!(lines(&mut w).is_empty(), "an observation records nothing");
    assert!(w.observe().unwrap().departed.is_empty());
}

#[test]
fn a_pull_request_conflicting_with_trunk_cannot_join_a_batch() {
    let mut w = world(0.0, 0.0);
    let p = add(&mut w, false, 1);
    let q = add(&mut w, true, 2);
    let o = w.observe().unwrap();
    assert_eq!(o.prs[0].status, PrStatus::Ready);
    let message = violation_of(start(&mut w, 1, &o.trunk, &[p, q]));
    assert!(message.contains("not ready"), "{message}");
    assert_eq!((w.runs().len(), w.next_event()), (0, None));
}

#[test]
fn every_violation_is_permanent_and_named() {
    let mut w = world(0.0, 0.0);
    let (p, q) = (add(&mut w, false, 1), add(&mut w, false, 2));
    let o = w.observe().unwrap();
    let trunk = o.trunk.clone();
    start(&mut w, 1, &trunk, &[p]).unwrap();
    let cases: Vec<(Result<(), ForgeError>, &str)> = vec![
        (start(&mut w, 1, &trunk, &[q]), "already in use"),
        (start(&mut w, 3, &trunk, &[q, q]), "twice"),
        (start(&mut w, 4, &trunk, &[99]), "not known"),
        (start(&mut w, 5, &commit(999), &[q]), "neither trunk"),
        (w.land(BatchId(1)), "batch 1 is not known"),
        (w.retire(BatchId(7)), "retire: batch 7 is not known"),
        (
            w.eject(
                PrNumber(99),
                &EjectReason::Conflicts {
                    with: ConflictWith::Trunk,
                },
            ),
            "pr 99 is not known",
        ),
    ];
    for (result, expected) in cases {
        let message = violation_of(result);
        assert!(message.contains(expected), "{message} lacks {expected}");
    }
    w.observe().unwrap();
    assert!(violation_of(w.land(BatchId(1))).contains("not reported as passed"));
    finish(&mut w);
    w.observe().unwrap();
    w.land(BatchId(1)).unwrap();
    assert!(violation_of(w.land(BatchId(1))).contains("already landed"));
    assert!(violation_of(start(&mut w, 8, &trunk, &[p])).contains("this plan lands"));
    let reason = EjectReason::Conflicts {
        with: ConflictWith::Trunk,
    };
    assert!(violation_of(w.eject(PrNumber(p), &reason)).contains("this plan lands"));
    assert!(violation_of(w.retire(BatchId(1))).contains("is landed in this plan"));
}

#[test]
fn observations_are_sorted() {
    let mut w = world(0.0, 0.0);
    let prs: Vec<u64> = (0..3).map(|k| add(&mut w, false, k)).collect();
    let trunk = w.observe().unwrap().trunk;
    start(&mut w, 9, &trunk, &[prs[2]]).unwrap();
    start(&mut w, 4, &trunk, &[prs[0]]).unwrap();
    let o = w.observe().unwrap();
    assert!(o.prs.windows(2).all(|p| p[0].number < p[1].number));
    assert!(o.batches.windows(2).all(|b| b[0].id < b[1].id));
}

#[test]
fn an_ejection_is_justified_only_by_badness_against_trunk() {
    // 1 and 2 conflict, both queued; 3 is broken. A queued partner does
    // not justify ejecting 1; being broken justifies ejecting 3.
    let (a, b, c) = only_first_two_conflict();
    let mut w = world(0.5, 0.0);
    let good = add(&mut w, false, a);
    add(&mut w, false, b);
    w.advance_to(60);
    let bad = add(&mut w, false, c);
    break_now(&mut w, bad);
    w.observe().unwrap();
    w.take_trace();
    let reason = EjectReason::BatchFailed { batch: BatchId(1) };
    w.eject(PrNumber(good), &reason).unwrap();
    w.eject(PrNumber(bad), &reason).unwrap();
    let exits: Vec<Option<(u64, Exit)>> = [good, bad]
        .iter()
        .map(|&n| w.prs[&PrNumber(n)].record.left.clone())
        .collect();
    let ejected = |bad_since| {
        let reason = reason.clone();
        Some((60, Exit::Ejected { reason, bad_since }))
    };
    assert_eq!(exits, [ejected(None), ejected(Some(60))]);
    assert_eq!(
        lines(&mut w)[..],
        [
            "eject 1 unjustified: batch 1 failed",
            "eject 3 justified: batch 1 failed"
        ][..]
    );
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.ejected_good, m.ejected_bad), (1, 1));
}

#[test]
fn a_child_of_a_bad_base_is_bad_from_its_base_s_breakage() {
    let mut w = world(0.0, 0.0);
    tree(&mut w);
    w.advance_to(60);
    break_now(&mut w, 1);
    w.advance_to(120);
    break_now(&mut w, 3);
    // 3 on 2 on 1: bad since 1 broke, the earlier; 5 on trunk is good.
    let since: Vec<Option<u64>> = (1..=5).map(|n| w.bad_since(PrNumber(n))).collect();
    assert_eq!(since, [Some(60), Some(60), Some(60), Some(60), None]);
    // A base that left no longer makes its children bad.
    w.prs.get_mut(&PrNumber(1)).unwrap().record.left = Some((120, Exit::Landed { bad: false }));
    let since: Vec<Option<u64>> = (2..=4).map(|n| w.bad_since(PrNumber(n))).collect();
    assert_eq!(since, [None, Some(120), None]);
}

#[test]
fn churn_repushes_and_withdraws_on_their_clocks() {
    let mut cfg = config(0.0, 0.0);
    cfg.repush_per_hour = 1.0;
    cfg.withdraw_per_hour = 0.5;
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(1, 0)).traced();
    let p = add(&mut w, false, 1);
    let head = w.prs[&PrNumber(p)].head.clone();
    finish(&mut w);
    let trace = lines(&mut w);
    let pr = &w.prs[&PrNumber(p)];
    assert!(matches!(pr.record.left, Some((_, Exit::Withdrawn))));
    assert_eq!(trace.last().map(String::as_str), Some("withdraw 1"));
    if trace.iter().any(|l| l == "repush 1") {
        assert_ne!(pr.head, head);
    }
    assert!(w.observe().unwrap().prs.is_empty());
}

#[test]
fn a_churn_delay_past_the_end_of_the_clock_schedules_nothing() {
    let mut cfg = config(0.0, 0.0);
    cfg.repush_per_hour = 1e-30;
    cfg.withdraw_per_hour = 1e-30;
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(1, 0));
    add(&mut w, false, 1);
    assert_eq!(w.next_event(), None);
}

#[test]
fn arrivals_and_truths_do_not_depend_on_what_the_scheduler_does() {
    let mut cfg = config(0.0, 0.0);
    cfg.arrivals_per_hour = 20.0;
    cfg.p_conflict = 0.5;
    let truths = |busy: bool| {
        let mut w = World::new(
            cfg.clone(),
            Admission::Label,
            Streams::for_replication(3, 1),
        );
        while let Some(t) = w.next_event() {
            w.advance_to(t);
            let o = w.observe().unwrap();
            if busy && o.batches.is_empty() {
                start(&mut w, t + 1, &o.trunk, &[o.prs[0].number.0]).unwrap();
            }
        }
        let prs = w.prs.values();
        prs.map(|p| (p.record.arrived_at, p.conflicts_with_trunk, p.key))
            .collect::<Vec<_>>()
    };
    let idle = truths(false);
    assert!(idle.len() > 5);
    assert_eq!(idle, truths(true));
}

/// Makes `pr` stacked on `base`, as if it arrived so.
fn stack(w: &mut World, pr: u64, base: u64, atomic: bool) {
    let p = w.prs.get_mut(&PrNumber(pr)).expect("arrived");
    p.base = Some(PrNumber(base));
    p.atomic = atomic;
}

/// Pull requests 1 to 5: 2 on 1, 3 (`Atomic`) on 2, 4 on 1, 5 on trunk.
fn tree(w: &mut World) {
    for key in 1..=5 {
        add(w, false, key);
    }
    stack(w, 2, 1, false);
    stack(w, 3, 2, true);
    stack(w, 4, 1, false);
}

fn bases(o: &Observation) -> Vec<(u64, PrBase, StackPolicy)> {
    o.prs
        .iter()
        .map(|p| (p.number.0, p.base, p.stack))
        .collect()
}

fn heads(w: &World) -> Vec<Sha> {
    w.prs.values().map(|p| p.head.clone()).collect()
}

/// A world whose first stack, of five, is atomic and drawn to conflict.
fn stack_of_five(admission: Admission, pr_ci: u64) -> World {
    let mut cfg = config(0.0, 0.0);
    cfg.stack_sizes = always(5);
    cfg.p_atomic = 1.0;
    cfg.p_conflict = 1.0;
    cfg.pr_ci_minutes = Minutes {
        min: pr_ci,
        max: pr_ci,
    };
    World::new(cfg, admission, Streams::for_replication(1, 0)).traced()
}

#[test]
fn a_stack_of_five_arrives_at_one_instant_each_on_the_one_before() {
    let mut w = stack_of_five(Admission::Label, 30);
    w.advance_to(100);
    w.fire(Event::Arrival);
    let arrivals: Vec<String> = queue_lines(&mut w)
        .into_iter()
        .filter(|l| l.starts_with("arrive "))
        .collect();
    assert_eq!(
        arrivals,
        [
            "arrive 1",
            "arrive 2 on 1 atomic",
            "arrive 3 on 2 atomic",
            "arrive 4 on 3 atomic",
            "arrive 5 on 4 atomic",
        ]
    );
    assert!(w.prs.values().all(|p| p.record.arrived_at == 100));
    // Under label all five are in the queue at once, stacked, held for
    // their own CI; only the root conflicts.
    let o = w.observe().unwrap();
    assert_eq!(observed(&o), [1, 2, 3, 4, 5]);
    assert_eq!(
        bases(&o)[1..],
        [
            (2, PrBase::Pr(PrNumber(1)), StackPolicy::Atomic),
            (3, PrBase::Pr(PrNumber(2)), StackPolicy::Atomic),
            (4, PrBase::Pr(PrNumber(3)), StackPolicy::Atomic),
            (5, PrBase::Pr(PrNumber(4)), StackPolicy::Atomic),
        ]
    );
    assert_eq!(
        (o.prs[0].base, o.prs[0].stack),
        (PrBase::Trunk, StackPolicy::Splittable)
    );
    assert_eq!(o.prs[0].status, PrStatus::Conflicting(ConflictWith::Trunk));
    assert!(o.prs[1..].iter().all(|p| p.status == pending()));
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn under_github_a_stack_of_five_enters_by_its_root_alone() {
    let mut w = stack_of_five(Admission::Github, 30);
    // A root that does not conflict, so it enters once its run passes.
    w.fire(Event::Arrival);
    w.prs.get_mut(&PrNumber(1)).unwrap().conflicts_with_trunk = false;
    assert!(observed(&w.observe().unwrap()).is_empty());
    assert_eq!(w.out(), 5);
    w.advance_to(30 * 60);
    assert_eq!(observed(&w.observe().unwrap()), [1]);
    let waits: Vec<String> = queue_lines(&mut w)
        .into_iter()
        .filter(|l| l.starts_with("wait ") || l.starts_with("admit "))
        .collect();
    assert_eq!(
        waits,
        [
            "admit 1",
            "wait 2 on 1",
            "wait 3 on 2",
            "wait 4 on 3",
            "wait 5 on 4"
        ]
    );
    assert_eq!(w.check_invariants(), Ok(()));
}

/// A world whose stacks are three pull requests and grow once, by two,
/// `minutes` after they arrive.
fn growing(hours: u64, minutes: u64) -> World {
    let mut cfg = config(0.0, 0.0);
    cfg.hours = hours;
    cfg.stack_sizes = always(3);
    cfg.p_stack_grows = 1.0;
    cfg.stack_growth_sizes = always(2);
    cfg.stack_growth_minutes = Minutes {
        min: minutes,
        max: minutes,
    };
    cfg.p_atomic = 1.0;
    cfg.p_conflict = 1.0;
    World::new(cfg, Admission::Label, Streams::for_replication(2, 0)).traced()
}

#[test]
fn a_stack_grows_on_what_is_left_of_it_and_draws_the_same_whatever_is_left() {
    // Withdraws `gone` before the growth fires, an hour in.
    let grown = |gone: &[u64]| {
        let mut w = growing(4, 60);
        w.fire(Event::Arrival);
        for &pr in gone {
            w.fire(Event::Withdraw(PrNumber(pr)));
        }
        settle(&mut w);
        let at = w.next_event().unwrap();
        assert_eq!(at, 60 * 60);
        w.advance_to(at);
        let o = w.observe().unwrap();
        (w.truths(), o)
    };
    let (kept, o) = grown(&[]);
    assert_eq!(kept.len(), 5);
    assert_eq!(observed(&o), [1, 2, 3, 4, 5]);
    // The growth is on the burst's top, the rest on it; only 1 conflicts.
    let growth: Vec<Option<PrNumber>> = kept[3..].iter().map(|t| t.3).collect();
    assert_eq!(growth, [Some(PrNumber(3)), Some(PrNumber(4))]);
    assert!(kept.iter().all(|t| t.0 == kept[0].0 || t.0 == 60 * 60));
    assert!(kept[0].1 && kept[1..].iter().all(|t| !t.1));
    let o_kept = o;
    // With the top gone it is on the next member down.
    let (top_gone, o) = grown(&[3]);
    assert_eq!(top_gone[3].3, Some(PrNumber(2)));
    assert_eq!(
        (o.prs[2].number, o.prs[2].base, o.prs[2].stack),
        (PrNumber(4), PrBase::Pr(PrNumber(2)), StackPolicy::Atomic)
    );
    // With the whole stack gone it is a stack of its own on trunk, its
    // root splittable and, never conflicting, ready once checked.
    let (all_gone, o) = grown(&[1]);
    assert_eq!(all_gone[3].3, None);
    assert_eq!(observed(&o), [4, 5]);
    assert_eq!(
        (o.prs[0].base, o.prs[0].stack, &o.prs[0].status),
        (PrBase::Trunk, StackPolicy::Splittable, &PrStatus::Ready)
    );
    assert_eq!(
        (o.prs[1].base, o.prs[1].stack),
        (PrBase::Pr(PrNumber(4)), StackPolicy::Atomic)
    );
    // Every draw is the same; only the bases differ.
    let truth = |t: &[(u64, bool, u64, Option<PrNumber>)]| -> Vec<_> {
        t.iter().map(|&(a, c, k, _)| (a, c, k)).collect()
    };
    assert_eq!(truth(&kept), truth(&top_gone));
    assert_eq!(truth(&kept), truth(&all_gone));
    assert_eq!(o_kept.prs.len(), 5);
}

#[test]
fn a_growth_past_the_horizon_is_not_scheduled() {
    // Arrivals stop at an hour; the growth would come at two.
    let mut w = growing(1, 120);
    w.fire(Event::Arrival);
    assert!(
        !w.events.values().any(|e| matches!(e, Event::Grow { .. })),
        "{:?}",
        w.events
    );
    // Inside the horizon it is.
    let mut w = growing(3, 120);
    w.fire(Event::Arrival);
    let grow = w
        .events
        .iter()
        .find(|(_, e)| matches!(e, Event::Grow { .. }));
    assert_eq!(
        grow.map(|(k, e)| (k.0, *e)),
        Some((
            120 * 60,
            Event::Grow {
                on: PrNumber(3),
                size: 2,
                atomic: true
            }
        ))
    );
}

#[test]
fn a_growth_with_no_delay_arrives_in_the_same_pass_after_its_stack() {
    let mut w = growing(1, 0);
    w.fire(Event::Arrival);
    settle(&mut w);
    let arrivals: Vec<String> = queue_lines(&mut w)
        .into_iter()
        .filter(|l| l.starts_with("arrive "))
        .collect();
    assert_eq!(arrivals.len(), 5, "{arrivals:?}");
    assert_eq!(arrivals[3], "arrive 4 on 3 atomic");
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_landed_base_leaves_its_children_reported_on_trunk() {
    let mut w = world(0.0, 0.0);
    tree(&mut w);
    let o = w.observe().unwrap();
    let splittable = StackPolicy::Splittable;
    assert_eq!(
        bases(&o),
        [
            (1, PrBase::Trunk, splittable),
            (2, PrBase::Pr(PrNumber(1)), splittable),
            (3, PrBase::Pr(PrNumber(2)), StackPolicy::Atomic),
            (4, PrBase::Pr(PrNumber(1)), splittable),
            (5, PrBase::Trunk, splittable),
        ]
    );
    assert_eq!(verdict_of(&mut w, 1, &[1, 2, 3]), CiStatus::Passed);
    w.land(BatchId(1)).unwrap();
    let o = w.observe().unwrap();
    assert_eq!(
        bases(&o),
        [
            (4, PrBase::Trunk, splittable),
            (5, PrBase::Trunk, splittable)
        ]
    );
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_base_leaving_blocks_its_descendants_recursively() {
    let mut w = world(0.0, 0.0);
    tree(&mut w);
    w.observe().unwrap();
    w.take_trace();
    w.fire(Event::Withdraw(PrNumber(1)));
    assert_eq!(
        lines(&mut w),
        [
            "withdraw 1",
            "blocked 2 on 1",
            "blocked 3 on 2",
            "blocked 4 on 1"
        ]
    );
    let o = w.observe().unwrap();
    let how: Vec<(u64, Departed)> = o.departed.iter().map(|d| (d.pr.0, d.how.clone())).collect();
    let blocked = |on| Departed::Withdrawn(Withdrawal::Blocked { on: PrNumber(on) });
    assert_eq!(
        how,
        [
            (1, Departed::Withdrawn(Withdrawal::IntentCleared)),
            (2, blocked(1)),
            (3, blocked(2)),
            (4, blocked(1)),
        ]
    );
    assert_eq!(bases(&o), [(5, PrBase::Trunk, StackPolicy::Splittable)]);
    let m = w.metrics(End::drained(w.now()));
    // The three that left with 1 count under its withdrawal.
    assert_eq!((m.withdrawn, m.left_with_base, m.unfinished), (4, 3, 1));
}

#[test]
fn an_ejection_mid_stack_blocks_only_what_is_above_it() {
    let mut w = world(0.0, 0.0);
    tree(&mut w);
    w.observe().unwrap();
    let reason = EjectReason::Conflicts {
        with: ConflictWith::Trunk,
    };
    w.eject(PrNumber(2), &reason).unwrap();
    let o = w.observe().unwrap();
    let how: Vec<(u64, Departed)> = o.departed.iter().map(|d| (d.pr.0, d.how.clone())).collect();
    let on_2 = Departed::Withdrawn(Withdrawal::Blocked { on: PrNumber(2) });
    assert_eq!(how, [(2, Departed::Ejected), (3, on_2)]);
    let queued: Vec<u64> = o.prs.iter().map(|p| p.number.0).collect();
    assert_eq!(queued, [1, 4, 5]);
}

#[test]
fn an_ejection_in_the_plan_that_blocked_a_pull_request_supersedes_the_block() {
    let mut w = world(0.0, 0.0);
    tree(&mut w);
    w.observe().unwrap();
    w.take_trace();
    let reason = EjectReason::BatchFailed { batch: BatchId(1) };
    w.eject(PrNumber(2), &reason).unwrap();
    w.eject(PrNumber(3), &reason).unwrap();
    // 3 was blocked on 2 and then ejected: one departure, the ejection.
    assert_eq!(
        lines(&mut w),
        [
            "eject 2 unjustified: batch 1 failed",
            "eject 3 unjustified: batch 1 failed",
        ]
    );
    let o = w.observe().unwrap();
    let how: Vec<(u64, Departed)> = o.departed.iter().map(|d| (d.pr.0, d.how.clone())).collect();
    assert_eq!(how, [(2, Departed::Ejected), (3, Departed::Ejected)]);
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.ejected_good, m.left_with_base), (2, 0));
}

fn ejected_by_batch_1(w: &mut World, prs: &[u64]) {
    w.observe().unwrap();
    let reason = EjectReason::BatchFailed { batch: BatchId(1) };
    for &pr in prs {
        w.eject(PrNumber(pr), &reason).unwrap();
    }
}

fn exit_of(w: &World, pr: u64) -> Exit {
    w.prs[&PrNumber(pr)].record.left.clone().expect("left").1
}

fn blocked_on(on: u64, bad_since: Option<u64>) -> Exit {
    Exit::Blocked {
        on: PrNumber(on),
        bad_since,
    }
}

#[test]
fn a_base_ejected_wrongly_takes_each_descendant_down_with_its_own_badness() {
    let mut w = world(0.0, 0.0);
    tree(&mut w);
    w.advance_to(60);
    break_now(&mut w, 2);
    w.take_trace();
    ejected_by_batch_1(&mut w, &[1]);
    assert_eq!(
        lines(&mut w),
        [
            "eject 1 unjustified: batch 1 failed",
            "blocked 2 on 1 bad",
            "blocked 3 on 2 bad",
            "blocked 4 on 1"
        ]
    );
    // 3 carries 2's breakage; 4, beside it, is clean.
    assert_eq!(exit_of(&w, 2), blocked_on(1, Some(60)));
    assert_eq!(exit_of(&w, 3), blocked_on(2, Some(60)));
    assert_eq!(exit_of(&w, 4), blocked_on(1, None));
    let m = w.metrics(End::drained(w.now()));
    assert_eq!(
        (
            m.ejected_bad,
            m.ejected_good,
            m.left_with_base,
            m.unfinished
        ),
        (2, 2, 3, 1)
    );
}

#[test]
fn a_bad_base_ejected_takes_its_descendants_down_bad_from_its_breakage() {
    let mut w = world(0.0, 0.0);
    tree(&mut w);
    w.advance_to(60);
    break_now(&mut w, 1);
    w.advance_to(3660);
    ejected_by_batch_1(&mut w, &[1]);
    for (pr, on) in [(2, 1), (3, 2), (4, 1)] {
        assert_eq!(exit_of(&w, pr), blocked_on(on, Some(60)), "pr {pr}");
    }
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.ejected_bad, m.ejected_good), (4, 0));
    // An hour from 1's breakage to the ejection, for all four.
    assert_eq!(
        (m.detect_p50_hours, m.detect_p95_hours),
        (Some(1.0), Some(1.0))
    );
}

#[test]
fn an_ejection_superseding_a_block_keeps_the_badness_from_before_the_base_left() {
    let mut w = world(0.0, 0.0);
    tree(&mut w);
    break_now(&mut w, 1);
    w.take_trace();
    ejected_by_batch_1(&mut w, &[1, 2]);
    // 2's block is taken back; 3 stays blocked on 2.
    assert_eq!(
        lines(&mut w),
        [
            "eject 1 justified: batch 1 failed",
            "blocked 3 on 2 bad",
            "blocked 4 on 1 bad",
            "eject 2 justified: batch 1 failed"
        ]
    );
    assert!(matches!(
        exit_of(&w, 2),
        Exit::Ejected {
            bad_since: Some(0),
            ..
        }
    ));
}

#[test]
fn ever_bad_is_set_at_the_breaking_landing_down_the_stack_and_at_arrival_on_a_bad_base() {
    let (a, b, c) = only_first_two_conflict();
    let mut w = world(0.5, 0.0);
    let first = add(&mut w, false, a);
    let second = add(&mut w, false, b);
    let third = add(&mut w, false, c);
    stack(&mut w, third, second, false);
    land_now(&mut w, 1, &[first]);
    let fourth = w.arrive(Some(PrNumber(second)), false, false).0;
    let fifth = w.arrive(None, false, false).0;
    settle(&mut w);
    let ever: Vec<bool> = [first, second, third, fourth, fifth]
        .iter()
        .map(|&n| w.prs[&PrNumber(n)].record.ever_bad)
        .collect();
    assert_eq!(ever, [false, true, true, true, false]);
    assert_eq!(w.metrics(End::drained(w.now())).ever_bad, 3);
}

#[test]
fn a_repush_restacks_every_descendant() {
    let mut w = world(0.0, 0.0);
    tree(&mut w);
    w.take_trace();
    let before = heads(&w);
    w.fire(Event::Repush(PrNumber(2)));
    let after = heads(&w);
    let changed: Vec<bool> = before.iter().zip(&after).map(|(b, a)| b != a).collect();
    assert_eq!(changed, [false, true, true, false, false]);
    w.fire(Event::Repush(PrNumber(1)));
    let changed: Vec<bool> = after.iter().zip(&heads(&w)).map(|(b, a)| b != a).collect();
    assert_eq!(changed, [true, true, true, true, false]);
    assert_eq!(
        queue_lines(&mut w),
        [
            "repush 2", "repush 3", "repush 1", "repush 2", "repush 3", "repush 4"
        ]
    );
}

#[test]
fn a_pull_request_never_conflicts_with_its_ancestors() {
    let mut w = world(1.0, 0.0);
    tree(&mut w);
    let pairs = |w: &World| -> Vec<(u64, u64)> {
        let numbers: Vec<PrNumber> = w.prs.keys().copied().collect();
        let mut pairs = Vec::new();
        for (k, &i) in numbers.iter().enumerate() {
            for &j in &numbers[k + 1..] {
                if w.conflicts(i, j) {
                    pairs.push((i.0, j.0));
                }
            }
        }
        pairs
    };
    // 2 and 3 are on 1's stack, 4 is on 1; siblings 2 and 4 are not
    // related, nor is anything with 5 on trunk.
    assert_eq!(pairs(&w), [(1, 5), (2, 4), (2, 5), (3, 4), (3, 5), (4, 5)]);
    assert_eq!(verdict_of(&mut w, 1, &[1, 2, 3]), CiStatus::Passed);
}

#[test]
fn two_landed_pull_requests_that_conflict_break_the_invariant_unless_landed_apart() {
    let mut w = world(1.0, 0.0);
    let first = add(&mut w, false, 1);
    let second = add(&mut w, false, 2);
    assert_eq!(verdict_of(&mut w, 1, &[first]), CiStatus::Passed);
    w.land(BatchId(1)).unwrap();
    assert_eq!(w.check_invariants(), Ok(()));
    // 2 was in the world when 1 landed: landing it too breaks the rule.
    let landed = Some((w.now(), Exit::Landed { bad: false }));
    w.prs.get_mut(&PrNumber(second)).unwrap().record.left = landed.clone();
    let message = w.check_invariants().unwrap_err();
    assert!(
        message.contains("prs 1 and 2 landed but conflict"),
        "{message}"
    );
    // Landed bad, through a run that missed the break, it is accepted.
    let bad = Some((w.now(), Exit::Landed { bad: true }));
    w.prs.get_mut(&PrNumber(second)).unwrap().record.left = bad;
    assert_eq!(w.landed_apart(PrNumber(first), PrNumber(second)), Ok(()));
    w.prs.get_mut(&PrNumber(second)).unwrap().record.left = landed;
    // A fix pushed at or after 1's landing makes 2 fresh against it.
    w.prs.get_mut(&PrNumber(second)).unwrap().last_fix_push = Some(w.now());
    assert_eq!(w.landed_apart(PrNumber(first), PrNumber(second)), Ok(()));
}

#[test]
fn a_landing_of_a_bad_or_conflicting_member_breaks_check_landed() {
    let mut w = world(1.0, 0.0);
    let (p, q) = (add(&mut w, false, 1), add(&mut w, false, 2));
    let member =
        |w: &World, n: u64| BatchMember::new(PrNumber(n), w.prs[&PrNumber(n)].head.clone());
    assert_eq!(w.check_landed(&[member(&w, p)], false), Ok(()));
    let both = [member(&w, p), member(&w, q)];
    let message = w.check_landed(&both, false).unwrap_err();
    assert!(
        message.contains("prs 1 and 2 landed but conflict"),
        "{message}"
    );
    assert_eq!(w.check_landed(&both, true), Ok(()));
    break_now(&mut w, p);
    let message = w.check_landed(&[member(&w, p)], false).unwrap_err();
    assert!(message.contains("pr 1 landed but is bad"), "{message}");
    assert_eq!(w.check_landed(&[member(&w, p)], true), Ok(()));
}

#[test]
fn a_landed_pull_request_whose_base_did_not_land_breaks_the_invariant() {
    let mut w = world(0.0, 0.0);
    tree(&mut w);
    w.prs.get_mut(&PrNumber(2)).unwrap().record.left = Some((0, Exit::Landed { bad: false }));
    let message = w.check_invariants().unwrap_err();
    assert!(message.contains("base 1 did not land"), "{message}");
}

#[test]
fn a_world_without_stacks_draws_what_it_drew_before_stacks() {
    let mut cfg = config(0.0, 0.0);
    cfg.arrivals_per_hour = 20.0;
    cfg.p_conflict = 0.5;
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(3, 1));
    finish(&mut w);
    let expected = [
        (67, false, 5323391624043147608, None),
        (167, false, 16137047164741765704, None),
        (215, true, 9595752625188343565, None),
        (263, true, 6229663207608200478, None),
        (487, false, 16846760811485287801, None),
        (567, false, 1566661914958349798, None),
    ];
    assert_eq!(w.truths()[..6], expected);
}

#[test]
fn a_start_names_the_batch_it_runs_on_unless_that_is_trunk() {
    let mut w = world(0.0, 0.0);
    for key in 1..=4 {
        add(&mut w, false, key);
    }
    let trunk = w.observe().unwrap().trunk;
    start(&mut w, 1, &trunk, &[1]).unwrap();
    let tip_1 = tip(&w.observe().unwrap(), 1);
    start(&mut w, 2, &tip_1, &[2]).unwrap();
    finish(&mut w);
    let tip_2 = tip(&w.observe().unwrap(), 2);
    // One plan: batch 1 lands, then batches start on its tip, now trunk,
    // and on batch 2's tip, still standing.
    w.land(BatchId(1)).unwrap();
    start(&mut w, 3, &tip_1, &[3]).unwrap();
    start(&mut w, 4, &tip_2, &[4]).unwrap();
    let starts: Vec<(u64, Option<u64>)> = w
        .take_trace()
        .into_iter()
        .filter_map(|e| match e {
            TraceEvent::Start { batch, base, .. } => Some((batch, base)),
            _ => None,
        })
        .collect();
    assert_eq!(starts, [(1, None), (2, Some(1)), (3, None), (4, Some(2))]);
}

#[test]
fn retirements_refusals_and_ejections_are_traced() {
    let mut w = world(0.0, 0.0);
    for key in 1..=3 {
        add(&mut w, key == 3, key);
    }
    let trunk = w.observe().unwrap().trunk;
    w.take_trace();
    // Batch 2 is cancelled while it runs.
    start(&mut w, 1, &trunk, &[1]).unwrap();
    start(&mut w, 2, &trunk, &[2]).unwrap();
    w.observe().unwrap();
    w.retire(BatchId(2)).unwrap();
    finish(&mut w);
    w.observe().unwrap();
    w.fire(Event::Withdraw(PrNumber(1)));
    w.land(BatchId(1)).unwrap();
    w.observe().unwrap();
    w.retire(BatchId(1)).unwrap();
    let reason = EjectReason::Conflicts {
        with: ConflictWith::Trunk,
    };
    w.eject(PrNumber(3), &reason).unwrap();
    assert_eq!(
        lines(&mut w),
        [
            "start 1 [1] on trunk",
            "start 2 [2] on trunk",
            "retire 2 cancelled",
            "ci 1 passed",
            "withdraw 1",
            "refused 1: #1 is gone",
            "retire 1",
            "eject 3 unjustified: conflicts with trunk, rebasing",
            "rebase 3 conflict",
        ]
    );
}

#[test]
fn an_arrival_carries_its_hidden_truth_and_its_stack() {
    let mut cfg = config(0.0, 0.0);
    cfg.p_conflict = 1.0;
    cfg.stack_sizes = always(8);
    cfg.p_atomic = 0.5;
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(1, 0)).traced();
    w.fire(Event::Arrival);
    let trace = w.take_trace().into_iter();
    let arrivals: Vec<TraceEvent> = trace
        .filter(|e| matches!(e, TraceEvent::Arrive { .. }))
        .collect();
    assert_eq!(arrivals.len(), 8);
    for (event, p) in arrivals.iter().zip(w.prs.values()) {
        let expected = TraceEvent::Arrive {
            t: 0,
            pr: p.record.number.0,
            base: p.base().map(|b| b.0),
            atomic: p.atomic,
            conflicts: p.conflicts_with_trunk,
        };
        assert_eq!(event, &expected);
    }
    // The first is on trunk with its drawn conflict; a cut arrival has none.
    assert!(matches!(
        arrivals[0],
        TraceEvent::Arrive {
            base: None,
            conflicts: true,
            ..
        }
    ));
    assert!(arrivals[1..].iter().all(|e| matches!(
        e,
        TraceEvent::Arrive {
            base: Some(_),
            conflicts: false,
            ..
        }
    )));
}

/// [`world`] whose pull requests' own CI takes `minutes` exactly.
fn pr_ci_world(minutes: u64) -> World {
    let mut cfg = config(0.0, 0.0);
    cfg.pr_ci_minutes = Minutes {
        min: minutes,
        max: minutes,
    };
    World::new(cfg, Admission::Label, Streams::for_replication(1, 0)).traced()
}

fn status(o: &Observation, pr: u64) -> PrStatus {
    let p = o.prs.iter().find(|p| p.number == PrNumber(pr));
    p.expect("observed").status.clone()
}

fn pending() -> PrStatus {
    PrStatus::Waiting(BTreeSet::from([Hold::OwnCiPending]))
}

#[test]
fn an_arrivals_own_ci_starts_at_once_and_is_not_billed() {
    let mut cfg = config(0.0, 0.0);
    cfg.pr_ci_minutes = Minutes { min: 5, max: 25 };
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(1, 0)).traced();
    let first = add(&mut w, false, 1);
    let second = add(&mut w, false, 2);
    assert_eq!(
        lines(&mut w),
        [
            "arrive 1", "pr-ci 1", "admit 1", "arrive 2", "pr-ci 2", "admit 2"
        ]
    );
    assert!(
        w.pr_ci_runs()
            .iter()
            .all(|r| r.started_at == 0 && !r.billed)
    );
    finish(&mut w);
    let durations: Vec<u64> = w.pr_ci_runs().iter().map(|r| r.consumed).collect();
    assert_eq!(durations.len(), 2);
    for d in &durations {
        assert!((300..=1500).contains(d) && d % 60 == 0, "{durations:?}");
    }
    assert_eq!(w.check_invariants(), Ok(()));
    // A batch and a re-push's run both run at once, and only the batch
    // costs the queue.
    let trunk = w.observe().unwrap().trunk;
    start(&mut w, 1, &trunk, &[first]).unwrap();
    w.fire(Event::Repush(PrNumber(second)));
    assert_eq!(w.runs()[0].started_at, w.now());
    assert_eq!(w.pr_ci_runs()[2].started_at, w.now());
    assert_eq!(w.check_invariants(), Ok(()));
    finish(&mut w);
    assert_eq!(w.check_invariants(), Ok(()));
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.billed_pr_ci_runs, m.billed_pr_ci_hours), (0, 0.0));
    assert_eq!(m.runner_hours, 600.0 / HOUR);
}

#[test]
fn a_pull_request_is_held_until_its_own_ci_passes() {
    let mut w = pr_ci_world(20);
    let p = add(&mut w, false, 1);
    let q = add(&mut w, false, 2);
    stack(&mut w, q, p, false);
    let o = w.observe().unwrap();
    assert_eq!(
        (status(&o, p), o.prs[1].base),
        (pending(), PrBase::Pr(PrNumber(p)))
    );
    let message = violation_of(start(&mut w, 1, &o.trunk, &[p]));
    assert!(message.contains("not ready"), "{message}");
    w.advance_to(1199);
    let o = w.observe().unwrap();
    assert_eq!((status(&o, p), status(&o, q)), (pending(), pending()));
    w.advance_to(1200);
    let o = w.observe().unwrap();
    assert_eq!(
        (status(&o, p), status(&o, q)),
        (PrStatus::Ready, PrStatus::Ready)
    );
    start(&mut w, 1, &o.trunk, &[p]).unwrap();
    let starts = w.pr_ci_runs().iter().map(|r| (r.pr.0, r.started_at));
    assert_eq!(starts.collect::<Vec<_>>(), [(1, 0), (2, 0)]);
}

#[test]
fn a_repush_cancels_the_old_heads_run_and_holds_the_new_head() {
    let mut w = pr_ci_world(20);
    let p = add(&mut w, false, 1);
    w.advance_to(1200);
    let o = w.observe().unwrap();
    assert_eq!(status(&o, p), PrStatus::Ready);
    start(&mut w, 1, &o.trunk, &[p]).unwrap();
    w.take_trace();
    w.advance_to(1500);
    w.fire(Event::Repush(PrNumber(p)));
    let o = w.observe().unwrap();
    assert_eq!(status(&o, p), pending());
    assert_ne!(o.prs[0].head, w.pr_ci_runs()[0].head);
    // A second re-push cuts the first new head's run short.
    w.advance_to(1800);
    w.fire(Event::Repush(PrNumber(p)));
    assert_eq!(
        lines(&mut w),
        [
            "repush 1",
            "pr-ci 1",
            "ci 1 passed",
            "repush 1",
            "pr-ci 1 cancelled",
            "pr-ci 1",
        ]
    );
    let cut = &w.pr_ci_runs()[1];
    assert_eq!((cut.consumed, cut.cancelled), (300, true));
    finish(&mut w);
    let o = w.observe().unwrap();
    assert_eq!(status(&o, p), PrStatus::Ready);
    // The batch holds the first head: its landing is refused.
    w.land(BatchId(1)).unwrap();
    let o = w.observe().unwrap();
    let gone = LandRefusal::MemberGone { pr: PrNumber(p) };
    assert!(matches!(state(&o, 1), BatchState::LandRefused { reason, .. } if reason == gone));
    assert_eq!(w.check_invariants(), Ok(()));
    let consumed: Vec<u64> = w.pr_ci_runs().iter().map(|r| r.consumed).collect();
    assert_eq!(consumed, [1200, 300, 1200]);
    assert_eq!(w.metrics(End::drained(w.now())).billed_pr_ci_runs, 0);
}

#[test]
fn a_pull_request_that_leaves_still_runs_its_run_to_the_end() {
    let mut w = pr_ci_world(20);
    add(&mut w, false, 1);
    add(&mut w, false, 2);
    w.take_trace();
    w.fire(Event::Withdraw(PrNumber(1)));
    w.fire(Event::Withdraw(PrNumber(2)));
    finish(&mut w);
    assert_eq!(
        lines(&mut w),
        [
            "withdraw 1",
            "withdraw 2",
            "pr-ci 1 passed",
            "pr-ci 2 passed"
        ]
    );
    let consumed: Vec<u64> = w.pr_ci_runs().iter().map(|r| r.consumed).collect();
    assert_eq!(consumed, [1200, 1200]);
    assert_eq!(w.metrics(End::drained(w.now())).runner_hours, 0.0);
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_pull_requests_own_ci_durations_do_not_depend_on_the_queue() {
    let mut cfg = config(0.0, 0.0);
    cfg.arrivals_per_hour = 20.0;
    cfg.pr_ci_minutes = Minutes { min: 1, max: 30 };
    let durations = |busy: bool| {
        let mut w = World::new(
            cfg.clone(),
            Admission::Label,
            Streams::for_replication(3, 1),
        );
        while let Some(t) = w.next_event() {
            w.advance_to(t);
            let o = w.observe().unwrap();
            let ready = o.prs.iter().find(|p| p.status == PrStatus::Ready);
            if let Some(ready) = ready.filter(|_| busy && o.batches.is_empty()) {
                start(&mut w, t + 1, &o.trunk, &[ready.number.0]).unwrap();
            }
        }
        let runs = w.pr_ci_runs().iter().map(|r| (r.pr, r.consumed));
        runs.collect::<BTreeMap<_, _>>()
    };
    let idle = durations(false);
    assert!(idle.len() > 5);
    assert_eq!(idle, durations(true));
}

/// A world under `admission` whose own CI runs take `pr_ci` minutes and
/// whose rebases take `rebase` minutes, exactly.
fn admitting(admission: Admission, pr_ci: u64, rebase: u64) -> World {
    let mut cfg = config(0.0, 0.0);
    cfg.pr_ci_minutes = Minutes {
        min: pr_ci,
        max: pr_ci,
    };
    cfg.rebase_minutes = Minutes {
        min: rebase,
        max: rebase,
    };
    World::new(cfg, admission, Streams::for_replication(1, 0)).traced()
}

fn conflict() -> EjectReason {
    EjectReason::Conflicts {
        with: ConflictWith::Trunk,
    }
}

fn observed(o: &Observation) -> Vec<u64> {
    o.prs.iter().map(|p| p.number.0).collect()
}

fn departures(o: &Observation) -> Vec<(u64, Departed)> {
    o.departed.iter().map(|d| (d.pr.0, d.how.clone())).collect()
}

/// The whole trace so far, checked along every pull request's life, as
/// `--trace` prints it without the pull requests' own CI.
fn checked_lines(w: &mut World) -> Vec<String> {
    let trace = w.take_trace();
    assert_eq!(crate::trace::check_lives(&trace), Ok(()));
    let lines = trace.iter().map(ToString::to_string);
    lines.filter(|l| !l.starts_with("pr-ci ")).collect()
}

#[test]
fn under_label_a_conflict_ejection_is_a_rebase_and_a_return() {
    let mut w = admitting(Admission::Label, 30, 20);
    let p = add(&mut w, true, 1);
    let o = w.observe().unwrap();
    assert_eq!(status(&o, p), PrStatus::Conflicting(ConflictWith::Trunk));
    let first = o.prs[0].head.clone();
    w.take_trace();
    w.eject(PrNumber(p), &conflict()).unwrap();
    let o = w.observe().unwrap();
    assert_eq!(
        (observed(&o), departures(&o)),
        (vec![], vec![(p, Departed::Ejected)])
    );
    assert_eq!(w.out(), 1);
    w.advance_to(20 * 60);
    let o = w.observe().unwrap();
    assert_ne!(o.prs[0].head, first);
    assert_eq!(status(&o, p), pending());
    w.advance_to(50 * 60);
    assert_eq!(status(&w.observe().unwrap(), p), PrStatus::Ready);
    assert_eq!(
        lines(&mut w),
        [
            "eject 1 unjustified: conflicts with trunk, rebasing",
            "rebase 1 conflict",
            "repush 1",
            "pr-ci 1 cancelled",
            "pr-ci 1",
            "admit 1",
            "pr-ci 1 passed",
        ]
    );
    let m = w.metrics(End::drained(w.now()));
    assert_eq!(
        (
            m.rebases,
            m.ejected_bad,
            m.ejected_good,
            m.billed_pr_ci_runs
        ),
        (1, 0, 0, 0)
    );
    assert_eq!(w.pr_ci_runs().len(), 2);
    // The first head's run was cut at the push, 20 minutes in.
    assert_eq!(w.pr_ci_runs()[0].consumed, 20 * 60);
    assert!(w.pr_ci_runs()[0].cancelled);
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn under_label_an_instant_rebase_returns_at_the_next_observation() {
    let mut w = admitting(Admission::Label, 0, 0);
    let p = add(&mut w, true, 1);
    let first = w.observe().unwrap().prs[0].head.clone();
    w.eject(PrNumber(p), &conflict()).unwrap();
    // The plan's own instant: absent, ejected.
    let o = w.observe().unwrap();
    assert_eq!(departures(&o), [(p, Departed::Ejected)]);
    settle(&mut w);
    let o = w.observe().unwrap();
    assert_eq!(observed(&o), [p]);
    assert_ne!(o.prs[0].head, first);
    assert_eq!(status(&o, p), PrStatus::Ready);
    assert_eq!(checked_lines(&mut w).last().unwrap(), "admit 1");
}

#[test]
fn under_label_a_rebasing_base_takes_its_stack_out_and_brings_it_back() {
    let mut w = admitting(Admission::Label, 30, 20);
    let base = add(&mut w, true, 1);
    let child = add(&mut w, false, 2);
    stack(&mut w, child, base, false);
    w.observe().unwrap();
    w.eject(PrNumber(base), &conflict()).unwrap();
    let o = w.observe().unwrap();
    let with_base = Departed::Withdrawn(Withdrawal::Blocked { on: PrNumber(base) });
    assert_eq!(
        departures(&o),
        [(base, Departed::Ejected), (child, with_base)]
    );
    // A growth on the stack while it is out is out with it.
    w.fire(Event::Grow {
        on: PrNumber(child),
        size: 1,
        atomic: false,
    });
    let late = w.prs[&PrNumber(3)].base().expect("on the stack").0;
    assert_eq!(late, child);
    assert!(observed(&w.observe().unwrap()).is_empty());
    assert_eq!(w.out(), 3);
    let before = heads(&w);
    w.advance_to(20 * 60);
    let o = w.observe().unwrap();
    assert_eq!(
        bases(&o)
            .into_iter()
            .map(|(n, b, _)| (n, b))
            .collect::<Vec<_>>(),
        [
            (1, PrBase::Trunk),
            (2, PrBase::Pr(PrNumber(1))),
            (3, PrBase::Pr(PrNumber(late)))
        ]
    );
    assert!(heads(&w).iter().zip(&before).all(|(a, b)| a != b));
    assert!(o.prs.iter().all(|p| p.status == pending()));
    assert_eq!(w.metrics(End::drained(w.now())).rebases, 1);
    let trace = checked_lines(&mut w);
    let from = trace.iter().position(|l| l.starts_with("eject")).unwrap();
    assert_eq!(
        trace[from..],
        [
            "eject 1 unjustified: conflicts with trunk, rebasing".to_owned(),
            "rebase 1 conflict".to_owned(),
            "blocked 2 with 1".to_owned(),
            format!("arrive 3 on {late}"),
            format!("blocked 3 with {late}"),
            "repush 1".to_owned(),
            "repush 2".to_owned(),
            "repush 3".to_owned(),
            "admit 1".to_owned(),
            "admit 2 on 1".to_owned(),
            format!("admit 3 on {late}"),
        ]
    );
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn an_ejection_in_the_plan_that_took_a_pull_request_out_with_its_base_supersedes_it() {
    let mut w = admitting(Admission::Label, 0, 20);
    let base = add(&mut w, true, 1);
    let child = add(&mut w, false, 2);
    stack(&mut w, child, base, false);
    w.observe().unwrap();
    w.take_trace();
    w.eject(PrNumber(base), &conflict()).unwrap();
    let failed = EjectReason::BatchFailed { batch: BatchId(1) };
    w.eject(PrNumber(child), &failed).unwrap();
    let o = w.observe().unwrap();
    assert_eq!(
        departures(&o),
        [(base, Departed::Ejected), (child, Departed::Ejected)]
    );
    assert_eq!(
        lines(&mut w),
        [
            "eject 1 unjustified: conflicts with trunk, rebasing",
            "rebase 1 conflict",
            "eject 2 unjustified: batch 1 failed",
        ]
    );
    assert_eq!(
        (w.out(), w.metrics(End::drained(w.now())).ejected_good),
        (1, 1)
    );
}

#[test]
fn a_withdrawal_during_a_rebase_is_final_and_cancels_the_push() {
    let mut w = admitting(Admission::Label, 0, 20);
    let p = add(&mut w, true, 1);
    w.observe().unwrap();
    w.eject(PrNumber(p), &conflict()).unwrap();
    // Its author is mid-rebase: a churn push mints no head.
    let head = w.prs[&PrNumber(p)].head.clone();
    w.fire(Event::Repush(PrNumber(p)));
    assert_eq!(w.prs[&PrNumber(p)].head, head);
    w.fire(Event::Withdraw(PrNumber(p)));
    assert!(!w.events.values().any(|e| matches!(e, Event::Rebase(_))));
    finish(&mut w);
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.withdrawn, m.rebases, w.out()), (1, 0, 0));
    assert_eq!(
        checked_lines(&mut w)[2..],
        [
            "eject 1 unjustified: conflicts with trunk, rebasing",
            "rebase 1 conflict",
            "withdraw 1"
        ]
    );
}

#[test]
fn under_label_a_landed_base_changes_nothing_for_its_child() {
    let mut w = admitting(Admission::Label, 0, 20);
    w.config.pr_ci_on_base_landed = false;
    let base = add(&mut w, false, 1);
    let child = add(&mut w, false, 2);
    stack(&mut w, child, base, false);
    assert_eq!(verdict_of(&mut w, 1, &[base]), CiStatus::Passed);
    w.land(BatchId(1)).unwrap();
    finish(&mut w);
    let o = w.observe().unwrap();
    assert_eq!(bases(&o)[0].1, PrBase::Trunk);
    assert_eq!(status(&o, child), PrStatus::Ready);
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.rebases, m.billed_pr_ci_runs, w.out()), (0, 0, 0));
    assert_eq!(w.pr_ci_runs().len(), 2);
}

#[test]
fn under_github_a_stack_enters_one_landing_at_a_time() {
    let mut w = admitting(Admission::Github, 30, 20);
    for key in 1..=3 {
        add(&mut w, false, key);
    }
    stack(&mut w, 2, 1, false);
    stack(&mut w, 3, 2, false);
    let o = w.observe().unwrap();
    assert!(observed(&o).is_empty());
    assert_eq!(w.out(), 3);
    w.advance_to(30 * 60);
    let o = w.observe().unwrap();
    assert_eq!(observed(&o), [1]);
    assert_eq!(status(&o, 1), PrStatus::Ready);
    start(&mut w, 1, &o.trunk, &[1]).unwrap();
    w.advance_to(40 * 60);
    w.observe().unwrap();
    w.land(BatchId(1)).unwrap();
    let (two, three) = (
        w.prs[&PrNumber(2)].head.clone(),
        w.prs[&PrNumber(3)].head.clone(),
    );
    w.advance_to(60 * 60);
    // The push re-pushed the grandchild too.
    assert_ne!(w.prs[&PrNumber(2)].head, two);
    assert_ne!(w.prs[&PrNumber(3)].head, three);
    assert!(observed(&w.observe().unwrap()).is_empty());
    w.advance_to(90 * 60);
    let o = w.observe().unwrap();
    assert_eq!(bases(&o)[..], [(2, PrBase::Trunk, StackPolicy::Splittable)]);
    assert_eq!(
        checked_lines(&mut w),
        [
            "arrive 1",
            "arrive 2",
            "arrive 3",
            "admit 1",
            "wait 2 on 1",
            "wait 3 on 2",
            "start 1 [1] on trunk",
            "ci 1 passed",
            "land 1 [1]",
            "rebase 2 base landed",
            "repush 2",
            "repush 3",
            "admit 2",
            "wait 3 on 2",
        ]
    );
    // The rebased child's run is billed; the restack of the one above it
    // is not.
    let billed: Vec<(u64, u64, bool)> = w
        .pr_ci_runs()
        .iter()
        .map(|r| (r.pr.0, r.started_at / 60, r.billed))
        .collect();
    assert_eq!(
        billed,
        [
            (1, 0, false),
            (2, 0, false),
            (3, 0, false),
            (2, 60, true),
            (3, 60, false)
        ]
    );
    // 2 lands; 3 is rebased onto trunk, its run billed, and enters.
    start(&mut w, 2, &o.trunk, &[2]).unwrap();
    w.advance_to(100 * 60);
    w.observe().unwrap();
    w.land(BatchId(2)).unwrap();
    w.advance_to(120 * 60);
    w.advance_to(150 * 60);
    let o = w.observe().unwrap();
    assert_eq!(observed(&o), [3]);
    start(&mut w, 3, &o.trunk, &[3]).unwrap();
    w.advance_to(160 * 60);
    w.observe().unwrap();
    w.land(BatchId(3)).unwrap();
    let trace = w.take_trace();
    let billed_requests: Vec<String> = trace
        .iter()
        .map(ToString::to_string)
        .filter(|l| l.ends_with(" billed"))
        .collect();
    assert_eq!(billed_requests, ["pr-ci 3 billed"]);
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.landed, m.rebases, m.billed_pr_ci_runs), (3, 2, 2));
    assert_eq!(m.billed_pr_ci_hours, 1.0);
    assert_eq!(m.runner_hours, 1.5);
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn under_github_a_conflict_is_rebased_before_the_queue_sees_it() {
    let mut w = admitting(Admission::Github, 30, 20);
    let p = add(&mut w, true, 1);
    for t in [30, 50] {
        w.advance_to(t * 60);
        assert!(observed(&w.observe().unwrap()).is_empty(), "at {t} min");
    }
    w.advance_to(80 * 60);
    let o = w.observe().unwrap();
    assert_eq!(status(&o, p), PrStatus::Ready);
    assert_eq!(
        checked_lines(&mut w),
        ["arrive 1", "rebase 1 conflict", "repush 1", "admit 1"]
    );
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.rebases, m.billed_pr_ci_runs, m.ejected_bad), (1, 0, 0));
    assert_eq!(w.pr_ci_runs().len(), 2);
}

#[test]
fn under_github_a_push_takes_an_enqueued_pull_request_out_until_it_passes() {
    let mut w = admitting(Admission::Github, 30, 20);
    let p = add(&mut w, false, 1);
    w.advance_to(30 * 60);
    assert_eq!(observed(&w.observe().unwrap()), [p]);
    w.fire(Event::Repush(PrNumber(p)));
    let o = w.observe().unwrap();
    assert_eq!(
        departures(&o),
        [(p, Departed::Withdrawn(Withdrawal::IntentCleared))]
    );
    w.advance_to(60 * 60);
    let o = w.observe().unwrap();
    assert_eq!(status(&o, p), PrStatus::Ready);
    assert_eq!(
        checked_lines(&mut w),
        ["arrive 1", "admit 1", "repush 1", "recheck 1", "admit 1"]
    );
}

#[test]
fn under_github_a_base_that_leaves_blocks_a_child_the_queue_never_saw() {
    let mut w = admitting(Admission::Github, 30, 20);
    let base = add(&mut w, false, 1);
    let child = add(&mut w, false, 2);
    stack(&mut w, child, base, false);
    w.advance_to(30 * 60);
    assert_eq!(observed(&w.observe().unwrap()), [base]);
    w.eject(
        PrNumber(base),
        &EjectReason::BatchFailed { batch: BatchId(1) },
    )
    .unwrap();
    let o = w.observe().unwrap();
    assert_eq!(departures(&o), [(base, Departed::Ejected)]);
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.ejected_good, m.left_with_base, w.out()), (2, 1, 0));
    assert_eq!(
        checked_lines(&mut w)[3..],
        [
            "wait 2 on 1",
            "eject 1 unjustified: batch 1 failed",
            "blocked 2 on 1"
        ]
    );
}

#[test]
fn batch_runs_and_pr_ci_runs_all_start_when_requested() {
    let mut w = pr_ci_world(20);
    let prs: Vec<u64> = (1..=4).map(|k| add(&mut w, false, k)).collect();
    w.advance_to(20 * 60);
    let trunk = w.observe().unwrap().trunk;
    w.take_trace();
    for (id, &pr) in prs[..3].iter().enumerate() {
        start(&mut w, id as u64 + 1, &trunk, &[pr]).unwrap();
    }
    w.fire(Event::Repush(PrNumber(prs[3])));
    assert!(w.runs().iter().all(|r| r.started_at == 20 * 60));
    assert_eq!(w.pr_ci_runs().last().unwrap().started_at, 20 * 60);
    assert_eq!(w.check_invariants(), Ok(()));
    assert_eq!(
        lines(&mut w),
        [
            "start 1 [1] on trunk",
            "start 2 [2] on trunk",
            "start 3 [3] on trunk",
            "repush 4",
            "pr-ci 4"
        ]
    );
    finish(&mut w);
    assert!(w.runs().iter().all(|r| r.consumed == 600));
    assert_eq!(w.check_invariants(), Ok(()));
}

/// Under `admission`, with own CI of 30 minutes, pull request 2 stacked on
/// 1, both checked and in the queue by 30 minutes; under `label` 2 is
/// enqueued, under `github` it waits for 1.
fn base_and_child(admission: Admission) -> World {
    let mut w = admitting(admission, 30, 20);
    add(&mut w, false, 1);
    add(&mut w, false, 2);
    stack(&mut w, 2, 1, false);
    w.advance_to(30 * 60);
    w
}

#[test]
fn under_label_a_landed_base_bills_its_childs_re_run_on_the_same_head() {
    let mut w = base_and_child(Admission::Label);
    add(&mut w, false, 3);
    stack(&mut w, 3, 2, false);
    w.advance_to(60 * 60);
    let o = w.observe().unwrap();
    assert_eq!(observed(&o), [1, 2, 3]);
    assert!(o.prs.iter().all(|p| p.status == PrStatus::Ready));
    let (head, runs) = (o.prs[1].head.clone(), w.pr_ci_runs().len());
    start(&mut w, 1, &o.trunk, &[1]).unwrap();
    w.advance_to(70 * 60);
    w.observe().unwrap();
    w.take_trace();
    w.land(BatchId(1)).unwrap();
    assert_eq!(lines(&mut w), ["land 1 [1]", "pr-ci 2 billed"]);
    let o = w.observe().unwrap();
    assert_eq!((status(&o, 2), &o.prs[0].head), (pending(), &head));
    // The grandchild is untouched: no new run, still ready.
    assert_eq!(
        (status(&o, 3), w.pr_ci_runs().len()),
        (PrStatus::Ready, runs + 1)
    );
    assert!(violation_of(start(&mut w, 2, &o.trunk, &[2])).contains("not ready"));
    w.advance_to(100 * 60);
    assert_eq!(status(&w.observe().unwrap(), 2), PrStatus::Ready);
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.billed_pr_ci_runs, m.billed_pr_ci_hours), (1, 0.5));
    assert_eq!(m.runner_hours, (10.0 + 30.0) / 60.0);
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn under_label_a_child_landing_during_its_billed_run_still_pays_for_all_of_it() {
    let mut w = base_and_child(Admission::Label);
    let o = w.observe().unwrap();
    start(&mut w, 1, &o.trunk, &[1]).unwrap();
    let o = w.observe().unwrap();
    start(&mut w, 2, &tip(&o, 1), &[2]).unwrap();
    w.advance_to(40 * 60);
    w.observe().unwrap();
    w.land(BatchId(1)).unwrap();
    let o = w.observe().unwrap();
    assert_eq!(status(&o, 2), pending());
    // The batch holding the child at that head still lands.
    w.land(BatchId(2)).unwrap();
    finish(&mut w);
    let trace = w.take_trace();
    assert_eq!(crate::trace::check_lives(&trace), Ok(()));
    let lines: Vec<String> = trace.iter().map(ToString::to_string).collect();
    let from = lines.iter().position(|l| l == "land 1 [1]").unwrap();
    assert_eq!(
        lines[from..],
        [
            "land 1 [1]",
            "pr-ci 2 billed",
            "land 2 [2]",
            "pr-ci 2 passed"
        ]
    );
    let last = w.pr_ci_runs().last().unwrap();
    assert_eq!((last.pr.0, last.consumed, last.billed), (2, 30 * 60, true));
    let m = w.metrics(End::drained(w.now()));
    assert_eq!(
        (m.landed, m.billed_pr_ci_runs, m.billed_pr_ci_hours),
        (2, 1, 0.5)
    );
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_re_push_during_a_billed_run_cancels_it_and_it_stays_billed() {
    let mut w = base_and_child(Admission::Label);
    let o = w.observe().unwrap();
    start(&mut w, 1, &o.trunk, &[1]).unwrap();
    w.advance_to(40 * 60);
    w.observe().unwrap();
    w.land(BatchId(1)).unwrap();
    w.advance_to(50 * 60);
    w.fire(Event::Repush(PrNumber(2)));
    finish(&mut w);
    let runs: Vec<(u64, bool, bool)> = w.pr_ci_runs()[2..]
        .iter()
        .map(|r| (r.consumed, r.cancelled, r.billed))
        .collect();
    assert_eq!(runs, [(600, true, true), (1800, false, false)]);
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.billed_pr_ci_runs, m.runner_hours), (1, 20.0 / 60.0));
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn under_github_with_no_re_run_a_landed_base_admits_its_children_as_they_are() {
    let mut w = base_and_child(Admission::Github);
    w.config.pr_ci_on_base_landed = false;
    add(&mut w, false, 3);
    stack(&mut w, 3, 1, false);
    let o = w.observe().unwrap();
    assert_eq!(observed(&o), [1]);
    start(&mut w, 1, &o.trunk, &[1]).unwrap();
    w.advance_to(40 * 60);
    w.observe().unwrap();
    w.land(BatchId(1)).unwrap();
    // 2 had passed and enters at once; 3 is still checking.
    let o = w.observe().unwrap();
    assert_eq!((observed(&o), status(&o, 2)), (vec![2], PrStatus::Ready));
    w.advance_to(60 * 60);
    assert_eq!(observed(&w.observe().unwrap()), [2, 3]);
    assert_eq!(
        checked_lines(&mut w),
        [
            "arrive 1",
            "arrive 2",
            "admit 1",
            "wait 2 on 1",
            "arrive 3",
            "start 1 [1] on trunk",
            "ci 1 passed",
            "land 1 [1]",
            "admit 2",
            "admit 3",
        ]
    );
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.rebases, m.billed_pr_ci_runs), (0, 0));
    assert_eq!(w.pr_ci_runs().len(), 3);
    assert_eq!(w.check_invariants(), Ok(()));
}

/// The pending fix pushes of `pr` in the event queue.
fn fixes(w: &World, pr: u64) -> usize {
    let pending = w.events.values();
    pending.filter(|&&e| e == Event::Fix(PrNumber(pr))).count()
}

fn fixing() -> PrStatus {
    PrStatus::Waiting(BTreeSet::from([Hold::OwnCiFailed]))
}

/// Whether each own-CI run of `pr`, in start order, was to fail.
fn own_verdicts(w: &World, pr: u64) -> Vec<bool> {
    let runs = w.pr_ci_runs().iter().filter(|r| r.pr == PrNumber(pr));
    runs.map(|r| r.failed).collect()
}

#[test]
fn under_label_a_broken_pull_request_s_re_push_fails_its_run_and_its_author_fixes_it() {
    let mut w = admitting(Admission::Label, 30, 40);
    let p = add(&mut w, false, 1);
    w.advance_to(30 * 60);
    break_now(&mut w, p);
    // A plain re-push clears nothing: its run fails.
    w.fire(Event::Repush(PrNumber(p)));
    w.advance_to(60 * 60);
    let o = w.observe().unwrap();
    assert_eq!((observed(&o), status(&o, p)), (vec![p], fixing()));
    assert_eq!(fixes(&w, p), 1);
    assert_eq!(w.check_invariants(), Ok(()));
    // A same-head re-run while the fix is pending fails too, and schedules
    // no second fix; a re-push waits for the fix.
    w.request_pr_ci(PrNumber(p), true);
    let o = w.observe().unwrap();
    let both = PrStatus::Waiting(BTreeSet::from([Hold::OwnCiPending, Hold::OwnCiFailed]));
    assert_eq!(status(&o, p), both);
    w.fire(Event::Repush(PrNumber(p)));
    w.advance_to(90 * 60);
    assert_eq!(fixes(&w, p), 1);
    assert_eq!(status(&w.observe().unwrap(), p), fixing());
    // The fix push, `rebase_minutes` after the failure, clears it.
    w.advance_to(100 * 60);
    let fixed = &w.prs[&PrNumber(p)];
    assert_eq!(
        (fixed.broke_at, fixed.last_fix_push, fixed.fix),
        (None, Some(100 * 60), None)
    );
    assert_eq!(status(&w.observe().unwrap(), p), pending());
    w.advance_to(130 * 60);
    assert_eq!(status(&w.observe().unwrap(), p), PrStatus::Ready);
    assert_eq!(own_verdicts(&w, p), [false, true, true, false]);
    assert_eq!(
        checked_lines(&mut w),
        [
            "arrive 1",
            "admit 1",
            "repush 1",
            "rebase 1 broken",
            "fixed 1",
            "repush 1"
        ]
    );
    assert_eq!(w.metrics(End::drained(w.now())).rebases, 1);
    // Fixed, it is good: ejecting it now is not justified.
    w.eject(PrNumber(p), &EjectReason::BatchFailed { batch: BatchId(9) })
        .unwrap();
    let left = w.prs[&PrNumber(p)].record.left.clone();
    assert!(matches!(
        left,
        Some((
            _,
            Exit::Ejected {
                bad_since: None,
                ..
            }
        ))
    ));
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn under_github_a_broken_pull_request_is_fixed_while_out_checking() {
    let mut w = admitting(Admission::Github, 30, 20);
    let p = add(&mut w, false, 1);
    w.advance_to(30 * 60);
    break_now(&mut w, p);
    w.fire(Event::Repush(PrNumber(p)));
    w.advance_to(60 * 60);
    assert_eq!(w.prs[&PrNumber(p)].out, Some(Out::Checking));
    assert_eq!(observed(&w.observe().unwrap()), Vec::<u64>::new());
    assert_eq!(w.check_invariants(), Ok(()));
    w.advance_to(110 * 60);
    assert_eq!(status(&w.observe().unwrap(), p), PrStatus::Ready);
    assert_eq!(
        checked_lines(&mut w),
        [
            "arrive 1",
            "admit 1",
            "repush 1",
            "recheck 1",
            "rebase 1 broken",
            "fixed 1",
            "repush 1",
            "admit 1"
        ]
    );
}

#[test]
fn a_fix_pending_on_a_base_and_its_child_pushes_each_once_per_fix() {
    let mut w = base_and_child(Admission::Label);
    break_now(&mut w, 1);
    break_now(&mut w, 2);
    w.fire(Event::Repush(PrNumber(1)));
    w.advance_to(60 * 60);
    assert_eq!((fixes(&w, 1), fixes(&w, 2)), (1, 1));
    // The base's fix re-heads the child and cancels the child's fix; the
    // child's own breakage is untouched, so its new head's run fails.
    w.advance_to(80 * 60);
    assert_eq!((fixes(&w, 1), fixes(&w, 2)), (0, 0));
    assert!(w.prs[&PrNumber(2)].broke_at.is_some());
    w.advance_to(110 * 60);
    assert_eq!(fixes(&w, 2), 1);
    finish(&mut w);
    assert_eq!(own_verdicts(&w, 1), [false, true, false]);
    assert_eq!(own_verdicts(&w, 2), [false, true, true, false]);
    let rebases: Vec<u32> = w.pr_records().iter().map(|r| r.rebases).collect();
    assert_eq!(rebases, [1, 1]);
    assert_eq!(
        checked_lines(&mut w),
        [
            "arrive 1",
            "admit 1",
            "arrive 2",
            "admit 2",
            "repush 1",
            "repush 2",
            "rebase 1 broken",
            "rebase 2 broken",
            "fixed 1",
            "repush 1",
            "repush 2",
            "rebase 2 broken",
            "fixed 2",
            "repush 2",
        ]
    );
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_child_of_a_broken_base_passes_its_own_ci_and_is_good_after_the_base_s_fix() {
    let mut w = base_and_child(Admission::Label);
    break_now(&mut w, 1);
    w.fire(Event::Repush(PrNumber(1)));
    w.advance_to(60 * 60);
    assert_eq!(own_verdicts(&w, 2), [false, false]);
    assert_eq!(fixes(&w, 2), 0);
    assert_eq!(w.bad_since(PrNumber(2)), Some(30 * 60));
    w.advance_to(80 * 60);
    assert_eq!(w.bad_since(PrNumber(2)), None);
    finish(&mut w);
    let o = w.observe().unwrap();
    assert_eq!(
        (status(&o, 1), status(&o, 2)),
        (PrStatus::Ready, PrStatus::Ready)
    );
}

#[test]
fn under_label_a_conflict_ejection_cancels_a_pending_fix_and_its_push_fails_again() {
    let mut w = admitting(Admission::Label, 30, 20);
    let p = add(&mut w, true, 1);
    w.advance_to(30 * 60);
    break_now(&mut w, p);
    w.fire(Event::Repush(PrNumber(p)));
    w.advance_to(60 * 60);
    // The conflict is reported over the hold.
    let o = w.observe().unwrap();
    assert_eq!(status(&o, p), PrStatus::Conflicting(ConflictWith::Trunk));
    assert_eq!(fixes(&w, p), 1);
    w.advance_to(70 * 60);
    w.eject(PrNumber(p), &conflict()).unwrap();
    assert_eq!((fixes(&w, p), w.prs[&PrNumber(p)].fix), (0, None));
    // The conflict push at 90 clears the conflict, not the breakage.
    w.advance_to(120 * 60);
    assert_eq!(fixes(&w, p), 1);
    finish(&mut w);
    assert_eq!(own_verdicts(&w, p), [false, true, true, false]);
    assert_eq!(status(&w.observe().unwrap(), p), PrStatus::Ready);
    assert_eq!(
        checked_lines(&mut w),
        [
            "arrive 1",
            "admit 1",
            "repush 1",
            "rebase 1 broken",
            "eject 1 justified: conflicts with trunk, rebasing",
            "rebase 1 conflict",
            "repush 1",
            "admit 1",
            "rebase 1 broken",
            "fixed 1",
            "repush 1",
        ]
    );
    assert_eq!(w.pr_records()[0].rebases, 2);
}

#[test]
fn under_label_a_base_going_out_cancels_its_child_s_fix() {
    let mut w = base_and_child(Admission::Label);
    break_now(&mut w, 2);
    w.fire(Event::Repush(PrNumber(2)));
    w.advance_to(60 * 60);
    assert_eq!(fixes(&w, 2), 1);
    w.advance_to(70 * 60);
    w.prs.get_mut(&PrNumber(1)).unwrap().conflicts_with_trunk = true;
    w.observe().unwrap();
    w.eject(PrNumber(1), &conflict()).unwrap();
    assert_eq!(fixes(&w, 2), 0);
    // The base's push at 90 re-heads the child; its run fails at 120.
    w.advance_to(120 * 60);
    assert_eq!(fixes(&w, 2), 1);
    finish(&mut w);
    assert_eq!(own_verdicts(&w, 2), [false, true, true, false]);
    let lines = checked_lines(&mut w);
    assert!(
        lines.contains(&"blocked 2 with 1 bad".to_owned()),
        "{lines:?}"
    );
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn leaving_cancels_a_pending_fix() {
    let mut w = admitting(Admission::Label, 30, 20);
    let p = add(&mut w, false, 1);
    w.advance_to(30 * 60);
    break_now(&mut w, p);
    w.fire(Event::Repush(PrNumber(p)));
    w.advance_to(60 * 60);
    assert_eq!(fixes(&w, p), 1);
    w.fire(Event::Withdraw(PrNumber(p)));
    assert_eq!((fixes(&w, p), w.prs[&PrNumber(p)].fix), (0, None));
    assert_eq!(crate::trace::check_lives(&w.take_trace()), Ok(()));
}

#[test]
fn a_failed_run_of_a_pull_request_out_for_a_rebase_starts_no_fix() {
    let mut w = admitting(Admission::Label, 30, 30);
    let p = add(&mut w, true, 1);
    w.advance_to(30 * 60);
    break_now(&mut w, p);
    w.fire(Event::Repush(PrNumber(p)));
    w.advance_to(40 * 60);
    w.observe().unwrap();
    w.eject(PrNumber(p), &conflict()).unwrap();
    // The failed run ends at 60 while the author is rebasing.
    w.advance_to(60 * 60);
    assert_eq!(own_verdicts(&w, p), [false, true]);
    assert_eq!(fixes(&w, p), 0);
    // The rebase push at 70 runs a new head that fails at 100.
    w.advance_to(100 * 60);
    assert_eq!(fixes(&w, p), 1);
    assert_eq!(crate::trace::check_lives(&w.take_trace()), Ok(()));
}

#[test]
fn under_label_a_landed_base_s_re_run_of_a_broken_child_fails_and_starts_its_fix() {
    let mut w = base_and_child(Admission::Label);
    break_now(&mut w, 2);
    land_now(&mut w, 1, &[1]);
    w.advance_to(60 * 60);
    assert_eq!(own_verdicts(&w, 2), [false, true]);
    assert_eq!(status(&w.observe().unwrap(), 2), fixing());
    finish(&mut w);
    assert_eq!(status(&w.observe().unwrap(), 2), PrStatus::Ready);
    assert_eq!(w.metrics(End::drained(w.now())).billed_pr_ci_runs, 1);
    assert_eq!(crate::trace::check_lives(&w.take_trace()), Ok(()));
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn under_github_a_landed_base_cancels_its_child_s_fix_and_the_rebase_push_fails_again() {
    let mut w = base_and_child(Admission::Github);
    assert_eq!(w.prs[&PrNumber(2)].out, Some(Out::Stacked));
    break_now(&mut w, 2);
    w.fire(Event::Repush(PrNumber(2)));
    w.advance_to(60 * 60);
    assert_eq!(
        (fixes(&w, 2), w.prs[&PrNumber(2)].out),
        (1, Some(Out::Checking))
    );
    w.advance_to(70 * 60);
    land_now(&mut w, 1, &[1]);
    assert_eq!((fixes(&w, 2), w.prs[&PrNumber(2)].fix), (0, None));
    // The landed-base push at 90 runs a head that fails at 120.
    w.advance_to(120 * 60);
    assert_eq!(fixes(&w, 2), 1);
    finish(&mut w);
    assert_eq!(own_verdicts(&w, 2), [false, true, true, false]);
    assert_eq!(observed(&w.observe().unwrap()), [2]);
    assert_eq!(w.pr_records()[1].rebases, 2);
    let lines = checked_lines(&mut w);
    let tail: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .skip_while(|l| !l.starts_with("repush 2"))
        .collect();
    assert_eq!(
        tail,
        [
            "repush 2",
            "rebase 2 broken",
            "start 1 [1] on trunk",
            "ci 1 passed",
            "land 1 [1]",
            "rebase 2 base landed",
            "repush 2",
            "rebase 2 broken",
            "fixed 2",
            "repush 2",
            "admit 2",
        ]
    );
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn under_github_an_enqueued_pull_request_with_a_fix_pending_breaks_check_places() {
    let mut w = admitting(Admission::Github, 30, 20);
    let p = add(&mut w, false, 1);
    w.advance_to(30 * 60);
    assert_eq!(w.check_invariants(), Ok(()));
    w.prs.get_mut(&PrNumber(p)).unwrap().fix = Some((u64::MAX, 0));
    let message = w.check_invariants().unwrap_err();
    assert!(message.contains("pr 1 is enqueued unchecked"), "{message}");
}

fn behind(o: &Observation, pr: u64) -> u32 {
    let p = o.prs.iter().find(|p| p.number == PrNumber(pr));
    p.expect("observed").behind
}

#[test]
fn behind_counts_the_landings_since_each_own_ci_run_started() {
    let mut w = pr_ci_world(20);
    let p = add(&mut w, false, 1);
    let q = add(&mut w, false, 2);
    let child = add(&mut w, false, 3);
    let d = add(&mut w, false, 4);
    stack(&mut w, child, p, false);
    w.advance_to(1200);
    let o = w.observe().unwrap();
    assert!(o.prs.iter().all(|p| p.behind == 0));
    land_now(&mut w, 1, &[q]);
    let o = w.observe().unwrap();
    assert_eq!([behind(&o, p), behind(&o, child), behind(&o, d)], [1, 1, 1]);
    // A push's run starts against today's trunk.
    w.fire(Event::Repush(PrNumber(d)));
    assert_eq!(behind(&w.observe().unwrap(), d), 0);
    // The base's landing re-runs the child's own CI on its head.
    land_now(&mut w, 2, &[p]);
    let o = w.observe().unwrap();
    assert_eq!((behind(&o, child), behind(&o, d)), (0, 1));
    assert_eq!(status(&o, child), pending());
    assert_eq!(w.landed_count, 2);
    assert_eq!(w.check_invariants(), Ok(()));
    w.landed_count += 1;
    let message = w.check_invariants().unwrap_err();
    assert_eq!(message, "3 landings counted, 2 pull requests landed");
}

#[test]
fn the_trunk_position_is_the_count_of_landed_pull_requests() {
    let mut w = pr_ci_world(20);
    let p = add(&mut w, false, 1);
    let q = add(&mut w, false, 2);
    w.advance_to(1200);
    assert_eq!(w.observe().unwrap().trunk_position, 0);
    land_now(&mut w, 1, &[p, q]);
    let o = w.observe().unwrap();
    assert_eq!(o.trunk_position, 2);
    assert_eq!(w.landed_count, 2);
}

#[test]
fn under_github_a_pull_request_appears_behind_by_the_landings_during_its_run() {
    let mut w = admitting(Admission::Github, 30, 0);
    let q = add(&mut w, false, 1);
    w.advance_to(30 * 60);
    let p = add(&mut w, false, 2);
    assert_eq!(observed(&w.observe().unwrap()), [q]);
    land_now(&mut w, 1, &[q]);
    w.advance_to(60 * 60);
    let o = w.observe().unwrap();
    assert_eq!(observed(&o), [p]);
    assert_eq!(behind(&o, p), 1);
    assert_eq!(w.check_invariants(), Ok(()));
}

/// [`world`] at `p_miss = 1`, every repair `trunk_fix` minutes after its
/// break, with only the first two of its pull requests conflicting.
fn missing(trunk_fix: u64) -> World {
    let mut cfg = config(0.5, 0.0);
    cfg.p_miss = 1.0;
    cfg.trunk_fix_minutes = Minutes {
        min: trunk_fix,
        max: trunk_fix,
    };
    World::new(cfg, Admission::Label, Streams::for_replication(1, 0)).traced()
}

/// In [`missing`], lands `a`, breaking `b`, then lands `b` through a run
/// that misses it: trunk is broken from then. Returns the three numbers;
/// the events up to the second landing are appended to `kept`.
fn land_a_bad_pull_request(w: &mut World, kept: &mut Vec<TraceEvent>) -> (u64, u64, u64) {
    let (a, b, c) = only_first_two_conflict();
    let (pa, pb, pc) = (add(w, false, a), add(w, false, b), add(w, false, c));
    land_now(w, 1, &[pa]);
    assert!(w.bad_since(PrNumber(pb)).is_some());
    kept.extend(w.take_trace());
    land_now(w, 2, &[pb]);
    (pa, pb, pc)
}

#[test]
fn a_miss_passes_a_run_over_a_broken_tree_and_its_landing_breaks_trunk() {
    let mut w = missing(60);
    let mut kept = Vec::new();
    let (_, pb, pc) = land_a_bad_pull_request(&mut w, &mut kept);
    let broken_at = w.now();
    assert_eq!(
        kept_queue_lines(&mut w, &mut kept),
        [
            "start 2 [2] on trunk",
            "ci 2 passed broken",
            "land 2 [2]",
            "trunk broken by 2"
        ]
    );
    assert_eq!(exit_of(&w, pb), Exit::Landed { bad: true });
    assert_eq!(w.check_invariants(), Ok(()));
    // Caught from here: every run on trunk fails, a probe included.
    w.config.p_miss = 0.0;
    let trunk = w.observe().unwrap().trunk;
    start(&mut w, 3, &trunk, &[pc]).unwrap();
    start(&mut w, 4, &trunk, &[]).unwrap();
    // A new arrival's own CI fails for trunk; its author waits for the
    // repair and re-runs it on the same head.
    let pd = add(&mut w, false, 99);
    let head = w.prs[&PrNumber(pd)].head.clone();
    assert_eq!(status(&w.observe().unwrap(), pd), fixing());
    let repair = broken_at + HOUR as u64;
    let fix = w.prs[&PrNumber(pd)].fix.expect("a pending re-run");
    assert_eq!(fix.0, repair);
    assert_eq!(w.check_invariants(), Ok(()));
    w.advance_to(repair - 1);
    assert!(matches!(ci(&mut w, 3), CiStatus::Failed(_)));
    assert!(matches!(ci(&mut w, 4), CiStatus::Failed(_)));
    w.advance_to(repair);
    assert_eq!(w.prs[&PrNumber(pd)].head, head);
    assert_eq!(status(&w.observe().unwrap(), pd), PrStatus::Ready);
    assert_eq!(
        kept_lines(&mut w, &mut kept),
        [
            "start 3 [3] on trunk",
            "start 4 [] on trunk",
            "arrive 4",
            "pr-ci 4",
            "admit 4",
            "pr-ci 4 failed",
            "re-run 4",
            "ci 3 failed broken",
            "ci 4 failed broken",
            "trunk repaired",
            "pr-ci 4",
            "pr-ci 4 passed"
        ]
    );
    // Repaired in place: trunk does not move, and a run on it passes.
    assert_eq!(w.observe().unwrap().trunk, trunk);
    assert_eq!(verdict_of(&mut w, 5, &[pc]), CiStatus::Passed);
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.landed_bad, m.trunk_broken_hours), (1, 1.0));
    assert_eq!(m.rebases, 0);
    assert_eq!(w.check_invariants(), Ok(()));
    kept.extend(w.take_trace());
    assert_eq!(crate::trace::check_lives(&kept), Ok(()));
}

#[test]
fn own_ci_over_a_broken_trunk_misses_at_the_same_rate() {
    let mut w = missing(60);
    land_a_bad_pull_request(&mut w, &mut Vec::new());
    let pd = add(&mut w, false, 99);
    assert_eq!(own_verdicts(&w, pd), [false]);
    assert_eq!(status(&w.observe().unwrap(), pd), PrStatus::Ready);
    w.take_trace();
    land_now(&mut w, 3, &[pd]);
    assert_eq!(exit_of(&w, pd), Exit::Landed { bad: false });
    assert_eq!(w.breaks.len(), 1);
    assert_eq!(
        queue_lines(&mut w),
        ["start 3 [4] on trunk", "ci 3 passed broken", "land 3 [4]"]
    );
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_broken_pull_request_s_own_ci_misses_too() {
    let mut w = missing(60);
    let p = add(&mut w, false, 1);
    break_now(&mut w, p);
    w.fire(Event::Repush(PrNumber(p)));
    settle(&mut w);
    assert_eq!(own_verdicts(&w, p), [false, false]);
    assert_eq!(status(&w.observe().unwrap(), p), PrStatus::Ready);
}

#[test]
fn a_pull_request_landed_beside_one_it_conflicts_with_is_a_bad_landing() {
    let mut w = missing(60);
    let (a, b, _) = only_first_two_conflict();
    let (pa, pb) = (add(&mut w, false, a), add(&mut w, false, b));
    land_now(&mut w, 1, &[pa, pb]);
    let both = [exit_of(&w, pa), exit_of(&w, pb)];
    assert_eq!(
        both,
        [Exit::Landed { bad: true }, Exit::Landed { bad: true }]
    );
    assert_eq!(w.breaks.len(), 1);
    assert_eq!(w.check_invariants(), Ok(()));
    finish(&mut w);
    let m = w.metrics(End::drained(w.now()));
    assert_eq!(
        (m.ever_bad, m.landed_bad, m.trunk_broken_hours),
        (2, 2, 1.0)
    );
}

#[test]
fn a_passing_run_on_the_same_head_is_the_pending_re_run() {
    let mut cfg = config(0.0, 0.0);
    cfg.pr_ci_minutes = Minutes { min: 10, max: 10 };
    cfg.rebase_minutes = Minutes { min: 120, max: 120 };
    cfg.outages = vec![Outage {
        start_hours: 1,
        hours: 1,
    }];
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(1, 0)).traced();
    let mut kept = Vec::new();
    let (pp, pc) = (add(&mut w, false, 1), add(&mut w, false, 2));
    stack(&mut w, pc, pp, false);
    // `c`'s re-pushed head fails its run into the window; the re-run is due
    // at 185 minutes.
    w.advance_to(55 * 60);
    w.fire(Event::Repush(PrNumber(pc)));
    w.advance_to(65 * 60);
    assert_eq!(status(&w.observe().unwrap(), pc), fixing());
    // `p` lands after the window: `c`'s run on its retarget is the re-run.
    w.advance_to(121 * 60);
    land_now(&mut w, 1, &[pp]);
    assert_eq!(w.prs[&PrNumber(pc)].fix, None);
    w.advance_to(131 * 60);
    assert_eq!(status(&w.observe().unwrap(), pc), PrStatus::Ready);
    kept_lines(&mut w, &mut kept);
    finish(&mut w);
    let after = kept_lines(&mut w, &mut kept);
    assert!(after.iter().all(|l| !l.starts_with("pr-ci ")));
    assert_eq!(own_verdicts(&w, pc), [false, true, false]);
    assert_eq!(w.check_invariants(), Ok(()));
    assert_eq!(crate::trace::check_lives(&kept), Ok(()));
}

#[test]
fn a_re_run_waits_for_the_last_of_two_repairs() {
    let mut w = missing(60);
    let (_, _, pc) = land_a_bad_pull_request(&mut w, &mut Vec::new());
    w.advance_to(w.now() + 30 * 60);
    // `c` conflicts with nothing: landed through the broken trunk, it
    // breaks nothing new.
    land_now(&mut w, 3, &[pc]);
    assert_eq!(w.breaks.len(), 1);
    w.config.trunk_fix_minutes = Minutes { min: 90, max: 90 };
    // A conflicting pair arriving now: landing both is a second break,
    // repaired after the first.
    let (a, b, _) = only_first_two_conflict();
    let (e, f) = (add(&mut w, false, a), add(&mut w, false, b));
    land_now(&mut w, 4, &[e, f]);
    assert_eq!(w.breaks.len(), 2);
    let second = w.now() + 90 * 60;
    w.config.p_miss = 0.0;
    let pg = add(&mut w, false, 1003);
    assert_eq!(w.prs[&PrNumber(pg)].fix.expect("pending").0, second);
    w.advance_to(second - 1);
    assert_eq!(own_verdicts(&w, pg), [true]);
    w.advance_to(second);
    assert_eq!(own_verdicts(&w, pg), [true, false]);
    let m = w.metrics(End::drained(w.now()));
    assert_eq!(m.trunk_broken_hours, 2.0);
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn an_open_break_without_its_repair_scheduled_breaks_the_invariant() {
    let mut w = missing(60);
    land_a_bad_pull_request(&mut w, &mut Vec::new());
    assert_eq!(w.check_invariants(), Ok(()));
    let (_, repair) = w.breaks[&BatchId(2)];
    w.events.remove(&repair);
    let message = w.check_invariants().unwrap_err();
    assert!(message.contains("batch 2's break"), "{message}");
}

#[test]
fn an_outage_fails_every_run_it_touches_and_own_ci_is_re_run_when_it_ends() {
    // Own CI and its author's response take no time: without the wait for
    // the window's end, a re-run would fail and re-run at one instant
    // forever.
    let mut cfg = config(0.0, 0.0);
    cfg.outages = vec![Outage {
        start_hours: 1,
        hours: 1,
    }];
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(1, 0)).traced();
    let mut kept = Vec::new();
    let (pa, pb, pc, pd) = (
        add(&mut w, false, 1),
        add(&mut w, false, 2),
        add(&mut w, false, 3),
        add(&mut w, false, 4),
    );
    let trunk = w.observe().unwrap().trunk;
    kept_lines(&mut w, &mut kept);
    // Ends before the window.
    start(&mut w, 1, &trunk, &[pa]).unwrap();
    // In flight when CI goes down.
    w.advance_to(55 * 60);
    start(&mut w, 2, &trunk, &[pb]).unwrap();
    // Own CI inside the window fails, and its author waits for the end.
    w.advance_to(90 * 60);
    let pe = add(&mut w, false, 5);
    let head = w.prs[&PrNumber(pe)].head.clone();
    let fix = w.prs[&PrNumber(pe)].fix.expect("a pending re-run");
    assert_eq!(fix.0, 2 * HOUR as u64);
    assert_eq!(status(&w.observe().unwrap(), pe), fixing());
    assert_eq!(w.check_invariants(), Ok(()));
    // Starts inside the window and ends after it.
    w.advance_to(115 * 60);
    start(&mut w, 3, &trunk, &[pc]).unwrap();
    w.advance_to(2 * HOUR as u64);
    assert_eq!(w.prs[&PrNumber(pe)].head, head);
    assert_eq!(status(&w.observe().unwrap(), pe), PrStatus::Ready);
    // Starts at the window's end.
    w.advance_to(125 * 60);
    start(&mut w, 4, &trunk, &[pd]).unwrap();
    finish(&mut w);
    assert_eq!(
        kept_lines(&mut w, &mut kept),
        [
            "start 1 [1] on trunk",
            "ci 1 passed",
            "start 2 [2] on trunk",
            "ci 2 failed outage",
            "arrive 5",
            "pr-ci 5",
            "admit 5",
            "pr-ci 5 failed",
            "re-run 5",
            "start 3 [3] on trunk",
            "pr-ci 5",
            "pr-ci 5 passed",
            "ci 3 failed outage",
            "start 4 [4] on trunk",
            "ci 4 passed",
        ]
    );
    assert_eq!(own_verdicts(&w, pe), [true, false]);
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.rebases, m.landed_bad, m.trunk_broken_hours), (0, 0, 0.0));
    assert_eq!(w.check_invariants(), Ok(()));
    assert_eq!(crate::trace::check_lives(&kept), Ok(()));
}

#[test]
fn a_re_run_waits_for_the_later_of_an_outage_and_a_repair() {
    let mut w = missing(60);
    land_a_bad_pull_request(&mut w, &mut Vec::new());
    let down = w.now() + 2 * HOUR as u64;
    w.config.outages = vec![Outage {
        start_hours: 0,
        hours: down / HOUR as u64,
    }];
    w.config.p_miss = 0.0;
    let pd = add(&mut w, false, 99);
    assert_eq!(w.prs[&PrNumber(pd)].fix.expect("pending").0, down);
    w.advance_to(down - 1);
    assert_eq!(lines(&mut w).last().unwrap(), "trunk repaired");
    assert_eq!(own_verdicts(&w, pd), [true]);
    w.advance_to(down);
    assert_eq!(own_verdicts(&w, pd), [true, false]);
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_re_run_that_a_landing_turns_into_a_fix_traces_its_rebase() {
    let mut cfg = config(0.5, 0.0);
    cfg.rebase_minutes = Minutes { min: 60, max: 60 };
    cfg.outages = vec![Outage {
        start_hours: 1,
        hours: 1,
    }];
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(1, 0)).traced();
    let mut kept = Vec::new();
    let (a, b, _) = only_first_two_conflict();
    let (pa, pb) = (add(&mut w, false, a), add(&mut w, false, b));
    // `b`'s re-pushed head fails its run into the window; its author will
    // re-run it at 150 minutes.
    w.advance_to(90 * 60);
    w.fire(Event::Repush(PrNumber(pb)));
    settle(&mut w);
    let tail = kept_lines(&mut w, &mut kept);
    assert_eq!(
        tail[tail.len() - 4..],
        ["repush 2", "pr-ci 2", "pr-ci 2 failed", "re-run 2"]
    );
    // `a` lands after the window and breaks `b` while it waits: the re-run
    // is a fix push when it fires.
    w.advance_to(130 * 60);
    land_now(&mut w, 1, &[pa]);
    w.advance_to(150 * 60);
    assert_eq!(
        kept_lines(&mut w, &mut kept),
        [
            "start 1 [1] on trunk",
            "ci 1 passed",
            "land 1 [1]",
            "broken 2 by 1",
            "rebase 2 broken",
            "fixed 2",
            "repush 2",
            "pr-ci 2",
            "pr-ci 2 passed",
        ]
    );
    assert_eq!(w.prs[&PrNumber(pb)].record.rebases, 1);
    assert_eq!(status(&w.observe().unwrap(), pb), PrStatus::Ready);
    assert_eq!(w.check_invariants(), Ok(()));
    assert_eq!(crate::trace::check_lives(&kept), Ok(()));
}

/// A world under `admission` as [`admitting`], whose authors act on every
/// notice that their pull request is probably broken.
fn heeding(admission: Admission, pr_ci: u64, rebase: u64) -> World {
    let mut w = admitting(admission, pr_ci, rebase);
    w.config.p_heed = 1.0;
    w
}

fn broken_notice() -> NoticeReason {
    NoticeReason::Broken { batch: BatchId(9) }
}

/// The pending answers to a notice of `pr` in the event queue.
fn heeds(w: &World, pr: u64) -> usize {
    let pending = w.events.values();
    pending.filter(|&&e| e == Event::Heed(PrNumber(pr))).count()
}

fn first_notice(w: &World, pr: u64) -> Option<FirstNotice> {
    w.prs[&PrNumber(pr)].record.notice
}

#[test]
fn a_heeded_notice_to_a_broken_pull_request_is_its_fix_push_when_the_author_acts() {
    for admission in [Admission::Label, Admission::Github] {
        let mut w = heeding(admission, 30, 20);
        let mut kept = Vec::new();
        let p = add(&mut w, false, 1);
        w.advance_to(30 * 60);
        break_now(&mut w, p);
        w.advance_to(40 * 60);
        w.observe().unwrap();
        w.notify(PrNumber(p), &broken_notice()).unwrap();
        assert_eq!(
            kept_queue_lines(&mut w, &mut kept),
            [
                "arrive 1",
                "admit 1",
                "notice 1 justified: probably broken against trunk (batch 9)"
            ]
        );
        let told = FirstNotice {
            t: 40 * 60,
            justified: true,
            waited: Some(10 * 60),
        };
        assert_eq!(first_notice(&w, p), Some(told));
        // Until the author pushes, it is where it was, with no fix pending.
        assert_eq!(status(&w.observe().unwrap(), p), PrStatus::Ready);
        assert_eq!((heeds(&w, p), w.prs[&PrNumber(p)].fix), (1, None));
        assert_eq!(w.check_invariants(), Ok(()));
        w.advance_to(60 * 60);
        let mut pushed = vec!["rebase 1 broken", "fixed 1", "repush 1"];
        if admission == Admission::Github {
            pushed.push("recheck 1");
            assert_eq!(w.prs[&PrNumber(p)].out, Some(Out::Checking));
        } else {
            assert_eq!(status(&w.observe().unwrap(), p), pending());
        }
        assert_eq!(kept_queue_lines(&mut w, &mut kept), pushed);
        let fixed = &w.prs[&PrNumber(p)];
        assert_eq!(
            (fixed.broke_at, fixed.last_fix_push, fixed.heed, fixed.fix),
            (None, Some(60 * 60), None, None)
        );
        assert_eq!(fixed.record.rebases, 1);
        assert_eq!(own_verdicts(&w, p), [false, false]);
        assert_eq!(first_notice(&w, p), Some(told));
        assert_eq!(w.check_invariants(), Ok(()));
        w.advance_to(90 * 60);
        assert_eq!(status(&w.observe().unwrap(), p), PrStatus::Ready);
        kept_lines(&mut w, &mut kept);
        assert_eq!(crate::trace::check_lives(&kept), Ok(()), "{admission:?}");
    }
}

#[test]
fn a_heeded_notice_to_a_good_pull_request_is_a_needless_push_and_it_lands_after() {
    for admission in [Admission::Label, Admission::Github] {
        let mut w = heeding(admission, 30, 20);
        let mut kept = Vec::new();
        let p = add(&mut w, false, 1);
        w.advance_to(30 * 60);
        w.observe().unwrap();
        w.notify(PrNumber(p), &broken_notice()).unwrap();
        let told = FirstNotice {
            t: 30 * 60,
            justified: false,
            waited: None,
        };
        assert_eq!(first_notice(&w, p), Some(told));
        kept_lines(&mut w, &mut kept);
        w.advance_to(50 * 60);
        let mut pushed = vec!["rebase 1 broken", "repush 1"];
        if admission == Admission::Github {
            pushed.push("recheck 1");
        }
        assert_eq!(kept_queue_lines(&mut w, &mut kept), pushed);
        assert_eq!(w.prs[&PrNumber(p)].record.rebases, 1);
        assert_eq!(w.check_invariants(), Ok(()));
        w.advance_to(80 * 60);
        land_now(&mut w, 1, &[p]);
        assert!(w.prs[&PrNumber(p)].landed());
        kept_lines(&mut w, &mut kept);
        assert_eq!(crate::trace::check_lives(&kept), Ok(()), "{admission:?}");
        let m = w.metrics(End::drained(w.now()));
        assert_eq!((m.notices_good, m.notice_p50_hours), (1, None));
    }
}

#[test]
fn under_label_a_told_pull_request_batched_before_its_heed_is_pushed_out_of_the_batch() {
    let mut w = heeding(Admission::Label, 30, 5);
    let p = add(&mut w, false, 1);
    w.advance_to(30 * 60);
    w.observe().unwrap();
    w.notify(PrNumber(p), &broken_notice()).unwrap();
    w.advance_to(31 * 60);
    let trunk = w.observe().unwrap().trunk;
    start(&mut w, 1, &trunk, &[p]).unwrap();
    // The heed at 35 re-heads it while its batch runs to 41.
    w.advance_to(41 * 60);
    w.observe().unwrap();
    w.land(BatchId(1)).unwrap();
    assert_eq!(
        checked_lines(&mut w),
        [
            "arrive 1",
            "admit 1",
            "notice 1 unjustified: probably broken against trunk (batch 9)",
            "start 1 [1] on trunk",
            "rebase 1 broken",
            "repush 1",
            "ci 1 passed",
            "refused 1: #1 is gone"
        ]
    );
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_told_pull_request_that_lands_before_its_heed_never_pushes() {
    let mut w = heeding(Admission::Label, 30, 20);
    let p = add(&mut w, false, 1);
    w.advance_to(30 * 60);
    w.observe().unwrap();
    w.notify(PrNumber(p), &broken_notice()).unwrap();
    assert_eq!(heeds(&w, p), 1);
    land_now(&mut w, 1, &[p]);
    assert_eq!((heeds(&w, p), w.prs[&PrNumber(p)].heed), (0, None));
    finish(&mut w);
    let lines = checked_lines(&mut w);
    assert!(!lines.iter().any(|l| l.starts_with("rebase")), "{lines:?}");
    assert_eq!(w.prs[&PrNumber(p)].record.rebases, 0);
}

#[test]
fn an_author_who_does_not_heed_is_only_told() {
    let mut w = admitting(Admission::Label, 30, 20);
    let p = add(&mut w, false, 1);
    w.advance_to(30 * 60);
    break_now(&mut w, p);
    w.observe().unwrap();
    w.notify(PrNumber(p), &broken_notice()).unwrap();
    assert_eq!((heeds(&w, p), fixes(&w, p)), (0, 0));
    let told = first_notice(&w, p).expect("told");
    assert_eq!((told.justified, told.waited), (true, Some(0)));
    finish(&mut w);
    assert_eq!(
        checked_lines(&mut w),
        [
            "arrive 1",
            "admit 1",
            "notice 1 justified: probably broken against trunk (batch 9)"
        ]
    );
    assert_eq!(w.prs[&PrNumber(p)].broke_at, Some(30 * 60));
}

#[test]
fn a_notice_that_it_fails_together_starts_nothing_and_is_judged_by_the_pair() {
    let (ka, kb, kc) = only_first_two_conflict();
    let mut w = world(0.5, 0.0);
    w.config.p_heed = 1.0;
    let a = add(&mut w, false, ka);
    let b = add(&mut w, false, kb);
    let c = add(&mut w, false, kc);
    w.observe().unwrap();
    let with_a = NoticeReason::InteractsWith {
        pr: PrNumber(a),
        batch: BatchId(9),
    };
    w.notify(PrNumber(b), &with_a).unwrap();
    w.notify(PrNumber(c), &with_a).unwrap();
    assert_eq!((heeds(&w, b), heeds(&w, c)), (0, 0));
    let judged = |pr| first_notice(&w, pr).map(|n| (n.justified, n.waited));
    assert_eq!(
        (judged(b), judged(c)),
        (Some((true, None)), Some((false, None)))
    );
    let lines = queue_lines(&mut w);
    assert_eq!(
        lines[lines.len() - 2..],
        [
            "notice 2 justified: probably fails together with #1 (batch 9)",
            "notice 3 unjustified: probably fails together with #1 (batch 9)"
        ]
    );
    let m = w.metrics(End::drained(w.now()));
    assert_eq!((m.notices_good, m.notice_p50_hours), (1, None));
}

#[test]
fn only_the_first_notice_is_kept_and_a_push_or_a_departure_cancels_the_heed() {
    let mut w = heeding(Admission::Label, 30, 20);
    let p = add(&mut w, false, 1);
    w.advance_to(30 * 60);
    w.observe().unwrap();
    w.notify(PrNumber(p), &broken_notice()).unwrap();
    let told = first_notice(&w, p);
    // Told again at the same head: traced, nothing new kept, no second
    // heed.
    w.advance_to(35 * 60);
    w.observe().unwrap();
    w.notify(PrNumber(p), &broken_notice()).unwrap();
    assert_eq!((first_notice(&w, p), heeds(&w, p)), (told, 1));
    // Someone else's push drops the heed and keeps the first notice.
    w.fire(Event::Repush(PrNumber(p)));
    assert_eq!((first_notice(&w, p), heeds(&w, p)), (told, 0));
    assert_eq!(w.prs[&PrNumber(p)].heed, None);
    w.observe().unwrap();
    w.notify(PrNumber(p), &broken_notice()).unwrap();
    assert_eq!(heeds(&w, p), 1);
    w.fire(Event::Withdraw(PrNumber(p)));
    assert_eq!((heeds(&w, p), w.prs[&PrNumber(p)].heed), (0, None));
    let lines = checked_lines(&mut w);
    let notices = lines.iter().filter(|l| l.starts_with("notice 1"));
    assert_eq!(notices.count(), 3);
    assert!(!lines.iter().any(|l| l.starts_with("rebase")), "{lines:?}");
}

#[test]
fn a_notice_while_its_author_s_fix_is_pending_schedules_no_heed() {
    let mut w = heeding(Admission::Label, 30, 40);
    let p = add(&mut w, false, 1);
    w.advance_to(30 * 60);
    break_now(&mut w, p);
    w.fire(Event::Repush(PrNumber(p)));
    w.advance_to(60 * 60);
    assert_eq!(status(&w.observe().unwrap(), p), fixing());
    let fix = w.prs[&PrNumber(p)].fix;
    assert!(fix.is_some());
    w.take_trace();
    w.notify(PrNumber(p), &broken_notice()).unwrap();
    assert_eq!(
        lines(&mut w),
        ["notice 1 justified: probably broken against trunk (batch 9)"]
    );
    assert_eq!((heeds(&w, p), w.prs[&PrNumber(p)].fix), (0, fix));
    // A heed firing while the fix is pending does nothing: the fix is the
    // author's push.
    w.fire(Event::Heed(PrNumber(p)));
    assert_eq!(lines(&mut w), Vec::<String>::new());
    assert_eq!(w.prs[&PrNumber(p)].record.rebases, 0);
    w.advance_to(100 * 60);
    assert_eq!(queue_lines(&mut w), ["fixed 1", "repush 1"]);
    let pr = &w.prs[&PrNumber(p)];
    assert_eq!((pr.record.rebases, pr.fix, pr.heed), (1, None, None));
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_heed_firing_while_its_author_is_rebasing_does_nothing() {
    let mut w = heeding(Admission::Label, 30, 20);
    let p = add(&mut w, true, 1);
    w.observe().unwrap();
    w.eject(PrNumber(p), &conflict()).unwrap();
    w.take_trace();
    w.fire(Event::Heed(PrNumber(p)));
    assert_eq!(lines(&mut w), Vec::<String>::new());
    let pr = &w.prs[&PrNumber(p)];
    assert!(matches!(pr.out, Some(Out::Rebasing { .. })));
    assert_eq!(pr.record.rebases, 0);
}
