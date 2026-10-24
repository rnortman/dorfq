use super::*;
use crate::config::{Admission, Minutes};

fn config(runners: usize, p_interact: f64, p_flake: f64) -> WorldConfig {
    WorldConfig {
        hours: 1,
        drain_hours: 1,
        arrivals_per_hour: 0.0,
        runners,
        ci_minutes: Minutes { min: 10, max: 10 },
        pr_ci_minutes: Minutes { min: 0, max: 0 },
        rebase_minutes: Minutes { min: 0, max: 0 },
        p_bad: 0.0,
        p_interact,
        p_flake,
        p_conflict: 0.0,
        repush_per_hour: 0.0,
        withdraw_per_hour: 0.0,
        p_stacked: 0.0,
        p_atomic: 0.0,
    }
}

fn world(runners: usize, p_interact: f64, p_flake: f64) -> World {
    World::new(
        config(runners, p_interact, p_flake),
        Admission::Label,
        Streams::for_replication(1, 0),
    )
    .traced()
}

/// A pull request arriving now with the given truth; its own CI runs
/// now, when it is instant and a runner is free.
fn add(w: &mut World, bad: bool, conflicts: bool, key: u64) -> u64 {
    w.fire(Event::Arrival);
    let number = *w.prs.last_key_value().expect("arrived").0;
    let pr = w.prs.get_mut(&number).expect("arrived");
    pr.bad = bad;
    pr.conflicts_with_trunk = conflicts;
    pr.key = key;
    settle(w);
    number.0
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

/// Keys `(a, b, c)` such that only `a` and `b` interact at `p = 0.5`.
fn only_first_two_interact() -> (u64, u64, u64) {
    let inter = |x, y| unit(mix(x, y)) < 0.5;
    let b = (2..).find(|&b| inter(1, b)).unwrap();
    let c = (2..).find(|&c| !inter(1, c) && !inter(b, c)).unwrap();
    (1, b, c)
}

#[test]
fn an_arrival_appears_on_trunk_with_its_drawn_mergeability() {
    let mut cfg = config(1, 0.0, 0.0);
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
        [
            "arrive 1",
            "pr-ci 1",
            "admit 1",
            "pr-ci 1 begins",
            "pr-ci 1 passed"
        ]
    );
}

#[test]
fn a_run_fails_for_a_bad_member_and_passes_without_one() {
    let mut w = world(1, 0.0, 0.0);
    let good = add(&mut w, false, false, 1);
    let bad = add(&mut w, true, false, 2);
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
        "ci 1 begins",
        "ci 1 passed",
        "start 2 [1,2] on trunk",
        "ci 2 begins",
        "ci 2 failed broken",
    ];
    assert_eq!(queue_lines(&mut w), trace);
}

#[test]
fn a_run_fails_for_an_interacting_own_member_pair() {
    let (a, b, c) = only_first_two_interact();
    let mut w = world(1, 0.5, 0.0);
    let pa = add(&mut w, false, false, a);
    let pb = add(&mut w, false, false, b);
    let pc = add(&mut w, false, false, c);
    assert_eq!(verdict_of(&mut w, 1, &[pa, pc]), CiStatus::Passed);
    assert!(matches!(
        verdict_of(&mut w, 2, &[pa, pb]),
        CiStatus::Failed(_)
    ));
}

