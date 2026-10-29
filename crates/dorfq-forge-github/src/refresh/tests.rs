use super::*;

const LISTING: u64 = 300;
const PR: u64 = 1800;
const BATCH: u64 = 30;

fn refresh() -> Refresh {
    Refresh {
        listing: Duration::from_secs(LISTING),
        pr: Duration::from_secs(PR),
        batch: Duration::from_secs(BATCH),
    }
}

fn schedule() -> Schedule {
    Schedule::new(&refresh())
}

fn t(seconds: u64) -> Timestamp {
    Timestamp(seconds)
}

fn pr(n: u64) -> Entity {
    Entity::Pr(PrNumber(n))
}

fn batch(id: u64) -> Entity {
    Entity::Batch(BatchId(id))
}

/// One interval of a [`Refresh`], to overwrite.
type Field = fn(&mut Refresh) -> &mut Duration;

#[test]
fn every_interval_must_be_whole_seconds_and_at_least_one() {
    assert_eq!(refresh().validate(), Ok(()));
    let cases: [(&str, Field); 3] = [
        ("listing_secs", |r| &mut r.listing),
        ("pr_secs", |r| &mut r.pr),
        ("batch_secs", |r| &mut r.batch),
    ];
    for (name, field) in cases {
        for bad in [
            Duration::ZERO,
            Duration::from_millis(500),
            Duration::from_millis(1500),
        ] {
            let mut r = refresh();
            *field(&mut r) = bad;
            let error = r.validate().unwrap_err();
            assert!(error.starts_with(name), "{error}");
        }
        let mut r = refresh();
        *field(&mut r) = Duration::from_secs(1);
        assert_eq!(r.validate(), Ok(()), "{name} of one second");
    }
}

#[test]
fn a_new_schedule_has_the_listing_and_trunk_due_and_nothing_else() {
    let s = schedule();
    assert_eq!(s.due(t(0)), vec![Entity::Listing, Entity::Trunk]);
    assert_eq!(s.due(t(1_000_000)), vec![Entity::Listing, Entity::Trunk]);
    assert!(s.tracks(Entity::Listing));
    assert!(s.tracks(Entity::Trunk));
    assert!(!s.tracks(pr(1)));
    assert_eq!(s.next_deadline(), Some(t(0)));
}

#[test]
fn due_lists_listing_trunk_batches_then_pull_requests_in_order() {
    let mut s = schedule();
    for entity in [pr(9), batch(4), pr(2), batch(1)] {
        s.due_now(entity, t(10));
    }
    assert_eq!(
        s.due(t(10)),
        vec![
            Entity::Listing,
            Entity::Trunk,
            batch(1),
            batch(4),
            pr(2),
            pr(9)
        ]
    );
    // Before their deadline only the startup pair is due.
    assert_eq!(s.due(t(9)), vec![Entity::Listing, Entity::Trunk]);
}

#[test]
fn a_refresh_alone_is_due_one_interval_later_by_class() {
    let mut s = schedule();
    s.refreshed(Entity::Listing, t(100), 1, 0);
    s.refreshed(Entity::Trunk, t(100), 1, 0);
    s.refreshed(batch(1), t(100), 1, 0);
    s.refreshed(pr(1), t(100), 1, 0);
    assert_eq!(s.due(t(100 + BATCH - 1)), vec![]);
    assert_eq!(s.due(t(100 + BATCH)), vec![Entity::Trunk, batch(1)]);
    assert_eq!(
        s.due(t(100 + LISTING)),
        vec![Entity::Listing, Entity::Trunk, batch(1)]
    );
    assert!(!s.due(t(100 + PR - 1)).contains(&pr(1)));
    assert!(s.due(t(100 + PR)).contains(&pr(1)));
    assert_eq!(s.next_deadline(), Some(t(100 + BATCH)));
}

