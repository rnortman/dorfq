use dorfq_scheduler::{Charge, Landing, SchedulerConfig, Speculation};

use super::*;
use crate::config::Minutes;
use crate::config::tests::WORLD;

const POLICIES: &str = "\
[[policy]]
name = \"floor\"
admission = \"label\"
kind = \"all-at-once\"

[[policy]]
name = \"bisect\"
admission = \"label\"
kind = \"bisect\"
lanes = 2
max_batch = 4
";

/// A sweep file over a short world: `baseline` then `axes` after the base.
fn file(baseline: &str, axes: &str) -> String {
    let world = WORLD.replace("hours = 48", "hours = 4");
    format!(
        "baseline = \"{baseline}\"\nseed = 1\nreplications = 2\n\n[world]\n{world}\n{POLICIES}\n{axes}"
    )
}

fn rejected(text: &str) -> String {
    SweepConfig::parse(text).unwrap_err().to_string()
}

const THREE_AXES: &str = "\
[[axis]]
keys = [\"world.arrivals_per_hour\"]
values = [1.0, 2.0, 3.0]

[[axis]]
keys = [\"world.ci_minutes\", \"world.pr_ci_minutes\"]
values = [{ min = 10, max = 20 }, { min = 30, max = 40 }]
labels = [\"short\", \"long\"]

[[axis]]
keys = [\"policy.bisect.max_batch\"]
values = [1, 8]
";

#[test]
fn three_axes_expand_first_axis_slowest_with_labels() {
    let sweep = SweepConfig::parse(&file("floor", THREE_AXES)).unwrap();
    assert_eq!(sweep.baseline, "floor");
    assert_eq!(sweep.points.len(), 12);
    let labels: Vec<Vec<&str>> = sweep
        .points
        .iter()
        .map(|p| p.labels.iter().map(String::as_str).collect())
        .collect();
    assert_eq!(labels[0], ["1.0", "short", "1"]);
    assert_eq!(labels[1], ["1.0", "short", "8"]);
    assert_eq!(labels[2], ["1.0", "long", "1"]);
    assert_eq!(labels[11], ["3.0", "long", "8"]);
    let headers: Vec<String> = sweep.axes.iter().map(Axis::header).collect();
    assert_eq!(
        headers,
        ["arrivals_per_hour", "ci_minutes+pr_ci_minutes", "max_batch"]
    );
    let last = &sweep.points[11].config;
    assert_eq!(last.world.arrivals_per_hour, 3.0);
    assert_eq!(
        (last.world.ci_minutes, last.world.pr_ci_minutes),
        (Minutes { min: 30, max: 40 }, Minutes { min: 30, max: 40 })
    );
    assert_eq!(
        last.policy[1].scheduler,
        SchedulerConfig::Bisect {
            lanes: 2,
            max_batch: 8
        }
    );
    assert_eq!(
        sweep.base,
        SimConfig::parse(&file_without_sweep_keys()).unwrap()
    );
}

fn file_without_sweep_keys() -> String {
    let world = WORLD.replace("hours = 48", "hours = 4");
    format!("seed = 1\nreplications = 2\n\n[world]\n{world}\n{POLICIES}")
}

#[test]
fn a_dotted_key_sets_a_leaf_two_tables_deep() {
    let axes = "[[axis]]\nkeys = [\"world.ci_minutes.max\"]\nvalues = [300]\n";
    let sweep = SweepConfig::parse(&file("floor", axes)).unwrap();
    assert_eq!(
        sweep.points[0].config.world.ci_minutes,
        Minutes { min: 150, max: 300 }
    );
}

#[test]
fn a_key_through_a_value_that_is_not_a_table_fails_naming_key_and_segment() {
    let axes = "[[axis]]\nkeys = [\"world.p_bad.x\"]\nvalues = [1]\n";
    let message = rejected(&file("floor", axes));
    assert!(
        message.contains("key world.p_bad.x: p_bad is not a table"),
        "{message}"
    );
}

#[test]
fn an_unknown_policy_fails_by_name() {
    let axes = "[[axis]]\nkeys = [\"policy.bayes.lanes\"]\nvalues = [1]\n";
    let message = rejected(&file("floor", axes));
    assert!(
        message.contains("no policy is named \"bayes\""),
        "{message}"
    );
}

#[test]
fn a_key_outside_world_and_policy_fails() {
    let axes = "[[axis]]\nkeys = [\"seed\"]\nvalues = [2]\n";
    let message = rejected(&file("floor", axes));
    assert!(
        message.contains("must be world.<field> or policy.<name>.<field>"),
        "{message}"
    );
}

#[test]
fn a_misspelt_field_fails_by_name() {
    let world = "[[axis]]\nkeys = [\"world.p_badd\"]\nvalues = [0.1]\n";
    let message = rejected(&file("floor", world));
    assert!(message.contains("p_badd"), "{message}");
    let policy = "[[axis]]\nkeys = [\"policy.bisect.max_batchh\"]\nvalues = [2]\n";
    let message = rejected(&file("floor", policy));
    assert!(message.contains("max_batchh"), "{message}");
}

#[test]
fn a_value_out_of_range_fails_naming_the_point_and_the_field() {
    let axes =
        "[[axis]]\nkeys = [\"world.p_bad\"]\nvalues = [0.1, 1.5]\nlabels = [\"low\", \"broken\"]\n";
    let message = rejected(&file("floor", axes));
    assert!(message.contains("point [broken]"), "{message}");
    assert!(
        message.contains("world.p_bad must be in [0, 1]"),
        "{message}"
    );
}

#[test]
fn zero_axes_is_one_point_at_the_base() {
    let sweep = SweepConfig::parse(&file("bisect", "")).unwrap();
    assert_eq!(sweep.points.len(), 1);
    assert!(sweep.points[0].labels.is_empty());
    assert_eq!(sweep.points[0].config, sweep.base);
}

#[test]
fn the_baseline_must_name_a_policy() {
    let message = rejected(&file("bayes", ""));
    assert!(message.contains("baseline \"bayes\""), "{message}");
    let message = rejected(&file_without_sweep_keys());
    assert!(message.contains("baseline is missing"), "{message}");
}

#[test]
fn an_axis_needs_keys_values_and_one_label_per_value() {
    let message = rejected(&file("floor", "[[axis]]\nkeys = []\nvalues = [1]\n"));
    assert!(
        message.contains("axis 1: keys must not be empty"),
        "{message}"
    );
    let axes = "[[axis]]\nkeys = [\"world.p_bad\"]\nvalues = []\n";
    let message = rejected(&file("floor", axes));
    assert!(
        message.contains("axis 1: values must not be empty"),
        "{message}"
    );
    let axes = "[[axis]]\nkeys = [\"world.p_bad\"]\nvalues = [0.1]\nlabels = [\"a\", \"b\"]\n";
    let message = rejected(&file("floor", axes));
    assert!(
        message.contains("axis 1: has 2 labels for 1 values"),
        "{message}"
    );
    let axes = "[[axis]]\nkeys = [\"world.p_bad\"]\nvalues = [0.1]\nname = \"x\"\n";
    let message = rejected(&file("floor", axes));
    assert!(message.contains("name"), "{message}");
}

#[test]
fn the_report_is_the_same_at_any_jobs() {
    let axes = "[[axis]]\nkeys = [\"world.p_flake\"]\nvalues = [0.0, 0.2]\n";
    let config = SweepConfig::parse(&file("floor", axes)).unwrap();
    let one = sweep(&config, NonZeroUsize::new(1).unwrap());
    let four = sweep(&config, NonZeroUsize::new(4).unwrap());
    assert_eq!(one, four);
    assert_eq!(one.points.len(), 2);
    let bisect = &one.points[1].report.policies[1];
    assert_eq!(bisect.name, "bisect");
    assert_eq!(bisect.replications.len(), 2);
    let direct = run(
        &config.points[1].config.world,
        config.points[1].config.policy[1].admission,
        &config.points[1].config.policy[1].scheduler,
        1,
        1,
        None,
    );
    assert_eq!(bisect.replications[1], direct);
}

fn metrics(landed: usize, runs: usize) -> Metrics {
    Metrics {
        arrived: 10,
        landed,
        ejected_bad: 0,
        ejected_good: 0,
        withdrawn: 0,
        blocked: 0,
        unfinished: 10 - landed,
        rebases: 0,
        runs,
        cancelled_runs: 0,
        mean_batch_size: Some(2.0),
        billed_pr_ci_runs: 0,
        runner_hours: runs as f64,
        billed_pr_ci_hours: 0.0,
        runner_hours_per_landed: Some(runs as f64 / landed as f64),
        wasted_fraction: Some(0.0),
        land_p50_hours: None,
        land_p95_hours: None,
        reject_p50_hours: None,
        reject_p95_hours: None,
    }
}

fn policy(name: &str, replications: Vec<Result<Metrics, SimError>>) -> PolicyReport {
    PolicyReport {
        name: name.to_owned(),
        replications,
        trace: Vec::new(),
    }
}

/// Two points, two policies; the second point's `tree` failed once.
fn report() -> SweepReport {
    let point =
        |labels: &[&str], base: Metrics, tree: Vec<Result<Metrics, SimError>>| PointReport {
            labels: labels.iter().map(|&l| l.to_owned()).collect(),
            report: Report {
                policies: vec![policy("chain", vec![Ok(base)]), policy("tree", tree)],
            },
        };
    SweepReport {
        axes: vec!["lanes".to_owned()],
        baseline: "chain".to_owned(),
        points: vec![
            point(&["4"], metrics(4, 8), vec![Ok(metrics(6, 6))]),
            point(
                &["8"],
                metrics(5, 10),
                vec![Ok(metrics(4, 12)), Err(SimError::Livelock { at: 7 })],
            ),
        ],
    }
}

fn cells(line: &str) -> Vec<&str> {
    line.split("  ")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect()
}

fn row<'a>(text: &'a str, first: &str) -> Vec<&'a str> {
    let line = text
        .lines()
        .find(|l| cells(l).first() == Some(&first))
        .unwrap_or_else(|| panic!("no row {first} in {text}"));
    cells(line)
}

#[test]
fn the_grid_has_a_row_per_point_and_policy_with_means_and_failures() {
    let text = report().to_string();
    let header = cells(text.lines().next().unwrap());
    assert_eq!(header[..4], ["point", "lanes", "policy", "arrived"]);
    assert_eq!(header.last(), Some(&"failure"));
    assert!(header.contains(&"cancelled_runs"));
    let rows: Vec<Vec<&str>> = text.lines().skip(1).take(4).map(cells).collect();
    assert_eq!(rows[0][..5], ["0", "4", "chain", "10.0", "4.0"]);
    assert_eq!(rows[1][..5], ["0", "4", "tree", "10.0", "6.0"]);
    assert_eq!(rows[3][..5], ["1", "8", "tree", "10.0", "4.0"]);
    assert_eq!(rows[3].last(), Some(&"1: livelock at t=7"));
    assert_eq!(rows[2].last(), Some(&"-"));
}

#[test]
fn the_summary_averages_relative_deltas_over_points_with_direction() {
    let text = report().to_string();
    assert_eq!(row(&text, "against chain"), ["against chain", "tree"]);
    // landed: +50% and -20%, better at one point of two.
    assert_eq!(row(&text, "landed"), ["landed", "+15.0% (1/2)"]);
    // runs: -25% and +20%; lower is better.
    assert_eq!(row(&text, "runs"), ["runs", "-2.5% (1/2)"]);
    // arrived has no direction.
    assert_eq!(row(&text, "arrived"), ["arrived", "+0.0%"]);
    // No point has it, or the baseline is 0.
    assert_eq!(row(&text, "land_p50_hours"), ["land_p50_hours", "-"]);
    assert_eq!(row(&text, "cancelled_runs"), ["cancelled_runs", "-"]);
}

#[test]
fn a_one_policy_sweep_prints_the_grid_alone() {
    let mut report = report();
    for point in &mut report.points {
        point.report.policies.truncate(1);
    }
    let text = report.to_string();
    assert_eq!(text.lines().count(), 3, "{text}");
    assert!(!text.contains("against"), "{text}");
}

#[test]
fn the_document_carries_schema_axes_points_and_cells() {
    let text = file("floor", THREE_AXES);
    let config = SweepConfig::parse(&text).unwrap();
    let mut results = report();
    results.points.truncate(1);
    let document: serde_json::Value =
        serde_json::from_str(&results.to_json("grid", &config, &text)).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["name"], "grid");
    assert_eq!(document["config_text"], text.as_str());
    assert_eq!(document["baseline"], "chain");
    assert_eq!(
        document["axes"][1],
        serde_json::json!({
            "keys": ["world.ci_minutes", "world.pr_ci_minutes"],
            "labels": ["short", "long"]
        })
    );
    let point = &document["points"][0];
    assert_eq!(point["labels"], serde_json::json!(["4"]));
    assert_eq!(point["config"]["world"]["arrivals_per_hour"], 1.0);
    assert_eq!(point["policies"][1]["name"], "tree");
    assert_eq!(
        point["policies"][1]["replications"][0]["metrics"]["landed"],
        6
    );
    assert!(point.get("trace").is_none());
}

/// Each bayes column's settings at a point, flags aside.
fn bayes_settings(config: &SimConfig) -> Vec<(usize, usize, f64, f64, f64)> {
    config
        .policy
        .iter()
        .filter_map(|p| match &p.scheduler {
            SchedulerConfig::Bayes(b) => {
                Some((b.lanes, b.max_batch, b.p_flake, b.p_interact, b.run_cost))
            }
            SchedulerConfig::Bisect { .. } | SchedulerConfig::AllAtOnce {} => None,
        })
        .collect()
}

#[test]
fn the_shipped_speculation_sweep_runs_four_bayes_variants_at_like_settings() {
    let sweep = SweepConfig::parse(include_str!("../../sweeps/speculation.toml")).unwrap();
    let names: Vec<&str> = sweep.base.policy.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "github-like",
            "bayes-chain",
            "bayes-full",
            "bayes-tree",
            "bayes-tree-transfer"
        ]
    );
    assert_eq!(sweep.baseline, "bayes-chain");
    assert_eq!(sweep.points.len(), 288);
    let busy = SimConfig::parse(include_str!("../../examples/busy.toml")).unwrap();
    assert_eq!(sweep.base.world, busy.world);
    let flags: Vec<_> = sweep
        .base
        .policy
        .iter()
        .filter_map(|p| match &p.scheduler {
            SchedulerConfig::Bayes(b) => Some((b.speculation, b.charge, b.landing)),
            SchedulerConfig::Bisect { .. } | SchedulerConfig::AllAtOnce {} => None,
        })
        .collect();
    assert_eq!(
        flags,
        [
            (Speculation::Chain, Charge::Discounted, Landing::Decay),
            (Speculation::Chain, Charge::Full, Landing::Decay),
            (Speculation::Tree, Charge::Full, Landing::Decay),
            (Speculation::Tree, Charge::Full, Landing::Transfer),
        ]
    );
    for point in &sweep.points {
        let world = &point.config.world;
        let settings = bayes_settings(&point.config);
        assert_eq!(settings.len(), 4);
        assert!(settings.iter().all(|s| *s == settings[0]), "{settings:?}");
        assert_eq!(
            (settings[0].2, settings[0].3),
            (world.p_flake, world.p_interact)
        );
        assert_eq!(world.ci_minutes, world.pr_ci_minutes);
        assert_eq!(
            point.config.policy[0].scheduler,
            SchedulerConfig::Bisect {
                lanes: 16,
                max_batch: 1
            }
        );
    }
}

#[test]
fn the_shipped_bisect_cap_sweep_has_twelve_points() {
    let sweep = SweepConfig::parse(include_str!("../../sweeps/bisect-cap.toml")).unwrap();
    assert_eq!(sweep.baseline, "bisect");
    assert_eq!(sweep.base.policy.len(), 1);
    assert_eq!(sweep.points.len(), 12);
    let busy = SimConfig::parse(include_str!("../../examples/busy.toml")).unwrap();
    assert_eq!(sweep.base.world, busy.world);
}
