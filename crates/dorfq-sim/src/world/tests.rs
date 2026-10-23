use super::*;
use crate::config::CiMinutes;

fn config(runners: usize, p_interact: f64, p_flake: f64) -> WorldConfig {
    WorldConfig {
        hours: 1,
        drain_hours: 1,
        arrivals_per_hour: 0.0,
        runners,
        ci_minutes: CiMinutes { min: 10, max: 10 },
        p_bad: 0.0,
        p_interact,
        p_flake,
        p_conflict: 0.0,
        repush_per_hour: 0.0,
        withdraw_per_hour: 0.0,
    }
}

fn world(runners: usize, p_interact: f64, p_flake: f64) -> World {
    World::new(
        config(runners, p_interact, p_flake),
        Streams::for_replication(1, 0),
    )
    .traced()
}

/// A pull request arriving now with the given truth.
fn add(w: &mut World, bad: bool, conflicts: bool, key: u64) -> u64 {
    w.fire(Event::Arrival);
    let number = *w.prs.last_key_value().expect("arrived").0;
    let pr = w.prs.get_mut(&number).expect("arrived");
    pr.bad = bad;
    pr.conflicts_with_trunk = conflicts;
    pr.key = key;
    number.0
}

fn start(w: &mut World, id: u64, base: &Sha, prs: &[u64]) -> Result<(), ForgeError> {
    w.start_batch(&BatchStart {
        id: BatchId(id),
        base: base.clone(),
        prs: prs.iter().map(|&n| PrNumber(n)).collect(),
    })
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

/// Keys `(a, b, c)` such that only `a` and `b` interact at `p = 0.5`.
fn only_first_two_interact() -> (u64, u64, u64) {
    let inter = |x, y| unit(mix(x, y)) < 0.5;
    let b = (2..).find(|&b| inter(1, b)).unwrap();
    let c = (2..).find(|&c| !inter(1, c) && !inter(b, c)).unwrap();
    (1, b, c)
}

#[test]
fn an_arrival_appears_with_passed_ci_and_its_drawn_mergeability() {
    let mut cfg = config(1, 0.0, 0.0);
    cfg.arrivals_per_hour = 100.0;
    cfg.p_conflict = 1.0;
    let mut w = World::new(cfg, Streams::for_replication(1, 0)).traced();
    let t = w.next_event().expect("an arrival is scheduled");
    w.advance_to(t);
    let o = w.observe().unwrap();
    assert_eq!(o.prs.len(), 1);
    assert_eq!(o.prs[0].head_ci, CiStatus::Passed);
    assert_eq!(o.prs[0].mergeable, Mergeable::Conflicting);
    assert_eq!(w.take_trace(), ["arrive 1"]);
}

#[test]
fn a_run_fails_for_a_bad_member_and_passes_without_one() {
    let mut w = world(1, 0.0, 0.0);
    let good = add(&mut w, false, false, 1);
    let bad = add(&mut w, true, false, 2);
    assert_eq!(verdict_of(&mut w, 1, &[good]), CiStatus::Passed);
    assert_eq!(verdict_of(&mut w, 2, &[good, bad]), CiStatus::Failed);
    let trace = ["arrive 1", "arrive 2", "ci 1 passed", "ci 2 failed"];
    assert_eq!(w.take_trace(), trace);
}

#[test]
fn a_run_fails_for_an_interacting_own_member_pair() {
    let (a, b, c) = only_first_two_interact();
    let mut w = world(1, 0.5, 0.0);
    let pa = add(&mut w, false, false, a);
    let pb = add(&mut w, false, false, b);
    let pc = add(&mut w, false, false, c);
    assert_eq!(verdict_of(&mut w, 1, &[pa, pc]), CiStatus::Passed);
    assert_eq!(verdict_of(&mut w, 2, &[pa, pb]), CiStatus::Failed);
}

#[test]
fn a_run_fails_for_a_member_interacting_with_a_landed_pull_request() {
    let mut w = world(1, 1.0, 0.0);
    let first = add(&mut w, false, false, 1);
    let second = add(&mut w, false, false, 2);
    assert_eq!(verdict_of(&mut w, 1, &[first]), CiStatus::Passed);
    w.land(BatchId(1)).unwrap();
    assert_eq!(verdict_of(&mut w, 2, &[second]), CiStatus::Failed);
}

#[test]
fn a_pair_whose_queue_times_did_not_overlap_does_not_fail_a_run() {
    let mut w = world(1, 1.0, 0.0);
    let first = add(&mut w, false, false, 1);
    assert_eq!(verdict_of(&mut w, 1, &[first]), CiStatus::Passed);
    w.land(BatchId(1)).unwrap();
    let later = add(&mut w, false, false, 2);
    assert_eq!(verdict_of(&mut w, 2, &[later]), CiStatus::Passed);
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_flake_fails_a_clean_run() {
    let mut w = world(1, 0.0, 1.0);
    let good = add(&mut w, false, false, 1);
    assert_eq!(verdict_of(&mut w, 1, &[good]), CiStatus::Failed);
}

#[test]
fn a_child_fails_with_its_parents_tree() {
    let (a, b, c) = only_first_two_interact();
    for parent in [vec![(true, a)], vec![(false, a), (false, b)]] {
        let mut w = world(2, 0.5, 0.0);
        let members: Vec<u64> = parent
            .iter()
            .map(|&(bad, key)| add(&mut w, bad, false, key))
            .collect();
        let child = add(&mut w, false, false, c);
        let o = w.observe().unwrap();
        start(&mut w, 1, &o.trunk, &members).unwrap();
        let unobserved = tip(&w.snapshot(), 1);
        start(&mut w, 2, &unobserved, &[child]).unwrap_err();
        // The child's base must be in the latest observation.
        let o = w.observe().unwrap();
        start(&mut w, 2, &tip(&o, 1), &[child]).unwrap();
        finish(&mut w);
        assert_eq!(ci(&mut w, 2), CiStatus::Failed, "parent {parent:?}");
    }
}

#[test]
fn the_runner_queue_holds_a_third_run_until_one_of_two_completes() {
    let mut w = world(2, 0.0, 0.0);
    let prs: Vec<u64> = (0..3).map(|k| add(&mut w, false, false, k)).collect();
    let trunk = w.observe().unwrap().trunk;
    for (id, &pr) in prs.iter().enumerate() {
        start(&mut w, id as u64 + 1, &trunk, &[pr]).unwrap();
    }
    assert_eq!((w.running, w.runs().len()), (2, 2));
    assert_eq!(w.waiting, [BatchId(3)]);
    w.advance_to(w.next_event().unwrap());
    assert_eq!(w.now(), 600);
    assert_eq!((w.running, w.runs().len()), (1, 3));
    assert_eq!(w.runs()[2].started_at, 600);
    finish(&mut w);
    assert_eq!(w.now(), 1200);
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn retiring_a_running_batch_frees_its_runner_and_charges_elapsed_time() {
    let mut w = world(1, 0.0, 0.0);
    let (p, q) = (add(&mut w, false, false, 1), add(&mut w, false, false, 2));
    let trunk = w.observe().unwrap().trunk;
    start(&mut w, 1, &trunk, &[p]).unwrap();
    start(&mut w, 2, &trunk, &[q]).unwrap();
    w.advance_to(240);
    w.observe().unwrap();
    w.retire(BatchId(1)).unwrap();
    assert_eq!(w.runs()[0].consumed, 240);
    assert_eq!(w.runs()[1].started_at, 240);
    assert_eq!(w.running, 1);
    w.retire(BatchId(2)).unwrap();
    assert_eq!((w.running, w.next_event()), (0, None));
    let m = w.metrics();
    assert_eq!(m.runner_hours, 240.0 / 3600.0);
    assert_eq!(m.wasted_fraction, Some(1.0));
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn retiring_a_waiting_batch_consumes_nothing() {
    let mut w = world(1, 0.0, 0.0);
    let (p, q) = (add(&mut w, false, false, 1), add(&mut w, false, false, 2));
    let trunk = w.observe().unwrap().trunk;
    start(&mut w, 1, &trunk, &[p]).unwrap();
    start(&mut w, 2, &trunk, &[q]).unwrap();
    w.observe().unwrap();
    w.retire(BatchId(2)).unwrap();
    assert!(w.waiting.is_empty());
    assert_eq!(w.runs().len(), 1);
}

#[test]
fn land_refuses_trunk_moved_and_member_gone_and_lands_a_chain_in_order() {
    let mut w = world(3, 0.0, 0.0);
    let prs: Vec<u64> = (0..4).map(|k| add(&mut w, false, false, k)).collect();
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
fn a_conflicting_member_makes_a_conflict_batch_with_no_run() {
    let mut w = world(1, 0.0, 0.0);
    let p = add(&mut w, false, false, 1);
    let q = add(&mut w, false, true, 2);
    let o = w.observe().unwrap();
    start(&mut w, 1, &o.trunk, &[p, q]).unwrap();
    let o = w.observe().unwrap();
    let with = ConflictWith::Trunk;
    assert_eq!(
        state(&o, 1),
        BatchState::Conflict {
            pr: PrNumber(q),
            with
        }
    );
    assert_eq!((w.runs().len(), w.next_event()), (0, None));
}

#[test]
fn every_violation_is_permanent_and_named() {
    let mut w = world(2, 0.0, 0.0);
    let (p, q) = (add(&mut w, false, false, 1), add(&mut w, false, false, 2));
    let o = w.observe().unwrap();
    let trunk = o.trunk.clone();
    start(&mut w, 1, &trunk, &[p]).unwrap();
    let cases: Vec<(Result<(), ForgeError>, &str)> = vec![
        (start(&mut w, 1, &trunk, &[q]), "already in use"),
        (start(&mut w, 2, &trunk, &[]), "has no members"),
        (start(&mut w, 3, &trunk, &[q, q]), "twice"),
        (start(&mut w, 4, &trunk, &[99]), "not known"),
        (start(&mut w, 5, &commit(999), &[q]), "neither trunk"),
        (w.land(BatchId(1)), "batch 1 is not known"),
        (w.retire(BatchId(7)), "retire: batch 7 is not known"),
        (
            w.eject(PrNumber(99), &EjectReason::ConflictsWithTrunk),
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
    let reason = EjectReason::ConflictsWithTrunk;
    assert!(violation_of(w.eject(PrNumber(p), &reason)).contains("this plan lands"));
}

#[test]
fn observations_are_sorted() {
    let mut w = world(2, 0.0, 0.0);
    let prs: Vec<u64> = (0..3).map(|k| add(&mut w, false, false, k)).collect();
    let trunk = w.observe().unwrap().trunk;
    start(&mut w, 9, &trunk, &[prs[2]]).unwrap();
    start(&mut w, 4, &trunk, &[prs[0]]).unwrap();
    let o = w.observe().unwrap();
    assert!(o.prs.windows(2).all(|p| p[0].number < p[1].number));
    assert!(o.batches.windows(2).all(|b| b[0].id < b[1].id));
}

#[test]
fn an_ejection_is_justified_only_by_the_truth() {
    let mut w = world(1, 0.0, 0.0);
    let good = add(&mut w, false, false, 1);
    let bad = add(&mut w, true, false, 2);
    w.observe().unwrap();
    let reason = EjectReason::ConflictsWithTrunk;
    w.eject(PrNumber(good), &reason).unwrap();
    w.eject(PrNumber(bad), &reason).unwrap();
    let m = w.metrics();
    assert_eq!((m.ejected_good, m.ejected_bad), (1, 1));
}

#[test]
fn churn_repushes_and_withdraws_on_their_clocks() {
    let mut cfg = config(1, 0.0, 0.0);
    cfg.repush_per_hour = 1.0;
    cfg.withdraw_per_hour = 0.5;
    let mut w = World::new(cfg, Streams::for_replication(1, 0)).traced();
    let p = add(&mut w, false, false, 1);
    let head = w.prs[&PrNumber(p)].head.clone();
    finish(&mut w);
    let trace = w.take_trace();
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
    let mut cfg = config(1, 0.0, 0.0);
    cfg.repush_per_hour = 1e-30;
    cfg.withdraw_per_hour = 1e-30;
    let mut w = World::new(cfg, Streams::for_replication(1, 0));
    add(&mut w, false, false, 1);
    assert_eq!(w.next_event(), None);
}

#[test]
fn arrivals_and_truths_do_not_depend_on_what_the_scheduler_does() {
    let mut cfg = config(1, 0.0, 0.0);
    cfg.arrivals_per_hour = 20.0;
    cfg.p_bad = 0.5;
    let truths = |busy: bool| {
        let mut w = World::new(cfg.clone(), Streams::for_replication(3, 1));
        while let Some(t) = w.next_event() {
            w.advance_to(t);
            let o = w.observe().unwrap();
            if busy && o.batches.is_empty() {
                start(&mut w, t + 1, &o.trunk, &[o.prs[0].number.0]).unwrap();
            }
        }
        let prs = w.prs.values();
        prs.map(|p| (p.record.arrived_at, p.bad, p.key))
            .collect::<Vec<_>>()
    };
    let idle = truths(false);
    assert!(idle.len() > 5);
    assert_eq!(idle, truths(true));
}
