//! The comparison table, metrics as rows and policies as columns, and the
//! JSON document the report site reads.

use std::fmt;

use serde::Serialize;

use crate::SimError;
use crate::config::SimConfig;
use crate::metrics::Metrics;
use crate::trace::TraceEvent;

/// The JSON document's schema number. It changes when a field changes
/// meaning, not when one is added.
pub const SCHEMA: u32 = 4;

/// One policy's replications, in replication order.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyReport {
    pub name: String,
    pub replications: Vec<Result<Metrics, SimError>>,
    /// The first replication's steps and events, when traced.
    pub trace: Vec<TraceEvent>,
}

/// Every policy's results, in configuration order.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub policies: Vec<PolicyReport>,
}

/// A metric row: its name, the decimals it prints with, and its value.
pub(crate) type Row = (&'static str, usize, fn(&Metrics) -> Option<f64>);

pub(crate) const ROWS: [Row; 26] = [
    ("arrived", 1, |m| Some(m.arrived as f64)),
    ("ever_bad", 1, |m| Some(m.ever_bad as f64)),
    ("landed", 1, |m| Some(m.landed as f64)),
    ("ejected_bad", 1, |m| Some(m.ejected_bad as f64)),
    ("ejected_good", 1, |m| Some(m.ejected_good as f64)),
    ("withdrawn", 1, |m| Some(m.withdrawn as f64)),
    ("left_with_base", 1, |m| Some(m.left_with_base as f64)),
    ("unfinished", 1, |m| Some(m.unfinished as f64)),
    ("rebases", 1, |m| Some(m.rebases as f64)),
    ("runs", 1, |m| Some(m.runs as f64)),
    ("cancelled_runs", 1, |m| Some(m.cancelled_runs as f64)),
    ("mean_batch_size", 2, |m| m.mean_batch_size),
    ("billed_pr_ci_runs", 1, |m| Some(m.billed_pr_ci_runs as f64)),
    ("runner_hours", 1, |m| Some(m.runner_hours)),
    ("billed_pr_ci_hours", 1, |m| Some(m.billed_pr_ci_hours)),
    ("runner_hours_per_landed", 2, |m| m.runner_hours_per_landed),
    ("wasted_fraction", 3, |m| m.wasted_fraction),
    ("land_p50_hours", 2, |m| m.land_p50_hours),
    ("land_p95_hours", 2, |m| m.land_p95_hours),
    ("reject_p50_hours", 2, |m| m.reject_p50_hours),
    ("reject_p95_hours", 2, |m| m.reject_p95_hours),
    ("detect_p50_hours", 2, |m| m.detect_p50_hours),
    ("detect_p95_hours", 2, |m| m.detect_p95_hours),
    ("ended_hours", 1, |m| Some(m.ended_hours)),
    ("cut_off", 2, |m| Some(f64::from(u8::from(m.cut_off)))),
    ("stalled", 2, |m| Some(f64::from(u8::from(m.stalled)))),
];

/// Which way a metric is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Better {
    Higher,
    Lower,
}

/// The metrics of [`ROWS`] that have a direction; the rest have none.
pub(crate) const BETTER: [(&str, Better); 18] = [
    ("landed", Better::Higher),
    ("ejected_bad", Better::Higher),
    ("ejected_good", Better::Lower),
    ("unfinished", Better::Lower),
    ("runs", Better::Lower),
    ("cancelled_runs", Better::Lower),
    ("runner_hours", Better::Lower),
    ("runner_hours_per_landed", Better::Lower),
    ("wasted_fraction", Better::Lower),
    ("land_p50_hours", Better::Lower),
    ("land_p95_hours", Better::Lower),
    ("reject_p50_hours", Better::Lower),
    ("reject_p95_hours", Better::Lower),
    ("detect_p50_hours", Better::Lower),
    ("detect_p95_hours", Better::Lower),
    ("ended_hours", Better::Lower),
    ("cut_off", Better::Lower),
    ("stalled", Better::Lower),
];