#[test]
fn a_run_fails_for_a_member_interacting_with_a_landed_pull_request() {
    let mut w = world(1, 1.0, 0.0);
    let first = add(&mut w, false, false, 1);
    let second = add(&mut w, false, false, 2);
    assert_eq!(verdict_of(&mut w, 1, &[first]), CiStatus::Passed);
    w.land(BatchId(1)).unwrap();
    assert!(matches!(
        verdict_of(&mut w, 2, &[second]),
        CiStatus::Failed(_)
    ));
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
    assert!(matches!(
        verdict_of(&mut w, 1, &[good]),
        CiStatus::Failed(_)
    ));
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
    let (a, b, c) = only_first_two_interact();
    let mut w = world(4, 0.5, 0.0);
    let pa = add(&mut w, false, false, a);
    let pb = add(&mut w, false, false, b);
    let pc = add(&mut w, false, false, c);
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
    let mut w = world(1, 0.0, 0.0);
    add(&mut w, false, false, 1);
    assert_eq!(verdict_of(&mut w, 1, &[]), CiStatus::Passed);
    assert!(violation_of(w.land(BatchId(1))).contains("is a probe"));
    w.retire(BatchId(1)).unwrap();
    assert!(w.observe().unwrap().batches.is_empty());
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
    assert_eq!(w.waiting, [Job::Batch(BatchId(3))]);
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
fn departed_matches_each_exit_and_each_exit_is_traced() {
    let mut w = world(1, 0.0, 0.0);
    let prs: Vec<u64> = (0..3).map(|k| add(&mut w, false, false, k)).collect();
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
        trace[trace.len() - 3..],
        [
            "land 1 [1]",
            "eject 2 unjustified: batch 1 failed",
            "withdraw 3"
        ]
    );
    let o = w.observe().unwrap();
    let how = [
        Departed::Landed { batch: BatchId(1) },
        Departed::Ejected,
        Departed::Withdrawn(Withdrawal::IntentCleared),
    ];
    let expected: Vec<dorfq_core::Departure> = (0..3)
        .map(|i| dorfq_core::Departure::new(PrNumber(prs[i]), heads[i].clone(), how[i].clone()))
        .collect();
    assert_eq!(o.departed, expected);
    assert!(lines(&mut w).is_empty(), "an observation records nothing");
    assert!(w.observe().unwrap().departed.is_empty());
}

#[test]
fn a_pull_request_conflicting_with_trunk_cannot_join_a_batch() {
    let mut w = world(1, 0.0, 0.0);
    let p = add(&mut w, false, false, 1);
    let q = add(&mut w, false, true, 2);
    let o = w.observe().unwrap();
    assert_eq!(o.prs[0].status, PrStatus::Ready);
    let message = violation_of(start(&mut w, 1, &o.trunk, &[p, q]));
    assert!(message.contains("not ready"), "{message}");
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
    let reason = EjectReason::Conflicts {
        with: ConflictWith::Trunk,
    };
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
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(1, 0)).traced();
    let p = add(&mut w, false, false, 1);
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
    let mut cfg = config(1, 0.0, 0.0);
    cfg.repush_per_hour = 1e-30;
    cfg.withdraw_per_hour = 1e-30;
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(1, 0));
    add(&mut w, false, false, 1);
    assert_eq!(w.next_event(), None);
}

#[test]
fn arrivals_and_truths_do_not_depend_on_what_the_scheduler_does() {
    let mut cfg = config(1, 0.0, 0.0);
    cfg.arrivals_per_hour = 20.0;
    cfg.p_bad = 0.5;
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
        prs.map(|p| (p.record.arrived_at, p.bad, p.key))
            .collect::<Vec<_>>()
    };
    let idle = truths(false);
    assert!(idle.len() > 5);
    assert_eq!(idle, truths(true));
}

/// Makes `pr` stacked on `base`, cut from it, as if it arrived so.
fn stack(w: &mut World, pr: u64, base: u64, atomic: bool) {
    let p = w.prs.get_mut(&PrNumber(pr)).expect("arrived");
    p.cut_from = Some(PrNumber(base));
    p.stacked = true;
    p.atomic = atomic;
}

