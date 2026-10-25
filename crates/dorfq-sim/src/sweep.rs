//! A grid of simulations: one base configuration, axes that set its fields,
//! every point of their cartesian product run at every policy and
//! replication, and the results summarised against a baseline policy.
//!
//! A sweep file is a run file with two more top-level keys: `baseline`, the
//! name of the policy every other policy's deltas are taken against, and
//! `[[axis]]` tables. An axis has `keys`, `values` and optional `labels`
//! (one per value; by default each value's TOML text). A key is
//! `world.<path>` or `policy.<name>.<path>`, `<path>` one or more dotted
//! segments: every segment but the last names a table the point already
//! has, and the last is set to the axis's value. A point is the base with
//! every axis's keys set to that point's value, parsed and validated as a
//! run file is. The first axis varies slowest.

use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use toml::{Table, Value};

use crate::config::SimConfig;
use crate::metrics::Metrics;
use crate::report::{self, Better, PolicyReport, ROWS, Replication, Report};
use crate::{SimError, run};

/// The sweep JSON document's schema number. It changes when a field
/// changes meaning, not when one is added.
pub const SWEEP_SCHEMA: u32 = 2;

/// One axis of the grid.
#[derive(Debug, Clone, PartialEq)]
pub struct Axis {
    /// The fields every value is set at.
    pub keys: Vec<String>,
    pub values: Vec<Value>,
    /// One per value.
    pub labels: Vec<String>,
}

/// One point of the grid: its label on each axis and the configuration it
/// runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub labels: Vec<String>,
    pub config: SimConfig,
}

/// A parsed sweep file.
#[derive(Debug, Clone, PartialEq)]
pub struct SweepConfig {
    /// The file without `baseline` and `axis`: a runnable configuration.
    pub base: SimConfig,
    /// The policy every other policy is compared against.
    pub baseline: String,
    pub axes: Vec<Axis>,
    /// The grid, first axis slowest.
    pub points: Vec<Point>,
}

/// A sweep file the simulator rejects; the text names the key or field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepConfigError(pub String);

impl fmt::Display for SweepConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid sweep: {}", self.0)
    }
}

impl Error for SweepConfigError {}