/// The direction of the metric `name`, if it has one.
pub(crate) fn better(name: &str) -> Option<Better> {
    BETTER.iter().find(|(n, _)| *n == name).map(|&(_, b)| b)
}

/// The JSON document.
#[derive(Serialize)]
struct Document<'a> {
    schema: u32,
    name: &'a str,
    config: &'a SimConfig,
    config_text: &'a str,
    policies: Vec<PolicyDocument<'a>>,
}

#[derive(Serialize)]
struct PolicyDocument<'a> {
    name: &'a str,
    replications: Vec<Replication<'a>>,
    trace: &'a [TraceEvent],
}

/// A replication's cell: its metrics, or its failure's text.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Replication<'a> {
    Metrics(&'a Metrics),
    Error(String),
}

impl<'a> Replication<'a> {
    pub(crate) fn of(replication: &'a Result<Metrics, SimError>) -> Self {
        match replication {
            Ok(metrics) => Replication::Metrics(metrics),
            Err(error) => Replication::Error(error.to_string()),
        }
    }
}

impl Report {
    /// True when some replication of some policy failed.
    pub fn failed(&self) -> bool {
        self.policies
            .iter()
            .any(|p| p.replications.iter().any(Result::is_err))
    }

    /// The report as one JSON document: the schema number, `name`, the
    /// `config` it was run from and `config_text`, the file's text as read,
    /// and per policy every replication's metrics or error and the trace. A
    /// metric no replication has is `null`.
    pub fn to_json(&self, name: &str, config: &SimConfig, config_text: &str) -> String {
        let policies = self
            .policies
            .iter()
            .map(|p| PolicyDocument {
                name: &p.name,
                replications: p.replications.iter().map(Replication::of).collect(),
                trace: &p.trace,
            })
            .collect();
        let document = Document {
            schema: SCHEMA,
            name,
            config,
            config_text,
            policies,
        };
        serde_json::to_string(&document).expect("the report serialises")
    }
}

impl PolicyReport {
    /// The values of the replications that ran and have the value.
    fn values(&self, value: fn(&Metrics) -> Option<f64>) -> Vec<f64> {
        self.replications
            .iter()
            .filter_map(|r| r.as_ref().ok().and_then(value))
            .collect()
    }

    /// The mean over the replications that ran and have the value; `None`
    /// when none has it.
    pub(crate) fn mean(&self, value: fn(&Metrics) -> Option<f64>) -> Option<f64> {
        let values = self.values(value);
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    }

    /// The mean over the replications that ran and have the value, with the
    /// minimum and maximum; `-` when none has it.
    fn cell(&self, decimals: usize, value: fn(&Metrics) -> Option<f64>) -> String {
        let values = self.values(value);
        let Some(mean) = self.mean(value) else {
            return "-".to_owned();
        };
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        format!("{mean:.decimals$} [{min:.decimals$}–{max:.decimals$}]")
    }

