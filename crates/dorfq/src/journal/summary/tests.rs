use dorfq_core::Withdrawal;
use serde_json::Value;

use super::*;

fn at(t: u64, kind: Kind) -> Event {
    Event { t, kind }
}

fn arrive(t: u64, pr: u64) -> Event {
    at(
        t,
        Kind::Arrive {
            pr,
            base: None,
            atomic: false,
            head: format!("{pr:040x}"),
        },
    )
}

fn start(t: u64, batch: u64, members: &[u64]) -> Event {
    at(
        t,
        Kind::Start {
            batch,
            base: None,
            members: members.to_vec(),
            probe: members.is_empty(),
        },
    )
}

fn run_end(t: u64, batch: u64, passed: bool, checks: &[&str]) -> Event {
    at(
        t,
        Kind::RunEnd {
            batch,
            passed,
            checks: checks.iter().map(|c| (*c).to_owned()).collect(),
        },
    )
}

fn retire(t: u64, batch: u64, cancelled: bool) -> Event {
    at(t, Kind::Retire { batch, cancelled })
}

fn blocked(t: u64, pr: u64, on: u64) -> Event {
    at(
        t,
        Kind::Blocked {
            pr,
            on,
            r#final: true,
        },
    )
}

fn eject(t: u64, pr: u64) -> Event {
    at(
        t,
        Kind::Eject {
            pr,
            reason: None,
            r#final: true,
        },
    )
}

fn withdraw(t: u64, pr: u64) -> Event {
    at(
        t,
        Kind::Withdraw {
            pr,
            how: Withdrawal::IntentCleared,
        },
    )
}

fn step(t: u64) -> Event {
    at(
        t,
        Kind::Step {
            queued: 0,
            in_flight: 0,
            land: Vec::new(),
            notify: Vec::new(),
            start: Vec::new(),
            eject: Vec::new(),
            retire: Vec::new(),
            at: t,
            tick: 0,
        },
    )
}

/// Every name of the summary, from one hand-built stream over three hours.
fn stream() -> Vec<Event> {
    vec![
        arrive(0, 1),
        arrive(0, 2),
        arrive(0, 3),
        arrive(0, 4),
        arrive(0, 5),
        arrive(0, 6),
        arrive(0, 8),
        at(0, Kind::PrCi { pr: 6 }),
        start(0, 1, &[1, 3]),
        step(0),
        at(
            1800,
            Kind::PrCiEnd {
                pr: 6,
                failed: false,
                cancelled: false,
            },
        ),
        at(
            1800,
            Kind::Repush {
                pr: 8,
                head: format!("{:040x}", 80),
            },
        ),
        at(
            1800,
            Kind::Conflicting {
                pr: 8,
                with: Value::from("trunk"),
            },
        ),
        at(
            1800,
            Kind::Notice {
                pr: 8,
                reason: "probably broken".to_owned(),
            },
        ),
        run_end(3600, 1, true, &[]),
        at(
            3600,
            Kind::Land {
                batch: 1,
                members: vec![1, 3],
            },
        ),
        retire(3600, 1, false),
        arrive(3600, 7),
        start(3600, 2, &[4, 5]),
        step(3600),
        run_end(5400, 2, false, &["build", "lint"]),
        retire(5400, 2, false),
        start(5400, 3, &[5]),
        at(
            5400,
            Kind::Error {
                message: "transient".to_owned(),
                fatal: false,
                at: 5400,
            },
        ),
        retire(6300, 3, true),
        start(6300, 4, &[]),
        // The block comes before its base's ejection: roots are resolved
        // once every exit is known.
        blocked(7200, 2, 4),
        eject(7200, 4),
        at(7200, Kind::Lost { pr: 7 }),
        start(7200, 5, &[5]),
        run_end(9000, 5, true, &[]),
        at(
            9000,
            Kind::Refused {
                batch: 5,
                reason: "trunk moved".to_owned(),
            },
        ),
        retire(9000, 5, false),
        withdraw(9000, 5),
        at(9000, Kind::Merged { pr: 6 }),
        at(
            9000,
            Kind::Restart {
                started: 1,
                dorfq: "0.1.0".to_owned(),
                at: 1,
            },
        ),
        arrive(9000, 7),
        step(10800),
    ]
}