#[test]
fn a_class_refreshed_together_is_spread_over_the_next_interval_and_stays_spread() {
    let mut s = schedule();
    s.refreshed(Entity::Listing, t(0), 1, 0);
    s.refreshed(Entity::Trunk, t(0), 1, 0);
    let n = 6;
    for i in 0..n {
        s.refreshed(pr(i as u64 + 1), t(0), n, i);
    }
    let deadline = |s: &Schedule, n: u64| {
        (0..10 * PR)
            .find(|&at| s.due(t(at)).contains(&pr(n)))
            .unwrap()
    };
    let first: Vec<u64> = (1..=6).map(|n| deadline(&s, n)).collect();
    assert_eq!(first, vec![1800, 2100, 2400, 2700, 3000, 3300]);
    assert!(first.iter().all(|&d| (PR..2 * PR).contains(&d)));

    // Each comes due alone and is refreshed alone at its deadline: the gaps
    // between them hold.
    for (i, &at) in first.iter().enumerate() {
        s.refreshed(pr(i as u64 + 1), t(at), 1, 0);
    }
    let second: Vec<u64> = (1..=6).map(|n| deadline(&s, n)).collect();
    assert_eq!(second, first.iter().map(|d| d + PR).collect::<Vec<_>>());
}

#[test]
#[should_panic(expected = "refresh index 2 is not below the class count 2")]
fn a_refresh_index_out_of_its_class_panics() {
    schedule().refreshed(pr(1), t(0), 2, 2);
}

#[test]
fn a_hint_moves_a_deadline_to_now_once_the_floor_has_passed() {
    let mut s = schedule();
    s.refreshed(pr(1), t(100), 1, 0);
    assert_eq!(s.hint(pr(1), t(200)), Some(t(200)));
    assert!(s.due(t(200)).contains(&pr(1)));
}

#[test]
fn a_hint_inside_the_floor_waits_for_it() {
    let mut s = schedule();
    s.refreshed(pr(1), t(100), 1, 0);
    assert_eq!(s.hint(pr(1), t(102)), Some(t(100 + HINT_FLOOR)));
    assert!(!s.due(t(104)).contains(&pr(1)));
    assert!(s.due(t(100 + HINT_FLOOR)).contains(&pr(1)));
}

#[test]
fn two_hints_in_a_burst_cost_one_refresh() {
    let mut s = schedule();
    s.refreshed(Entity::Listing, t(100), 1, 0);
    s.refreshed(Entity::Trunk, t(100), 1, 0);
    s.refreshed(pr(1), t(100), 1, 0);
    s.hint(pr(1), t(110));
    assert_eq!(s.due(t(110)), vec![pr(1)]);
    s.refreshed(pr(1), t(110), 1, 0);
    // The second delivery of the burst arrives a second after the refresh
    // the first caused: it is coalesced into one at the floor.
    s.hint(pr(1), t(111));
    s.hint(pr(1), t(112));
    assert_eq!(s.due(t(114)), vec![]);
    assert_eq!(s.due(t(115)), vec![pr(1)]);
    assert_eq!(s.next_deadline(), Some(t(115)));
}

#[test]
fn a_hint_never_moves_a_deadline_later() {
    let mut s = schedule();
    // Refreshed two seconds ago, then marked due by the queue's own write:
    // a hint's floor would be three seconds away, and it must not apply.
    s.refreshed(pr(1), t(100), 1, 0);
    assert_eq!(s.due_now(pr(1), t(102)), t(102));
    assert_eq!(s.hint(pr(1), t(102)), Some(t(102)));
    assert!(s.due(t(102)).contains(&pr(1)));

    // A deadline already sooner than the hint's floor stands.
    s.soon(pr(2), t(100));
    assert_eq!(s.hint(pr(2), t(101)), Some(t(100 + HINT_FLOOR)));
    assert!(s.due(t(100 + HINT_FLOOR)).contains(&pr(2)));
}

#[test]
fn a_hint_for_an_entity_never_refreshed_makes_it_due_now() {
    let mut s = schedule();
    s.refreshed(Entity::Listing, t(0), 1, 0);
    s.refreshed(Entity::Trunk, t(0), 1, 0);
    s.due_now(pr(3), t(25));
    s.hint(pr(3), t(20));
    assert_eq!(s.due(t(20)), vec![pr(3)]);
}