fn error(message: impl Into<String>) -> SweepConfigError {
    SweepConfigError(message.into())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AxisFile {
    keys: Vec<String>,
    values: Vec<Value>,
    labels: Option<Vec<String>>,
}

impl Axis {
    /// The grid's column header: the distinct last segments of the keys,
    /// joined by `+`.
    pub fn header(&self) -> String {
        let mut names: Vec<&str> = Vec::new();
        for key in &self.keys {
            let name = key.rsplit('.').next().unwrap_or(key);
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names.join("+")
    }
}

/// Parses and validates `table` as a run file.
fn runnable(table: &Table) -> Result<SimConfig, String> {
    let config: SimConfig = Value::Table(table.clone())
        .try_into()
        .map_err(|e: toml::de::Error| format!("invalid configuration: {}", e.message()))?;
    config.validate().map_err(|e| e.to_string())?;
    Ok(config)
}

/// Sets the field `key` names in `table` to `value`.
fn set(table: &mut Table, key: &str, value: Value) -> Result<(), String> {
    let segments: Vec<&str> = key.split('.').collect();
    let (mut current, path) = match segments.as_slice() {
        ["world", path @ ..] if !path.is_empty() => match table.get_mut("world") {
            Some(Value::Table(world)) => (world, path),
            _ => return Err(format!("key {key}: world is not a table")),
        },
        ["policy", name, path @ ..] if !path.is_empty() => {
            let policy = table
                .get_mut("policy")
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
                .filter_map(Value::as_table_mut)
                .find(|p| p.get("name").and_then(Value::as_str) == Some(name));
            match policy {
                Some(policy) => (policy, path),
                None => return Err(format!("key {key}: no policy is named {name:?}")),
            }
        }
        _ => {
            return Err(format!(
                "key {key}: must be world.<field> or policy.<name>.<field>"
            ));
        }
    };
    let (leaf, tables) = path.split_last().expect("path is not empty");
    for segment in tables {
        current = match current.get_mut(*segment) {
            Some(Value::Table(next)) => next,
            _ => return Err(format!("key {key}: {segment} is not a table")),
        };
    }
    current.insert((*leaf).to_owned(), value);
    Ok(())
}

impl SweepConfig {
    /// Parses a sweep file's text and expands its grid, validating every
    /// point.
    pub fn parse(text: &str) -> Result<SweepConfig, SweepConfigError> {
        let mut table: Table = toml::from_str(text).map_err(|e| error(e.to_string()))?;
        let baseline = match table.remove("baseline") {
            Some(Value::String(baseline)) => baseline,
            Some(_) => return Err(error("baseline must be a policy name")),
            None => return Err(error("baseline is missing")),
        };
        let files: Vec<AxisFile> = match table.remove("axis") {
            None => Vec::new(),
            Some(axes) => axes
                .try_into()
                .map_err(|e: toml::de::Error| error(format!("axis: {}", e.message())))?,
        };
        let base = runnable(&table).map_err(|e| error(format!("base: {e}")))?;
        if !base.policy.iter().any(|p| p.name == baseline) {
            return Err(error(format!(
                "baseline {baseline:?} names no policy of the base"
            )));
        }
        let mut axes = Vec::new();
        for (n, file) in files.into_iter().enumerate() {
            let n = n + 1;
            if file.keys.is_empty() {
                return Err(error(format!("axis {n}: keys must not be empty")));
            }
            if file.values.is_empty() {
                return Err(error(format!("axis {n}: values must not be empty")));
            }
            let labels = match file.labels {
                Some(labels) if labels.len() != file.values.len() => {
                    return Err(error(format!(
                        "axis {n}: has {} labels for {} values",
                        labels.len(),
                        file.values.len()
                    )));
                }
                Some(labels) => labels,
                None => file.values.iter().map(Value::to_string).collect(),
            };
            axes.push(Axis {
                keys: file.keys,
                values: file.values,
                labels,
            });
        }
        let points = expand(&table, &axes)?;
        Ok(SweepConfig {
            base,
            baseline,
            axes,
            points,
        })
    }
}

/// Every point of the grid over `base`, first axis slowest.
fn expand(base: &Table, axes: &[Axis]) -> Result<Vec<Point>, SweepConfigError> {
    let count = axes
        .iter()
        .try_fold(1usize, |n, a| n.checked_mul(a.values.len()))
        .ok_or_else(|| error("the grid has more points than fit in memory"))?;
    let mut points = Vec::with_capacity(count);
    for index in 0..count {
        let mut chosen = vec![0; axes.len()];
        let mut rest = index;
        for (a, axis) in axes.iter().enumerate().rev() {
            chosen[a] = rest % axis.values.len();
            rest /= axis.values.len();
        }
        let labels: Vec<String> = axes
            .iter()
            .zip(&chosen)
            .map(|(axis, &v)| axis.labels[v].clone())
            .collect();
        let at = |message: String| error(format!("point [{}]: {message}", labels.join(", ")));
        let mut table = base.clone();
        for (axis, &v) in axes.iter().zip(&chosen) {
            for key in &axis.keys {
                set(&mut table, key, axis.values[v].clone()).map_err(at)?;
            }
        }
        let config = runnable(&table).map_err(at)?;
        points.push(Point { labels, config });
    }
    Ok(points)
}

/// One point's results.
#[derive(Debug, Clone, PartialEq)]
pub struct PointReport {
    pub labels: Vec<String>,
    /// Every policy's replications, untraced, in configuration order.
    pub report: Report,
}

/// A sweep's results.
#[derive(Debug, Clone, PartialEq)]
pub struct SweepReport {
    /// Each axis's column header.
    pub axes: Vec<String>,
    pub baseline: String,
    pub points: Vec<PointReport>,
}

/// Runs every replication of every policy at every point on `jobs` threads.
/// Each replication is an independent task, so the report is the same at
/// any `jobs`.
pub fn sweep(config: &SweepConfig, jobs: NonZeroUsize) -> SweepReport {
    let policies = config.base.policy.len();
    let replications =
        usize::try_from(config.base.replications).expect("replications fit in memory");
    let per_point = policies * replications;
    let tasks = config.points.len() * per_point;
    let next = AtomicUsize::new(0);
    let task = |t: usize| {
        let point = &config.points[t / per_point].config;
        let policy = &point.policy[t % per_point / replications];
        let k = (t % replications) as u64;
        run(
            &point.world,
            policy.admission,
            &policy.scheduler,
            point.seed,
            k,
            None,
        )
    };
    let finished: Vec<Vec<(usize, Result<Metrics, SimError>)>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..jobs.get().min(tasks))
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let t = next.fetch_add(1, Ordering::Relaxed);
                        if t >= tasks {
                            return done;
                        }
                        done.push((t, task(t)));
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| {
                w.join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    let mut slots: Vec<Option<Result<Metrics, SimError>>> = vec![None; tasks];
    for (t, result) in finished.into_iter().flatten() {
        slots[t] = Some(result);
    }
    let mut results = slots.into_iter().map(|slot| slot.expect("every task ran"));
    let points = config
        .points
        .iter()
        .map(|point| PointReport {
            labels: point.labels.clone(),
            report: Report {
                policies: point
                    .config
                    .policy
                    .iter()
                    .map(|policy| PolicyReport {
                        name: policy.name.clone(),
                        replications: results.by_ref().take(replications).collect(),
                        trace: Vec::new(),
                    })
                    .collect(),
            },
        })
        .collect();
    SweepReport {
        axes: config.axes.iter().map(Axis::header).collect(),
        baseline: config.baseline.clone(),
        points,
    }
}

#[derive(Serialize)]
struct SweepDocument<'a> {
    schema: u32,
    name: &'a str,
    config_text: &'a str,
    baseline: &'a str,
    axes: Vec<AxisDocument<'a>>,
    points: Vec<PointDocument<'a>>,
}

#[derive(Serialize)]
struct AxisDocument<'a> {
    keys: &'a [String],
    labels: &'a [String],
}