/// Pull requests 1 to 5: 2 on 1, 3 (`Atomic`) on 2, 4 on 1, 5 on trunk.
fn tree(w: &mut World) {
    for key in 1..=5 {
        add(w, false, false, key);
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

#[test]
fn a_stacked_arrival_is_reported_on_its_base_and_atomic_when_drawn_so() {
    let mut cfg = config(1, 0.0, 0.0);
    cfg.p_stacked = 1.0;
    cfg.p_atomic = 1.0;
    cfg.p_conflict = 1.0;
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(1, 0)).traced();
    for _ in 0..8 {
        w.fire(Event::Arrival);
    }
    settle(&mut w);
    let o = w.observe().unwrap();
    // The first arrival has nothing to be cut from: it is on trunk and
    // keeps its drawn conflict.
    assert_eq!(o.prs[0].base, PrBase::Trunk);
    assert_eq!(o.prs[0].status, PrStatus::Conflicting(ConflictWith::Trunk));
    let mut stacked = 0;
    for (p, s) in w.prs.values().zip(&o.prs) {
        match p.cut_from {
            Some(target) => {
                stacked += 1;
                assert_eq!(p.base(), Some(target));
                assert!(target < p.record.number);
                assert_eq!(
                    (s.base, s.stack, &s.status),
                    (PrBase::Pr(target), StackPolicy::Atomic, &PrStatus::Ready)
                );
            }
            None => {
                assert_eq!((s.base, s.stack), (PrBase::Trunk, StackPolicy::Splittable));
                assert_eq!(s.status, PrStatus::Conflicting(ConflictWith::Trunk));
            }
        }
    }
    assert!(stacked > 3, "{stacked} of 8 stacked");
    let trace = queue_lines(&mut w);
    let on = trace
        .iter()
        .filter(|l| l.contains(" on ") && l.ends_with(" atomic"));
    assert_eq!(on.count(), stacked, "{trace:?}");
}

#[test]
fn a_landed_base_leaves_its_children_reported_on_trunk() {
    let mut w = world(1, 0.0, 0.0);
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
    let mut w = world(1, 0.0, 0.0);
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
    let m = w.metrics();
    assert_eq!((m.withdrawn, m.blocked, m.unfinished), (1, 3, 1));
}

#[test]
fn an_ejection_mid_stack_blocks_only_what_is_above_it() {
    let mut w = world(1, 0.0, 0.0);
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
    let mut w = world(1, 0.0, 0.0);
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
    let m = w.metrics();
    assert_eq!((m.ejected_good, m.blocked), (2, 0));
}

#[test]
fn a_repush_restacks_every_descendant() {
    let mut w = world(1, 0.0, 0.0);
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
fn a_pull_request_never_interacts_with_its_ancestors() {
    let mut w = world(1, 1.0, 0.0);
    tree(&mut w);
    // Queue times overlap only once the clock has moved past the arrivals.
    w.advance_to(1);
    let pairs = |w: &World| -> Vec<(u64, u64)> {
        let numbers: Vec<PrNumber> = w.prs.keys().copied().collect();
        let mut pairs = Vec::new();
        for (k, &i) in numbers.iter().enumerate() {
            for &j in &numbers[k + 1..] {
                if w.interacts(i, j) {
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
fn a_landed_pull_request_whose_base_did_not_land_breaks_the_invariant() {
    let mut w = world(1, 0.0, 0.0);
    tree(&mut w);
    w.prs.get_mut(&PrNumber(2)).unwrap().record.left = Some((0, Exit::Landed));
    let message = w.check_invariants().unwrap_err();
    assert!(message.contains("base 1 did not land"), "{message}");
}

#[test]
fn a_world_without_stacks_draws_what_it_drew_before_stacks() {
    let mut cfg = config(1, 0.0, 0.0);
    cfg.arrivals_per_hour = 20.0;
    cfg.p_bad = 0.5;
    cfg.p_conflict = 0.5;
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(3, 1));
    finish(&mut w);
    let expected = [
        (67, false, true, 9386675400015353148, None),
        (167, false, true, 9595752625188343565, None),
        (215, true, true, 11222048117854481362, None),
        (263, false, false, 1566661914958349798, None),
        (487, false, false, 9259621691918785673, None),
        (567, true, true, 1945186956785583718, None),
    ];
    assert_eq!(w.truths()[..6], expected);
}

#[test]
fn a_cut_arrival_whose_target_left_is_on_trunk_and_changes_no_draw() {
    let mut cfg = config(1, 0.0, 0.0);
    cfg.p_stacked = 1.0;
    cfg.p_atomic = 1.0;
    cfg.p_conflict = 1.0;
    let arrivals = |withdraw_first: bool| {
        let mut w = World::new(
            cfg.clone(),
            Admission::Label,
            Streams::for_replication(2, 0),
        );
        w.fire(Event::Arrival);
        if withdraw_first {
            w.fire(Event::Withdraw(PrNumber(1)));
        }
        for _ in 0..7 {
            w.fire(Event::Arrival);
        }
        settle(&mut w);
        let o = w.observe().unwrap();
        (
            w.truths(),
            bases(&o),
            o.prs.iter().map(|p| p.status.clone()).collect::<Vec<_>>(),
        )
    };
    let (kept, kept_bases, _) = arrivals(false);
    let (left, left_bases, statuses) = arrivals(true);
    assert_eq!(kept, left);
    let cut_from_1: Vec<u64> = (0..kept.len())
        .filter(|&i| kept[i].4 == Some(PrNumber(1)))
        .map(|i| i as u64 + 1)
        .collect();
    assert!(!cut_from_1.is_empty(), "nothing cut from 1 at this seed");
    for n in cut_from_1 {
        let on_kept = kept_bases.iter().find(|b| b.0 == n).unwrap();
        assert_eq!(on_kept.1, PrBase::Pr(PrNumber(1)));
        let k = left_bases.iter().position(|b| b.0 == n).unwrap();
        assert_eq!(left_bases[k].1, PrBase::Trunk);
        assert_eq!(left_bases[k].2, StackPolicy::Splittable);
        assert_eq!(statuses[k], PrStatus::Ready);
    }
}

#[test]
fn a_start_names_the_batch_it_runs_on_unless_that_is_trunk() {
    let mut w = world(2, 0.0, 0.0);
    for key in 1..=4 {
        add(&mut w, false, false, key);
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
    let mut w = world(1, 0.0, 0.0);
    for key in 1..=3 {
        add(&mut w, false, key == 3, key);
    }
    let trunk = w.observe().unwrap().trunk;
    w.take_trace();
    // One runner: batch 2 waits behind batch 1 and is cancelled waiting.
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
            "ci 1 begins",
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
    let mut cfg = config(1, 0.0, 0.0);
    cfg.p_bad = 1.0;
    cfg.p_conflict = 1.0;
    cfg.p_stacked = 1.0;
    cfg.p_atomic = 0.5;
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(1, 0)).traced();
    for _ in 0..8 {
        w.fire(Event::Arrival);
    }
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
            truth: Truth {
                bad: true,
                conflicts: p.conflicts_with_trunk,
            },
        };
        assert_eq!(event, &expected);
    }
    // The first is on trunk with its drawn conflict; a cut arrival has none.
    assert!(matches!(
        arrivals[0],
        TraceEvent::Arrive {
            base: None,
            truth: Truth {
                conflicts: true,
                ..
            },
            ..
        }
    ));
    assert!(arrivals[1..].iter().all(|e| matches!(
        e,
        TraceEvent::Arrive {
            base: Some(_),
            truth: Truth {
                conflicts: false,
                ..
            },
            ..
        }
    )));
}

/// [`world`] whose pull requests' own CI takes `minutes` exactly.
fn pr_ci_world(runners: usize, minutes: u64) -> World {
    let mut cfg = config(runners, 0.0, 0.0);
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
fn an_arrival_runs_its_own_ci_on_a_runner_shared_with_batches() {
    let mut cfg = config(2, 0.0, 0.0);
    cfg.pr_ci_minutes = Minutes { min: 5, max: 25 };
    let mut w = World::new(cfg, Admission::Label, Streams::for_replication(1, 0)).traced();
    let first = add(&mut w, false, false, 1);
    let second = add(&mut w, false, false, 2);
    assert_eq!(w.running, 2);
    assert_eq!(
        lines(&mut w),
        [
            "arrive 1",
            "pr-ci 1",
            "admit 1",
            "pr-ci 1 begins",
            "arrive 2",
            "pr-ci 2",
            "admit 2",
            "pr-ci 2 begins"
        ]
    );
    finish(&mut w);
    let durations: Vec<u64> = w.pr_ci_runs().iter().map(|r| r.consumed).collect();
    assert_eq!(durations.len(), 2);
    for d in &durations {
        assert!((300..=1500).contains(d) && d % 60 == 0, "{durations:?}");
    }
    assert_eq!(w.check_invariants(), Ok(()));
    // A batch and a PR CI run share the two runners.
    let trunk = w.observe().unwrap().trunk;
    start(&mut w, 1, &trunk, &[first]).unwrap();
    w.fire(Event::Repush(PrNumber(second)));
    assert_eq!(w.running, 2);
    assert_eq!(w.check_invariants(), Ok(()));
    finish(&mut w);
    assert_eq!(w.check_invariants(), Ok(()));
    let m = w.metrics();
    assert_eq!(m.pr_ci_runs, 3);
    let pr_ci: u64 = w.pr_ci_runs().iter().map(|r| r.consumed).sum();
    assert_eq!(m.pr_ci_hours, pr_ci as f64 / HOUR);
    assert_eq!(m.runner_hours, (pr_ci + 600) as f64 / HOUR);
}

#[test]
fn a_pull_request_is_held_until_its_own_ci_passes() {
    let mut w = pr_ci_world(1, 20);
    let p = add(&mut w, false, false, 1);
    let q = add(&mut w, false, false, 2);
    stack(&mut w, q, p, false);
    let o = w.observe().unwrap();
    assert_eq!(
        (status(&o, p), o.prs[1].base),
        (pending(), PrBase::Pr(PrNumber(p)))
    );
    let message = violation_of(start(&mut w, 1, &o.trunk, &[p]));
    assert!(message.contains("not ready"), "{message}");
    // One runner: 2's run waits behind 1's.
    w.advance_to(1200);
    let o = w.observe().unwrap();
    assert_eq!((status(&o, p), status(&o, q)), (PrStatus::Ready, pending()));
    start(&mut w, 1, &o.trunk, &[p]).unwrap();
    w.advance_to(2400);
    let o = w.observe().unwrap();
    assert_eq!(status(&o, q), PrStatus::Ready);
    let starts = w.pr_ci_runs().iter().map(|r| (r.pr.0, r.started_at));
    assert_eq!(starts.collect::<Vec<_>>(), [(1, 0), (2, 1200)]);
}

#[test]
fn an_instant_own_ci_is_ready_at_once_but_still_waits_for_a_runner() {
    let mut w = pr_ci_world(1, 0);
    let p = add(&mut w, false, false, 1);
    let o = w.observe().unwrap();
    assert_eq!(status(&o, p), PrStatus::Ready);
    start(&mut w, 1, &o.trunk, &[p]).unwrap();
    // The batch holds the only runner: the next arrival's run waits.
    let q = add(&mut w, false, false, 2);
    let o = w.observe().unwrap();
    assert_eq!(status(&o, q), pending());
    w.advance_to(600);
    let o = w.observe().unwrap();
    assert_eq!(status(&o, q), PrStatus::Ready);
    assert_eq!(w.pr_ci_runs()[1].started_at, 600);
}

#[test]
fn a_repush_cancels_the_old_heads_run_and_holds_the_new_head() {
    let mut w = pr_ci_world(2, 20);
    let p = add(&mut w, false, false, 1);
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
            "pr-ci 1 begins",
            "ci 1 passed",
            "repush 1",
            "pr-ci 1 cancelled",
            "pr-ci 1",
            "pr-ci 1 begins"
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
    let m = w.metrics();
    assert_eq!(m.pr_ci_runs, 3);
    assert_eq!(m.pr_ci_hours, (1200 + 300 + 1200) as f64 / HOUR);
}

#[test]
fn a_repush_while_the_run_waits_drops_it_from_the_runner_queue() {
    let mut w = pr_ci_world(1, 20);
    let p = add(&mut w, false, false, 1);
    let q = add(&mut w, false, false, 2);
    w.take_trace();
    w.fire(Event::Repush(PrNumber(q)));
    assert_eq!(lines(&mut w), ["repush 2", "pr-ci 2 cancelled", "pr-ci 2"]);
    assert_eq!(w.waiting, [Job::PrCi(PrNumber(q))]);
    finish(&mut w);
    let runs: Vec<(u64, bool)> = w
        .pr_ci_runs()
        .iter()
        .map(|r| (r.pr.0, r.cancelled))
        .collect();
    assert_eq!(runs, [(p, false), (q, false)]);
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_pull_request_that_leaves_still_pays_for_its_run() {
    let mut w = pr_ci_world(1, 20);
    let p = add(&mut w, false, false, 1);
    let q = add(&mut w, false, false, 2);
    w.take_trace();
    w.fire(Event::Withdraw(PrNumber(p)));
    w.fire(Event::Withdraw(PrNumber(q)));
    finish(&mut w);
    assert_eq!(
        lines(&mut w),
        [
            "withdraw 1",
            "withdraw 2",
            "pr-ci 1 passed",
            "pr-ci 2 begins",
            "pr-ci 2 passed"
        ]
    );
    assert_eq!(w.metrics().pr_ci_hours, 2400.0 / HOUR);
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn a_pull_requests_own_ci_durations_do_not_depend_on_the_queue() {
    let mut cfg = config(1, 0.0, 0.0);
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

/// A world under `admission` on `runners` runners whose own CI runs take
/// `pr_ci` minutes and whose rebases take `rebase` minutes, exactly.
fn admitting(admission: Admission, runners: usize, pr_ci: u64, rebase: u64) -> World {
    let mut cfg = config(runners, 0.0, 0.0);
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
    let mut w = admitting(Admission::Label, 2, 30, 20);
    let p = add(&mut w, false, true, 1);
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
            "pr-ci 1 begins",
            "pr-ci 1 passed",
        ]
    );
    let m = w.metrics();
    assert_eq!(
        (m.rebases, m.ejected_bad, m.ejected_good, m.pr_ci_runs),
        (1, 0, 0, 2)
    );
    // The first head's run was cut at the push, 20 minutes in.
    assert_eq!(w.pr_ci_runs()[0].consumed, 20 * 60);
    assert!(w.pr_ci_runs()[0].cancelled);
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn under_label_an_instant_rebase_returns_at_the_next_observation() {
    let mut w = admitting(Admission::Label, 2, 0, 0);
    let p = add(&mut w, false, true, 1);
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
    let mut w = admitting(Admission::Label, 4, 30, 20);
    let base = add(&mut w, false, true, 1);
    let child = add(&mut w, false, false, 2);
    stack(&mut w, child, base, false);
    w.observe().unwrap();
    w.eject(PrNumber(base), &conflict()).unwrap();
    let o = w.observe().unwrap();
    let with_base = Departed::Withdrawn(Withdrawal::Blocked { on: PrNumber(base) });
    assert_eq!(
        departures(&o),
        [(base, Departed::Ejected), (child, with_base)]
    );
    // An arrival cut from the stack while it is out is out with it.
    w.config.p_stacked = 1.0;
    w.fire(Event::Arrival);
    let late = w.prs[&PrNumber(3)].base().expect("cut from the stack").0;
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
    assert_eq!(w.metrics().rebases, 1);
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
    let mut w = admitting(Admission::Label, 2, 0, 20);
    let base = add(&mut w, false, true, 1);
    let child = add(&mut w, false, false, 2);
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
    assert_eq!((w.out(), w.metrics().ejected_good), (1, 1));
}

#[test]
fn a_withdrawal_during_a_rebase_is_final_and_cancels_the_push() {
    let mut w = admitting(Admission::Label, 2, 0, 20);
    let p = add(&mut w, false, true, 1);
    w.observe().unwrap();
    w.eject(PrNumber(p), &conflict()).unwrap();
    // Its author is mid-rebase: a churn push mints no head.
    let head = w.prs[&PrNumber(p)].head.clone();
    w.fire(Event::Repush(PrNumber(p)));
    assert_eq!(w.prs[&PrNumber(p)].head, head);
    w.fire(Event::Withdraw(PrNumber(p)));
    assert!(!w.events.values().any(|e| matches!(e, Event::Rebase(_))));
    finish(&mut w);
    let m = w.metrics();
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
    let mut w = admitting(Admission::Label, 2, 0, 20);
    let base = add(&mut w, false, false, 1);
    let child = add(&mut w, false, false, 2);
    stack(&mut w, child, base, false);
    assert_eq!(verdict_of(&mut w, 1, &[base]), CiStatus::Passed);
    w.land(BatchId(1)).unwrap();
    finish(&mut w);
    let o = w.observe().unwrap();
    assert_eq!(bases(&o)[0].1, PrBase::Trunk);
    assert_eq!(status(&o, child), PrStatus::Ready);
    let m = w.metrics();
    assert_eq!((m.rebases, m.pr_ci_runs, w.out()), (0, 2, 0));
}

#[test]
fn under_github_a_stack_enters_one_landing_at_a_time() {
    let mut w = admitting(Admission::Github, 4, 30, 20);
    for key in 1..=3 {
        add(&mut w, false, false, key);
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
            "ci 1 begins",
            "ci 1 passed",
            "land 1 [1]",
            "rebase 2 base landed",
            "repush 2",
            "repush 3",
            "admit 2",
            "wait 3 on 2",
        ]
    );
    let m = w.metrics();
    assert_eq!((m.rebases, m.pr_ci_runs), (1, 5));
    assert_eq!(w.check_invariants(), Ok(()));
}

#[test]
fn under_github_a_conflict_is_rebased_before_the_queue_sees_it() {
    let mut w = admitting(Admission::Github, 2, 30, 20);
    let p = add(&mut w, false, true, 1);
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
    let m = w.metrics();
    assert_eq!((m.rebases, m.pr_ci_runs, m.ejected_bad), (1, 2, 0));
}

#[test]
fn under_github_a_push_takes_an_enqueued_pull_request_out_until_it_passes() {
    let mut w = admitting(Admission::Github, 2, 30, 20);
    let p = add(&mut w, false, false, 1);
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
    let mut w = admitting(Admission::Github, 2, 30, 20);
    let base = add(&mut w, false, false, 1);
    let child = add(&mut w, false, false, 2);
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
    let m = w.metrics();
    assert_eq!((m.ejected_good, m.blocked, w.out()), (1, 1, 0));
    assert_eq!(
        checked_lines(&mut w)[3..],
        [
            "wait 2 on 1",
            "eject 1 unjustified: batch 1 failed",
            "blocked 2 on 1"
        ]
    );
}