#[test]
fn summarises_every_name() {
    let s = summarise(&stream());
    assert_eq!(s.window_hours, 3.0);
    assert_eq!((s.ticks, s.restarts, s.errors), (3, 1, 1));
    assert_eq!(s.arrived, 9, "two lifetimes of 7");
    assert_eq!(s.landed, 2);
    assert_eq!(s.merged_outside, 1);
    assert_eq!(s.ejected, 2, "4, and 2 blocked on it");
    assert_eq!(s.withdrawn, 1);
    assert_eq!(s.left_with_base, 1);
    assert_eq!(s.lost, 1);
    assert_eq!(s.unfinished, 2, "8, and 7's second lifetime");
    assert_eq!((s.repushes, s.conflicts), (1, 1));
    assert_eq!((s.runs, s.cancelled_runs), (5, 1));
    assert_eq!((s.passed_runs, s.failed_runs), (2, 1));
    assert_eq!((s.probes, s.refused_landings), (1, 1));
    assert_eq!(s.mean_batch_size, Some(1.2));
    // 1.0 + 0.5 + 0.25 + 0.5, and the probe pending since 6300 to the end.
    assert_eq!(s.batch_runner_hours, 3.5);
    assert_eq!(s.batch_runner_hours_per_landed, Some(1.75));
    // Batches 2, 3 and 5 never landed; the pending probe is not waste.
    assert_eq!(s.wasted_fraction, Some(4500.0 / 12600.0));
    assert_eq!(
        (s.batch_run_p50_hours, s.batch_run_p95_hours),
        (Some(0.5), Some(1.0))
    );
    assert_eq!((s.land_p50_hours, s.land_p95_hours), (Some(1.0), Some(1.0)));
    assert_eq!(
        (s.eject_p50_hours, s.eject_p95_hours),
        (Some(2.0), Some(2.0))
    );
    assert_eq!(
        (s.own_ci_p50_hours, s.own_ci_p95_hours),
        (Some(0.5), Some(0.5))
    );
    assert_eq!(s.notices, 1);
    assert_eq!(
        s.failed_checks,
        BTreeMap::from([("build".to_owned(), 1), ("lint".to_owned(), 1)])
    );
}

#[test]
fn a_block_counts_under_the_root_of_its_chain_or_under_left_with_base_alone() {
    let events = vec![
        arrive(0, 1),
        arrive(0, 2),
        arrive(0, 3),
        arrive(0, 4),
        arrive(0, 5),
        arrive(0, 6),
        withdraw(3600, 1),
        blocked(3600, 2, 1),
        blocked(3600, 3, 2),
        // 9 is not in the input; 6's block roots in a `merged`.
        blocked(3600, 4, 9),
        at(3600, Kind::Merged { pr: 5 }),
        blocked(3600, 6, 5),
    ];
    let s = summarise(&events);
    assert_eq!(s.withdrawn, 3, "1, and 2 and 3 down the chain");
    assert_eq!(s.left_with_base, 4, "2, 3, 4 and 6");
    assert_eq!(s.merged_outside, 1);
    assert_eq!((s.landed, s.ejected, s.unfinished), (0, 0, 0));
}

#[test]
fn a_number_arriving_while_its_lifetime_is_open_ends_that_one_as_lost() {
    let s = summarise(&[arrive(0, 1), arrive(3600, 1), withdraw(7200, 1)]);
    assert_eq!((s.arrived, s.lost, s.withdrawn, s.unfinished), (2, 1, 1, 0));
}