#[derive(Serialize)]
struct PointDocument<'a> {
    labels: &'a [String],
    config: &'a SimConfig,
    policies: Vec<PolicyDocument<'a>>,
}

#[derive(Serialize)]
struct PolicyDocument<'a> {
    name: &'a str,
    replications: Vec<Replication<'a>>,
}

impl SweepReport {
    /// True when some replication at some point failed.
    pub fn failed(&self) -> bool {
        self.points.iter().any(|p| p.report.failed())
    }

    /// The sweep as one JSON document: the schema number, `name`, the
    /// file's text as read, the baseline, each axis's keys and labels, and
    /// per point its labels, its configuration and every policy's
    /// replications, each its metrics or its error.
    pub fn to_json(&self, name: &str, config: &SweepConfig, config_text: &str) -> String {
        let document = SweepDocument {
            schema: SWEEP_SCHEMA,
            name,
            config_text,
            baseline: &self.baseline,
            axes: config
                .axes
                .iter()
                .map(|a| AxisDocument {
                    keys: &a.keys,
                    labels: &a.labels,
                })
                .collect(),
            points: self
                .points
                .iter()
                .zip(&config.points)
                .map(|(result, point)| PointDocument {
                    labels: &result.labels,
                    config: &point.config,
                    policies: result
                        .report
                        .policies
                        .iter()
                        .map(|p| PolicyDocument {
                            name: &p.name,
                            replications: p.replications.iter().map(Replication::of).collect(),
                        })
                        .collect(),
                })
                .collect(),
        };
        serde_json::to_string(&document).expect("the sweep serialises")
    }

    /// One row per point and policy: the point's index and labels, the
    /// policy, every metric's mean over the replications that have it, and
    /// the failures.
    fn grid(&self) -> Vec<Vec<String>> {
        let header = ["point".to_owned()]
            .into_iter()
            .chain(self.axes.iter().cloned())
            .chain(["policy".to_owned()])
            .chain(ROWS.iter().map(|(name, _, _)| (*name).to_owned()))
            .chain(["failure".to_owned()]);
        let mut lines = vec![header.collect()];
        for (index, point) in self.points.iter().enumerate() {
            for policy in &point.report.policies {
                let metrics = ROWS.iter().map(|&(_, decimals, value)| {
                    policy
                        .mean(value)
                        .map_or_else(|| "-".to_owned(), |m| format!("{m:.decimals$}"))
                });
                let line = [index.to_string()]
                    .into_iter()
                    .chain(point.labels.iter().cloned())
                    .chain([policy.name.clone()])
                    .chain(metrics)
                    .chain([policy.failure()]);
                lines.push(line.collect());
            }
        }
        lines
    }

    /// One row per metric, one column per policy but the baseline: the
    /// mean over points of the relative delta against the baseline, over
    /// the points where the baseline's is not 0, and where the metric has a
    /// direction the points it is better and worse at out of those
    /// compared. A point is compared where both have the metric.
    fn summary(&self) -> Vec<Vec<String>> {
        let Some(first) = self.points.first() else {
            return Vec::new();
        };
        let names: Vec<&str> = first
            .report
            .policies
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        let baseline = names
            .iter()
            .position(|&n| n == self.baseline)
            .expect("the baseline names a policy");
        let others: Vec<usize> = (0..names.len()).filter(|&p| p != baseline).collect();
        let header = [format!("against {}", self.baseline)]
            .into_iter()
            .chain(others.iter().map(|&p| names[p].to_owned()));
        let mut lines = vec![header.collect::<Vec<String>>()];
        for &(name, _, value) in &ROWS {
            let cells = others.iter().map(|&p| {
                let mut deltas = Vec::new();
                let (mut compared, mut better, mut worse) = (0, 0, 0);
                for point in &self.points {
                    let policies = &point.report.policies;
                    let (Some(b), Some(o)) =
                        (policies[baseline].mean(value), policies[p].mean(value))
                    else {
                        continue;
                    };
                    compared += 1;
                    if b != 0.0 {
                        deltas.push((o - b) / b);
                    }
                    let (up, down) = match report::better(name) {
                        Some(Better::Higher) => (o > b, o < b),
                        Some(Better::Lower) => (o < b, o > b),
                        None => (false, false),
                    };
                    better += usize::from(up);
                    worse += usize::from(down);
                }
                let mean = if deltas.is_empty() {
                    "-".to_owned()
                } else {
                    let mean = 100.0 * deltas.iter().sum::<f64>() / deltas.len() as f64;
                    format!("{mean:+.1}%")
                };
                match report::better(name) {
                    Some(_) if compared > 0 => format!("{mean} ({better}+ {worse}- /{compared})"),
                    _ => mean,
                }
            });
            lines.push([name.to_owned()].into_iter().chain(cells).collect());
        }
        lines
    }
}

impl fmt::Display for SweepReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        report::table(f, &self.grid())?;
        let summary = self.summary();
        if summary.first().is_some_and(|header| header.len() > 1) {
            writeln!(f)?;
            report::table(f, &summary)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