    /// `-`, or the count of failed replications and the first one's text.
    pub(crate) fn failure(&self) -> String {
        let mut failures = self.replications.iter().filter_map(|r| r.as_ref().err());
        match failures.next() {
            None => "-".to_owned(),
            Some(first) => format!("{}: {first}", 1 + failures.count()),
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut lines: Vec<Vec<String>> = Vec::new();
        let header = std::iter::once("metric".to_owned());
        lines.push(
            header
                .chain(self.policies.iter().map(|p| p.name.clone()))
                .collect(),
        );
        for (name, decimals, value) in ROWS {
            let cells = self.policies.iter().map(|p| p.cell(decimals, value));
            lines.push(std::iter::once(name.to_owned()).chain(cells).collect());
        }
        let failures = self.policies.iter().map(PolicyReport::failure);
        lines.push(
            std::iter::once("failure".to_owned())
                .chain(failures)
                .collect(),
        );
        table(f, &lines)
    }
}

/// Writes `lines` as a table: every column padded to its widest cell, two
/// spaces between columns. Every line has as many cells as the first.
pub(crate) fn table(f: &mut fmt::Formatter<'_>, lines: &[Vec<String>]) -> fmt::Result {
    let Some(first) = lines.first() else {
        return Ok(());
    };
    let widths: Vec<usize> = (0..first.len())
        .map(|c| {
            lines
                .iter()
                .map(|l| l[c].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    for line in lines {
        let padded: Vec<String> = line
            .iter()
            .zip(&widths)
            .map(|(cell, &width)| {
                let pad = width - cell.chars().count();
                format!("{cell}{}", " ".repeat(pad))
            })
            .collect();
        writeln!(f, "{}", padded.join("  ").trim_end())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(landed: usize, per_landed: Option<f64>) -> Metrics {
        Metrics {
            arrived: 11,
            landed,
            ejected_bad: 0,
            ejected_good: 0,
            withdrawn: 0,
            unfinished: 10 - landed,
            left_with_base: 1,
            ever_bad: 2,
            rebases: 0,
            runs: 2,
            cancelled_runs: 1,
            mean_batch_size: Some(5.0),
            billed_pr_ci_runs: 3,
            runner_hours: 6.0,
            billed_pr_ci_hours: 1.5,
            runner_hours_per_landed: per_landed,
            wasted_fraction: Some(0.0),
            land_p50_hours: None,
            land_p95_hours: None,
            reject_p50_hours: None,
            reject_p95_hours: None,
            detect_p50_hours: None,
            detect_p95_hours: None,
            ended_hours: 80.0,
            cut_off: landed < 10,
            stalled: false,
        }
    }

    fn report() -> Report {
        Report {
            policies: vec![
                PolicyReport {
                    name: "floor".to_owned(),
                    replications: vec![Ok(metrics(4, Some(1.5))), Ok(metrics(6, Some(1.0)))],
                    trace: Vec::new(),
                },
                PolicyReport {
                    name: "bisect".to_owned(),
                    replications: vec![
                        Err(SimError::Livelock { at: 7 }),
                        Ok(metrics(10, Some(0.6))),
                        Err(SimError::Livelock { at: 9 }),
                    ],
                    trace: Vec::new(),
                },
            ],
        }
    }

    fn row<'a>(table: &'a str, name: &str) -> Vec<&'a str> {
        let line = table
            .lines()
            .find(|l| l.split("  ").next().map(str::trim) == Some(name))
            .unwrap_or_else(|| panic!("no row {name} in {table}"));
        line.split("  ")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect()
    }

    #[test]
    fn columns_are_policy_names_and_cells_are_mean_min_max() {
        let table = report().to_string();
        assert_eq!(row(&table, "metric"), ["metric", "floor", "bisect"]);
        assert_eq!(
            row(&table, "landed"),
            ["landed", "5.0 [4.0–6.0]", "10.0 [10.0–10.0]"]
        );
        assert_eq!(
            row(&table, "runner_hours_per_landed"),
            [
                "runner_hours_per_landed",
                "1.25 [1.00–1.50]",
                "0.60 [0.60–0.60]"
            ]
        );
    }

    #[test]
    fn left_with_base_follows_withdrawn() {
        let table = report().to_string();
        let names: Vec<&str> = table
            .lines()
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        let withdrawn = names.iter().position(|&n| n == "withdrawn").unwrap();
        assert_eq!(names[withdrawn + 1], "left_with_base");
        assert_eq!(
            row(&table, "left_with_base"),
            ["left_with_base", "1.0 [1.0–1.0]", "1.0 [1.0–1.0]"]
        );
    }

    #[test]
    fn the_rows_are_in_order() {
        let table = report().to_string();
        let names: Vec<&str> = table
            .lines()
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "metric",
                "arrived",
                "ever_bad",
                "landed",
                "ejected_bad",
                "ejected_good",
                "withdrawn",
                "left_with_base",
                "unfinished",
                "rebases",
                "runs",
                "cancelled_runs",
                "mean_batch_size",
                "billed_pr_ci_runs",
                "runner_hours",
                "billed_pr_ci_hours",
                "runner_hours_per_landed",
                "wasted_fraction",
                "land_p50_hours",
                "land_p95_hours",
                "reject_p50_hours",
                "reject_p95_hours",
                "detect_p50_hours",
                "detect_p95_hours",
                "ended_hours",
                "cut_off",
                "stalled",
                "failure",
            ]
        );
        assert_eq!(
            row(&table, "billed_pr_ci_hours"),
            ["billed_pr_ci_hours", "1.5 [1.5–1.5]", "1.5 [1.5–1.5]"]
        );
    }

    #[test]
    fn every_direction_names_a_row() {
        for (name, _) in BETTER {
            assert!(ROWS.iter().any(|(row, _, _)| *row == name), "{name}");
        }
        assert_eq!(better("landed"), Some(Better::Higher));
        assert_eq!(better("ejected_bad"), Some(Better::Higher));
        assert_eq!(better("cancelled_runs"), Some(Better::Lower));
        assert_eq!(better("arrived"), None);
        assert_eq!(better("stalled"), Some(Better::Lower));
        assert_eq!(better("detect_p95_hours"), Some(Better::Lower));
        assert_eq!(better("ever_bad"), None);
        assert_eq!(better("left_with_base"), None);
    }

    #[test]
    fn the_end_rows_read_as_fractions_of_replications() {
        let table = report().to_string();
        assert_eq!(
            row(&table, "cut_off"),
            ["cut_off", "1.00 [1.00–1.00]", "0.00 [0.00–0.00]"]
        );
        assert_eq!(
            row(&table, "ended_hours"),
            ["ended_hours", "80.0 [80.0–80.0]", "80.0 [80.0–80.0]"]
        );
    }

    #[test]
    fn a_metric_no_replication_has_prints_a_dash() {
        let table = report().to_string();
        assert_eq!(row(&table, "land_p50_hours"), ["land_p50_hours", "-", "-"]);
    }

    #[test]
    fn failures_are_counted_with_the_first_text() {
        let report = report();
        assert!(report.failed());
        let table = report.to_string();
        assert_eq!(
            row(&table, "failure"),
            ["failure", "-", "2: livelock at t=7"]
        );
    }

    #[test]
    fn a_report_without_failures_has_not_failed() {
        let mut report = report();
        report.policies.truncate(1);
        assert!(!report.failed());
    }

    #[test]
    fn the_document_carries_schema_name_config_cells_and_traces() {
        let text = include_str!("../examples/sim.toml");
        let config = SimConfig::parse(text).unwrap();
        let mut report = report();
        report.policies[0].trace = vec![TraceEvent::Withdraw { t: 3, pr: 1 }];
        let document: serde_json::Value =
            serde_json::from_str(&report.to_json("sim", &config, text)).unwrap();
        assert_eq!(document["schema"], 4);
        assert_eq!(document["name"], "sim");
        assert_eq!(document["config_text"], text);
        assert_eq!(document["config"]["seed"], 1);
        assert_eq!(document["config"]["policy"][0]["kind"], "bisect");
        let floor = &document["policies"][0];
        assert_eq!(floor["name"], "floor");
        assert_eq!(floor["replications"][0]["metrics"]["landed"], 4);
        assert_eq!(
            floor["replications"][0]["metrics"]["land_p50_hours"],
            serde_json::Value::Null
        );
        assert_eq!(
            floor["trace"],
            serde_json::json!([{ "kind": "withdraw", "t": 3, "pr": 1 }])
        );
        let bisect = &document["policies"][1];
        assert_eq!(
            bisect["replications"][0],
            serde_json::json!({ "error": "livelock at t=7" })
        );
        assert_eq!(bisect["replications"][1]["metrics"]["landed"], 10);
        assert_eq!(bisect["trace"], serde_json::json!([]));
    }
}