#[test]
fn an_empty_stream_has_no_rates() {
    let s = summarise(&[]);
    assert_eq!((s.window_hours, s.arrived, s.runs), (0.0, 0, 0));
    assert_eq!(
        (s.mean_batch_size, s.wasted_fraction, s.land_p50_hours),
        (None, None, None)
    );
}

#[test]
fn the_table_has_a_row_per_name_and_per_failed_check() {
    let table = summarise(&stream()).to_string();
    let names: Vec<&str> = table
        .lines()
        .map(|l| l.rsplit_once("  ").expect("name and value").0.trim_end())
        .collect();
    let json = serde_json::to_value(summarise(&stream())).unwrap();
    let mut expected: Vec<String> = json
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| *k != "failed_checks")
        .cloned()
        .collect();
    expected.sort();
    let mut plain: Vec<String> = names
        .iter()
        .filter(|n| !n.starts_with("failed_checks"))
        .map(|n| (*n).to_owned())
        .collect();
    plain.sort();
    assert_eq!(plain, expected);
    assert!(table.contains("failed_checks build"), "{table}");
    assert!(table.contains("failed_checks lint"), "{table}");
    let empty = summarise(&[]).to_string();
    let land = empty
        .lines()
        .find(|l| l.starts_with("land_p50_hours"))
        .unwrap();
    assert!(land.ends_with(" -"), "{land}");
}

#[test]
fn the_json_names_the_pages_metric_rows() {
    let json = serde_json::to_value(summarise(&stream())).unwrap();
    for name in [
        "arrived",
        "landed",
        "withdrawn",
        "left_with_base",
        "unfinished",
        "runs",
        "cancelled_runs",
        "mean_batch_size",
        "wasted_fraction",
        "land_p50_hours",
        "land_p95_hours",
    ] {
        assert!(json.get(name).is_some(), "{name}");
    }
}

#[test]
fn land_percentiles_rank_as_the_simulators_do() {
    let hours = [7, 1, 9, 4, 10, 2, 8, 3, 6, 5, 13];
    let mut events = Vec::new();
    let mut records = Vec::new();
    for (pr, h) in (1..).zip(hours) {
        let left = h * 3600;
        events.push(arrive(0, pr));
        events.push(at(
            left,
            Kind::Land {
                batch: pr,
                members: vec![pr],
            },
        ));
        records.push(dorfq_sim::PrRecord {
            number: dorfq_core::PrNumber(pr),
            arrived_at: 0,
            left: Some((left, dorfq_sim::Exit::Landed { bad: false })),
            rebases: 0,
            ever_bad: false,
            notice: None,
        });
    }
    events.sort_by_key(|e| e.t);
    let real = summarise(&events);
    let simulator = dorfq_sim::Metrics::summarise(
        &records,
        &[],
        &[],
        &[],
        dorfq_sim::RunningSeconds {
            batches: 0,
            billed_pr_ci: 0,
        },
        dorfq_sim::End::drained(13 * 3600),
    );
    assert_eq!(real.landed, hours.len());
    assert_eq!(real.land_p50_hours, simulator.land_p50_hours);
    assert_eq!(real.land_p95_hours, simulator.land_p95_hours);
}

#[test]
fn a_batch_whose_build_conflicted_counts_in_no_run_figure() {
    let events = [
        arrive(0, 1),
        arrive(0, 2),
        start(0, 1, &[1]),
        start(0, 2, &[1, 2]),
        at(
            0,
            Kind::Conflict {
                batch: 2,
                pr: 2,
                with: Value::from(1),
            },
        ),
        retire(3_600, 2, false),
        run_end(7_200, 1, false, &[]),
        retire(7_200, 1, false),
    ];
    let s = summarise(&events);
    assert_eq!(s.runs, 1);
    assert_eq!(s.mean_batch_size, Some(1.0));
    assert_eq!(s.batch_runner_hours, 2.0);
    assert_eq!(s.wasted_fraction, Some(1.0));
}
