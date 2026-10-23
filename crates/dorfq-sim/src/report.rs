//! The comparison table: metrics as rows, policies as columns.

use std::fmt;

use crate::SimError;
use crate::metrics::Metrics;

/// One policy's replications, in replication order.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyReport {
    pub name: String,
    pub replications: Vec<Result<Metrics, SimError>>,
    /// The first replication's step and event lines, when traced.
    pub trace: Vec<String>,
}

/// Every policy's results, in configuration order.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub policies: Vec<PolicyReport>,
}

/// A metric row: its name, the decimals it prints with, and its value.
type Row = (&'static str, usize, fn(&Metrics) -> Option<f64>);

const ROWS: [Row; 16] = [
    ("arrived", 1, |m| Some(m.arrived as f64)),
    ("landed", 1, |m| Some(m.landed as f64)),
    ("ejected_bad", 1, |m| Some(m.ejected_bad as f64)),
    ("ejected_good", 1, |m| Some(m.ejected_good as f64)),
    ("withdrawn", 1, |m| Some(m.withdrawn as f64)),
    ("unfinished", 1, |m| Some(m.unfinished as f64)),
    ("runs", 1, |m| Some(m.runs as f64)),
    ("mean_batch_size", 2, |m| m.mean_batch_size),
    ("runner_hours", 1, |m| Some(m.runner_hours)),
    ("runner_hours_per_landed", 2, |m| m.runner_hours_per_landed),
    ("wasted_fraction", 3, |m| m.wasted_fraction),
    ("land_p50_hours", 2, |m| m.land_p50_hours),
    ("land_p95_hours", 2, |m| m.land_p95_hours),
    ("reject_p50_hours", 2, |m| m.reject_p50_hours),
    ("reject_p95_hours", 2, |m| m.reject_p95_hours),
    ("utilisation", 3, |m| m.utilisation),
];

impl Report {
    /// True when some replication of some policy failed.
    pub fn failed(&self) -> bool {
        self.policies
            .iter()
            .any(|p| p.replications.iter().any(Result::is_err))
    }
}

impl PolicyReport {
    /// The mean over the replications that ran and have the value, with the
    /// minimum and maximum; `-` when none has it.
    fn cell(&self, decimals: usize, value: fn(&Metrics) -> Option<f64>) -> String {
        let values: Vec<f64> = self
            .replications
            .iter()
            .filter_map(|r| r.as_ref().ok().and_then(value))
            .collect();
        if values.is_empty() {
            return "-".to_owned();
        }
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        format!("{mean:.decimals$} [{min:.decimals$}–{max:.decimals$}]")
    }

    /// `-`, or the count of failed replications and the first one's text.
    fn failure(&self) -> String {
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
        let columns = lines[0].len();
        let widths: Vec<usize> = (0..columns)
            .map(|c| {
                lines
                    .iter()
                    .map(|l| l[c].chars().count())
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        for line in &lines {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(landed: usize, per_landed: Option<f64>) -> Metrics {
        Metrics {
            arrived: 10,
            landed,
            ejected_bad: 0,
            ejected_good: 0,
            withdrawn: 0,
            unfinished: 10 - landed,
            runs: 2,
            mean_batch_size: Some(5.0),
            runner_hours: 6.0,
            runner_hours_per_landed: per_landed,
            wasted_fraction: Some(0.0),
            land_p50_hours: None,
            land_p95_hours: None,
            reject_p50_hours: None,
            reject_p95_hours: None,
            utilisation: Some(0.5),
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
}