#[test]
fn a_hint_for_an_untracked_entity_tracks_nothing() {
    let mut s = schedule();
    assert_eq!(s.hint(pr(7), t(10)), None);
    assert_eq!(s.hint(batch(7), t(10)), None);
    assert!(!s.tracks(pr(7)));
    assert!(!s.tracks(batch(7)));
    assert_eq!(s.due(t(10)), vec![Entity::Listing, Entity::Trunk]);
}

#[test]
fn due_now_ignores_the_floor_and_tracks_a_new_entity() {
    let mut s = schedule();
    s.refreshed(batch(2), t(100), 1, 0);
    assert_eq!(s.due_now(batch(2), t(101)), t(101));
    assert!(s.due(t(101)).contains(&batch(2)));
    assert_eq!(s.due_now(pr(5), t(101)), t(101));
    assert!(s.tracks(pr(5)));
    assert!(s.due(t(101)).contains(&pr(5)));
}

#[test]
fn soon_is_due_after_the_floor_and_counts_as_a_refresh() {
    let mut s = schedule();
    s.soon(pr(4), t(100));
    assert!(s.tracks(pr(4)));
    assert!(!s.due(t(100 + HINT_FLOOR - 1)).contains(&pr(4)));
    assert!(s.due(t(100 + HINT_FLOOR)).contains(&pr(4)));
    // A refresh that settles it goes back to the class interval.
    s.refreshed(pr(4), t(105), 1, 0);
    assert!(!s.due(t(105 + PR - 1)).contains(&pr(4)));
    // Its floor runs from the soon refresh as from any other.
    let mut s = schedule();
    s.soon(pr(4), t(100));
    s.due_now(pr(4), t(200));
    s.refreshed(pr(4), t(200), 1, 0);
    s.soon(pr(4), t(300));
    s.hint(pr(4), t(301));
    assert!(!s.due(t(304)).contains(&pr(4)));
    assert!(s.due(t(305)).contains(&pr(4)));
}

#[test]
fn forget_stops_tracking() {
    let mut s = schedule();
    s.refreshed(pr(1), t(0), 1, 0);
    s.forget(pr(1));
    assert!(!s.tracks(pr(1)));
    assert!(!s.due(t(10 * PR)).contains(&pr(1)));
    // Tracked again, it has no memory of the earlier refresh's floor.
    s.due_now(pr(1), t(10));
    s.hint(pr(1), t(1));
    assert!(s.due(t(1)).contains(&pr(1)));
}

#[test]
fn a_pause_holds_the_next_deadline_back_and_a_later_one_stands() {
    let mut s = schedule();
    s.refreshed(Entity::Listing, t(0), 1, 0);
    s.refreshed(Entity::Trunk, t(0), 1, 0);
    assert_eq!(s.next_deadline(), Some(t(BATCH)));
    assert_eq!(s.paused_until(), None);
    s.pause(t(500));
    assert_eq!(s.next_deadline(), Some(t(500)));
    // An earlier pause does not shorten the one in force.
    s.pause(t(400));
    assert_eq!(s.next_deadline(), Some(t(500)));
    assert_eq!(s.paused_until(), Some(t(500)));
    // Nor does a hint, or an entity made due at once.
    s.due_now(pr(1), t(10));
    s.hint(Entity::Trunk, t(10));
    assert_eq!(s.next_deadline(), Some(t(500)));
    // Past the pause, the earliest deadline is the answer again.
    let mut s = schedule();
    s.pause(t(5));
    s.refreshed(Entity::Listing, t(0), 1, 0);
    s.refreshed(Entity::Trunk, t(0), 1, 0);
    assert_eq!(s.next_deadline(), Some(t(BATCH)));
}

#[test]
fn a_pause_does_not_change_what_is_due() {
    let mut s = schedule();
    s.pause(t(1000));
    assert_eq!(s.due(t(0)), vec![Entity::Listing, Entity::Trunk]);
}

#[test]
fn nothing_tracked_has_no_next_deadline() {
    let mut s = schedule();
    s.forget(Entity::Listing);
    s.forget(Entity::Trunk);
    s.pause(t(100));
    assert_eq!(s.next_deadline(), None);
}
