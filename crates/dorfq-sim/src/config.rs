//! The simulator's configuration file.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use dorfq_scheduler::SchedulerConfig;
use serde::{Deserialize, Serialize};

/// A whole simulation: the world, the replications, the policies compared.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SimConfig {
    /// Master seed; every replication's streams derive from it.
    pub seed: u64,
    pub replications: u64,
    pub world: WorldConfig,
    pub policy: Vec<Policy>,
}

/// The hidden-truth world every policy runs against.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorldConfig {
    /// Arrivals stop after this many hours.
    pub hours: u64,
    /// The backstop: a replication runs until its world empties, and is cut
    /// off at `hours + drain_hours` at the latest. Set long; reaching it
    /// leaves the work still present counted as unfinished.
    pub drain_hours: u64,
    /// A replication is cut off as stalled `stall_hours` after its last
    /// landing or ejection while a pull request is present. At least 1.
    pub stall_hours: u64,
    /// Mean rate of new pull requests. Stacks arrive as a Poisson process
    /// at this rate divided by the mean number of pull requests a stack
    /// brings, growth included.
    pub arrivals_per_hour: f64,
    /// Uniform duration of a batch's CI run.
    pub ci_minutes: Minutes,
    /// Uniform duration of a pull request's own CI run, which must pass on
    /// its current head before a batch may take it. Every run starts the
    /// instant it is requested; `{ min = 0, max = 0 }` passes at once.
    pub pr_ci_minutes: Minutes,
    /// Uniform delay from a rebase becoming necessary — a conflict, or under
    /// `github` admission a landed base — to the author's push of it.
    pub rebase_minutes: Minutes,
    /// Whether a landed base makes the pull requests stacked on it run
    /// their own CI again — under `github` admission through the author's
    /// rebase of the rest of the stack, under `label` on the direct
    /// children's current heads. The direct children's runs are billed to
    /// the queue. Off, a landed base changes nothing for its stack.
    pub pr_ci_on_base_landed: bool,
    /// A pair of pull requests conflicts: a run holding both unlanded
    /// fails, and whichever lands first breaks the other against trunk.
    pub p_interact: f64,
    /// A run fails for no reason in the tree.
    pub p_flake: f64,
    /// A stack arrives with its root conflicting against trunk; no other
    /// pull request can, being cut from the stack's branch.
    pub p_conflict: f64,
    /// Per queued pull request: head changes, intent kept.
    pub repush_per_hour: f64,
    /// Per queued pull request: intent cleared.
    pub withdraw_per_hour: f64,
    /// Relative weights of a stack arriving with 1, 2, … pull requests:
    /// `stack_sizes[k - 1]` is the weight of size `k`. Non-empty, every
    /// weight finite and at least 0, their sum positive.
    pub stack_sizes: Vec<f64>,
    /// A stack later gets a second burst of pull requests on top of it.
    pub p_stack_grows: f64,
    /// Relative weights of that burst bringing 1, 2, … pull requests, as
    /// `stack_sizes`.
    pub stack_growth_sizes: Vec<f64>,
    /// Uniform delay from a stack's arrival to that second burst.
    pub stack_growth_minutes: Minutes,
    /// A stack lands only whole: every pull request above its root is
    /// `Atomic`.
    pub p_atomic: f64,
}

/// Bounds of a uniform duration, in minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Minutes {
    pub min: u64,
    pub max: u64,
}

/// What the queue asks of a pull request before it sees it, and what
/// takes one out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Admission {
    /// DorfQ's: the label is the intent. A pull request is in the queue from
    /// arrival, stacked on its base, held until its own CI passes on its
    /// current head; a conflict takes it and its descendants out for a
    /// rebase.
    Label,
    /// GitHub's: the queue sees a pull request only once it is on trunk and
    /// its current head has passed its own CI. A stacked pull request waits
    /// for its base to land, is rebased and re-checked; a push takes an
    /// enqueued one out until the new head passes.
    Github,
}

/// One scheduler under comparison, told apart by `name`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Policy {
    /// The report's column header and the trace's label; unique.
    pub name: String,
    pub admission: Admission,
    #[serde(flatten)]
    pub scheduler: SchedulerConfig,
}

/// A configuration the simulator rejects, naming the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimConfigError {
    /// The file is not TOML of the expected shape; the text names the field.
    Parse(String),
    /// A field's value is outside its range.
    Invalid { field: String, requirement: String },
}

impl fmt::Display for SimConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SimConfigError::Parse(message) => write!(f, "invalid configuration: {message}"),
            SimConfigError::Invalid { field, requirement } => {
                write!(f, "invalid configuration: {field} {requirement}")
            }
        }
    }
}

impl Error for SimConfigError {}

fn invalid(field: impl Into<String>, requirement: impl Into<String>) -> SimConfigError {
    SimConfigError::Invalid {
        field: field.into(),
        requirement: requirement.into(),
    }
}

fn probability(field: &str, p: f64) -> Result<(), SimConfigError> {
    if (0.0..=1.0).contains(&p) {
        Ok(())
    } else {
        Err(invalid(format!("world.{field}"), "must be in [0, 1]"))
    }
}

/// Checks a table of relative weights: non-empty, every weight finite and
/// at least 0, their sum positive, and `Σ k·w_k` finite so its mean is.
fn weights(field: &str, table: &[f64]) -> Result<(), SimConfigError> {
    if table.is_empty() {
        return Err(invalid(format!("world.{field}"), "must not be empty"));
    }
    if !table.iter().all(|w| w.is_finite() && *w >= 0.0) {
        return Err(invalid(
            format!("world.{field}"),
            "must hold weights that are finite and at least 0",
        ));
    }
    if table.iter().sum::<f64>() <= 0.0 {
        return Err(invalid(
            format!("world.{field}"),
            "must have a positive sum",
        ));
    }
    if !weighted_sum(table).is_finite() {
        return Err(invalid(
            format!("world.{field}"),
            "must have a finite sum of each size times its weight",
        ));
    }
    Ok(())
}

/// `Σ k·w_k` over a table of weights for sizes 1, 2, ….
fn weighted_sum(table: &[f64]) -> f64 {
    table
        .iter()
        .zip(1u32..)
        .map(|(w, k)| f64::from(k) * w)
        .sum()
}

/// The mean size `Σ k·w_k / Σ w_k` of a table of weights for sizes 1, 2, ….
fn mean_size(table: &[f64]) -> f64 {
    weighted_sum(table) / table.iter().sum::<f64>()
}

fn rate(field: &str, r: f64) -> Result<(), SimConfigError> {
    if r.is_finite() && r >= 0.0 {
        Ok(())
    } else {
        Err(invalid(
            format!("world.{field}"),
            "must be finite and at least 0",
        ))
    }
}

/// Checks a duration's bounds and returns its maximum in seconds.
fn minutes(field: &str, m: Minutes) -> Result<u128, SimConfigError> {
    if m.min > m.max {
        return Err(invalid(
            format!("world.{field}"),
            "must have min no greater than max",
        ));
    }
    let longest = u128::from(m.max) * 60;
    if longest >= u128::from(u64::MAX) {
        return Err(invalid(
            format!("world.{field}"),
            "must have a max that fits the simulator's clock",
        ));
    }
    Ok(longest)
}

impl SimConfig {
    /// Parses and validates a configuration file's text.
    pub fn parse(text: &str) -> Result<SimConfig, SimConfigError> {
        let config: SimConfig =
            toml::from_str(text).map_err(|e| SimConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Checks every range and the uniqueness of policy names.
    pub fn validate(&self) -> Result<(), SimConfigError> {
        if self.replications == 0 {
            return Err(invalid("replications", "must be at least 1"));
        }
        self.world.validate()?;
        if self.policy.is_empty() {
            return Err(invalid("policy", "must name at least one policy"));
        }
        let mut names = BTreeSet::new();
        for policy in &self.policy {
            if !names.insert(policy.name.as_str()) {
                return Err(invalid(
                    format!("policy {:?}", policy.name),
                    "is named twice",
                ));
            }
            policy.scheduler.validate().map_err(|e| {
                invalid(
                    format!("policy {:?}: {}", policy.name, e.field),
                    e.requirement,
                )
            })?;
        }
        Ok(())
    }
}

impl WorldConfig {
    /// The mean number of pull requests a stack brings: its first burst
    /// plus, with probability `p_stack_grows`, its growth.
    pub fn mean_pull_requests_per_stack(&self) -> f64 {
        mean_size(&self.stack_sizes) + self.p_stack_grows * mean_size(&self.stack_growth_sizes)
    }

    /// Checks every range.
    pub fn validate(&self) -> Result<(), SimConfigError> {
        if self.stall_hours == 0 {
            return Err(invalid("world.stall_hours", "must be at least 1"));
        }
        let longest_run = minutes("ci_minutes", self.ci_minutes)?;
        let longest_pr_ci = minutes("pr_ci_minutes", self.pr_ci_minutes)?;
        let longest_rebase = minutes("rebase_minutes", self.rebase_minutes)?;
        let longest_growth = minutes("stack_growth_minutes", self.stack_growth_minutes)?;
        // The clock is u64 seconds: the deadline plus the longest delay must
        // stay below its end, so no delay and no deadline overflows it.
        let longest = longest_run
            .max(longest_pr_ci)
            .max(longest_rebase)
            .max(longest_growth);
        let deadline =
            (u128::from(self.hours) + u128::from(self.drain_hours)) * u128::from(crate::HOUR);
        if deadline + longest >= u128::from(u64::MAX) {
            return Err(invalid(
                "world.hours",
                "plus drain_hours and the longest delay must fit the simulator's clock",
            ));
        }
        probability("p_interact", self.p_interact)?;
        probability("p_flake", self.p_flake)?;
        probability("p_conflict", self.p_conflict)?;
        weights("stack_sizes", &self.stack_sizes)?;
        probability("p_stack_grows", self.p_stack_grows)?;
        weights("stack_growth_sizes", &self.stack_growth_sizes)?;
        probability("p_atomic", self.p_atomic)?;
        rate("arrivals_per_hour", self.arrivals_per_hour)?;
        rate("repush_per_hour", self.repush_per_hour)?;
        rate("withdraw_per_hour", self.withdraw_per_hour)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use dorfq_scheduler::{BayesConfig, Charge, Landing, Speculation};

    pub(crate) const WORLD: &str = "\
hours = 48
drain_hours = 24
stall_hours = 24
arrivals_per_hour = 4.0
ci_minutes = { min = 150, max = 210 }
pr_ci_minutes = { min = 40, max = 60 }
rebase_minutes = { min = 15, max = 120 }
pr_ci_on_base_landed = true
p_interact = 0.002
p_flake = 0.02
p_conflict = 0.02
repush_per_hour = 0.0
withdraw_per_hour = 0.0
stack_sizes = [3.0, 2.0, 1.0]
p_stack_grows = 0.3
stack_growth_sizes = [1.0]
stack_growth_minutes = { min = 30, max = 480 }
p_atomic = 0.2
";

    fn file(world: &str, policies: &str) -> String {
        format!("seed = 1\nreplications = 4\n\n[world]\n{world}\n{policies}")
    }

    const POLICIES: &str = "\
[[policy]]
name = \"floor\"
admission = \"label\"
kind = \"all-at-once\"

[[policy]]
name = \"bisect\"
admission = \"github\"
kind = \"bisect\"
lanes = 2
max_batch = 16
";

    fn with(field: &str, value: &str) -> String {
        let world: String = WORLD
            .lines()
            .map(|line| {
                if line.starts_with(&format!("{field} ")) {
                    format!("{field} = {value}\n")
                } else {
                    format!("{line}\n")
                }
            })
            .collect();
        file(&world, POLICIES)
    }

    fn rejected(text: &str) -> String {
        SimConfig::parse(text).unwrap_err().to_string()
    }

    #[test]
    fn the_example_world_parses() {
        let config = SimConfig::parse(include_str!("../examples/sim.toml")).unwrap();
        assert_eq!(config.seed, 1);
        assert_eq!(config.replications, 4);
        assert_eq!(config.world.hours, 168);
        assert_eq!(config.world.drain_hours, 1000);
        assert_eq!(config.world.stall_hours, 168);
        assert_eq!(config.world.arrivals_per_hour, 0.75);
        assert!(config.world.pr_ci_on_base_landed);
        assert_eq!(config.world.ci_minutes, Minutes { min: 45, max: 120 });
        assert_eq!(config.world.pr_ci_minutes, Minutes { min: 45, max: 120 });
        assert_eq!(config.world.rebase_minutes, Minutes { min: 15, max: 120 });
        let sizes = [
            16.0, 16.0, 15.0, 13.0, 11.0, 9.0, 7.0, 5.0, 3.0, 2.0, 2.0, 1.0,
        ];
        assert_eq!(config.world.stack_sizes, sizes);
        assert_eq!(config.world.p_stack_grows, 0.3);
        assert_eq!(config.world.stack_growth_sizes, [3.0, 2.0, 1.0]);
        assert_eq!(
            config.world.stack_growth_minutes,
            Minutes { min: 30, max: 480 }
        );
        assert_eq!(config.world.p_atomic, 0.2);
        let mean = config.world.mean_pull_requests_per_stack();
        assert!((mean - 4.74).abs() < 1e-9, "{mean}");
        let policies: Vec<_> = config
            .policy
            .iter()
            .map(|p| (p.name.as_str(), &p.scheduler))
            .collect();
        let admissions: Vec<Admission> = config.policy.iter().map(|p| p.admission).collect();
        assert_eq!(
            admissions,
            [
                Admission::Label,
                Admission::Github,
                Admission::Label,
                Admission::Label,
                Admission::Label
            ]
        );
        let bayes = |speculation, charge| BayesConfig {
            lanes: 2,
            max_batch: 16,
            p_interact: 0.001,
            p_flake: 0.02,
            run_cost: 0.5,
            w_reject: 1.0,
            w_age: 0.02,
            eject_above: 0.9,
            others_below: 0.1,
            speculate_above: 0.0,
            speculation,
            charge,
            landing: Landing::Decay,
        };
        assert_eq!(
            policies,
            [
                ("all-at-once", &SchedulerConfig::AllAtOnce {}),
                (
                    "github-like",
                    &SchedulerConfig::Bisect {
                        lanes: 16,
                        max_batch: 1
                    }
                ),
                (
                    "bisect",
                    &SchedulerConfig::Bisect {
                        lanes: 2,
                        max_batch: 8
                    }
                ),
                (
                    "bayes",
                    &SchedulerConfig::Bayes(bayes(Speculation::Chain, Charge::Discounted))
                ),
                (
                    "bayes-tree",
                    &SchedulerConfig::Bayes(bayes(Speculation::Tree, Charge::Full))
                ),
            ]
        );
    }

    #[test]
    fn the_busy_world_parses_with_its_five_policies() {
        let config = SimConfig::parse(include_str!("../examples/busy.toml")).unwrap();
        assert_eq!(config.world.arrivals_per_hour, 5.0);
        assert_eq!(
            (config.world.drain_hours, config.world.stall_hours),
            (1000, 168)
        );
        assert_eq!(config.world.ci_minutes, Minutes { min: 45, max: 120 });
        assert_eq!(
            (config.world.p_flake, config.world.p_interact),
            (0.03, 0.001)
        );
        assert!(config.world.pr_ci_on_base_landed);
        assert_eq!(config.world.stack_sizes.len(), 12);
        assert_eq!(config.world.stack_growth_sizes, [3.0, 2.0, 1.0]);
        assert_eq!(config.world.pr_ci_minutes, Minutes { min: 45, max: 120 });
        assert_eq!(config.world.rebase_minutes, Minutes { min: 15, max: 120 });
        let admissions: Vec<Admission> = config.policy.iter().map(|p| p.admission).collect();
        assert_eq!(
            admissions,
            [
                Admission::Label,
                Admission::Github,
                Admission::Label,
                Admission::Label,
                Admission::Label
            ]
        );
        let names: Vec<&str> = config.policy.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "all-at-once",
                "github-like",
                "bisect",
                "bayes",
                "bayes-tree"
            ]
        );
        assert_eq!(
            config.policy[1].scheduler,
            SchedulerConfig::Bisect {
                lanes: 16,
                max_batch: 1
            }
        );
        assert_eq!(
            config.policy[2].scheduler,
            SchedulerConfig::Bisect {
                lanes: 8,
                max_batch: 1
            }
        );
        let flags: Vec<_> = config.policy[3..]
            .iter()
            .map(|p| {
                let SchedulerConfig::Bayes(bayes) = p.scheduler else {
                    panic!("{p:?}");
                };
                assert_eq!((bayes.lanes, bayes.max_batch), (8, 8));
                assert_eq!(bayes.w_age, 0.02);
                assert_eq!(
                    (bayes.p_interact, bayes.p_flake),
                    (config.world.p_interact, config.world.p_flake)
                );
                (bayes.speculation, bayes.charge, bayes.landing)
            })
            .collect();
        assert_eq!(
            flags,
            [
                (Speculation::Chain, Charge::Discounted, Landing::Decay),
                (Speculation::Tree, Charge::Full, Landing::Decay)
            ]
        );
    }

    #[test]
    fn a_parsed_configuration_round_trips_through_its_serialisation() {
        for text in [
            include_str!("../examples/sim.toml"),
            include_str!("../examples/busy.toml"),
        ] {
            let config = SimConfig::parse(text).unwrap();
            let json = serde_json::to_string(&config).unwrap();
            let back: SimConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(back, config);
        }
        let config = SimConfig::parse(include_str!("../examples/sim.toml")).unwrap();
        let value = serde_json::to_value(&config).unwrap();
        assert_eq!(
            value["policy"][1],
            serde_json::json!({
                "name": "github-like", "admission": "github",
                "kind": "bisect", "lanes": 16, "max_batch": 1
            })
        );
    }

    /// `lanes` is the capacity knob, so a comparison across dorfq's columns
    /// holds it equal; `github-like` runs at the concurrency GitHub's queue
    /// is given in practice.
    #[test]
    fn every_policy_with_lanes_but_github_like_in_an_example_has_the_same_lanes() {
        for (name, text) in [
            ("sim.toml", include_str!("../examples/sim.toml")),
            ("busy.toml", include_str!("../examples/busy.toml")),
        ] {
            let config = SimConfig::parse(text).unwrap();
            let lanes: BTreeSet<usize> = config
                .policy
                .iter()
                .filter(|p| p.name != "github-like")
                .filter_map(|p| match &p.scheduler {
                    SchedulerConfig::Bisect { lanes, .. } => Some(*lanes),
                    SchedulerConfig::Bayes(bayes) => Some(bayes.lanes),
                    SchedulerConfig::AllAtOnce {} => None,
                })
                .collect();
            assert_eq!(lanes.len(), 1, "{name}: {lanes:?}");
        }
    }

    /// The report page shows an example's text as its parameters, so each
    /// parameter's comment is its explanation.
    #[test]
    fn every_parameter_line_of_each_example_is_commented() {
        for (name, text) in [
            ("sim.toml", include_str!("../examples/sim.toml")),
            ("busy.toml", include_str!("../examples/busy.toml")),
        ] {
            let parameters = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('['));
            for line in parameters {
                let comment = line.split_once(" # ").map(|(_, c)| c.trim());
                assert!(
                    comment.is_some_and(|c| !c.is_empty()),
                    "{name}: no comment on {line:?}"
                );
            }
        }
    }

    #[test]
    fn the_mean_stack_counts_its_growth_at_its_probability() {
        let mut world = SimConfig::parse(&file(WORLD, POLICIES)).unwrap().world;
        world.stack_sizes = vec![1.0];
        world.p_stack_grows = 0.0;
        for growth in [vec![1.0], vec![0.0, 0.0, 5.0]] {
            world.stack_growth_sizes = growth;
            assert_eq!(world.mean_pull_requests_per_stack(), 1.0);
        }
        world.stack_sizes = vec![1.0, 1.0];
        world.stack_growth_sizes = vec![0.0, 1.0];
        world.p_stack_grows = 0.5;
        assert_eq!(world.mean_pull_requests_per_stack(), 2.5);
    }

    #[test]
    fn a_world_without_flakes_conflicts_or_interactions_is_legal() {
        let world = WORLD
            .replace("p_interact = 0.002", "p_interact = 0.0")
            .replace("p_flake = 0.02", "p_flake = 0.0")
            .replace("p_conflict = 0.02", "p_conflict = 0.0");
        SimConfig::parse(&file(&world, POLICIES)).unwrap();
    }

    #[test]
    fn every_validation_error_names_its_field() {
        for (field, value) in [
            ("p_interact", "-0.1"),
            ("p_flake", "2.0"),
            ("p_conflict", "-1.0"),
            ("p_stack_grows", "1.1"),
            ("p_atomic", "-0.5"),
            ("arrivals_per_hour", "-1.0"),
            ("repush_per_hour", "-1.0"),
            ("withdraw_per_hour", "inf"),
            ("ci_minutes", "{ min = 3, max = 2 }"),
            ("pr_ci_minutes", "{ min = 3, max = 2 }"),
            ("rebase_minutes", "{ min = 3, max = 2 }"),
            ("stack_growth_minutes", "{ min = 3, max = 2 }"),
        ] {
            let message = rejected(&with(field, value));
            assert!(message.contains(&format!("world.{field}")), "{message}");
        }
        for field in ["stack_sizes", "stack_growth_sizes"] {
            for value in [
                "[]",
                "[1.0, -0.5]",
                "[1.0, inf]",
                "[1.0, nan]",
                "[0.0, 0.0]",
                "[1e308, 1e308]",
                "[0.0, 1e308]",
            ] {
                let message = rejected(&with(field, value));
                assert!(
                    message.contains(&format!("world.{field} ")),
                    "{field} = {value}: {message}"
                );
            }
        }
        let zero = file(WORLD, POLICIES).replace("replications = 4", "replications = 0");
        assert!(rejected(&zero).contains("replications"));
        assert!(rejected(&file(WORLD, "")).contains("policy"));
    }

    #[test]
    fn a_stall_window_of_zero_is_rejected_naming_it() {
        let message = rejected(&with("stall_hours", "0"));
        assert!(message.contains("world.stall_hours"), "{message}");
        let world = SimConfig::parse(&with("stall_hours", "1")).unwrap().world;
        assert_eq!(world.stall_hours, 1);
    }

    #[test]
    fn a_horizon_or_run_beyond_the_clock_is_rejected_by_name() {
        let hours = rejected(&with("hours", "6000000000000000"));
        assert!(hours.contains("world.hours"), "{hours}");
        let drain = rejected(&with("drain_hours", "6000000000000000"));
        assert!(drain.contains("drain_hours"), "{drain}");
        let ci = rejected(&with(
            "ci_minutes",
            "{ min = 0, max = 18446744073709551615 }",
        ));
        assert!(ci.contains("world.ci_minutes"), "{ci}");
        let pr_ci = rejected(&with(
            "pr_ci_minutes",
            "{ min = 0, max = 18446744073709551615 }",
        ));
        assert!(pr_ci.contains("world.pr_ci_minutes"), "{pr_ci}");
        let rebase = rejected(&with(
            "rebase_minutes",
            "{ min = 0, max = 18446744073709551615 }",
        ));
        assert!(rebase.contains("world.rebase_minutes"), "{rebase}");
        // Each fits on its own, but not after the deadline.
        let growth = rejected(&with(
            "stack_growth_minutes",
            "{ min = 0, max = 18446744073709551615 }",
        ));
        assert!(growth.contains("world.stack_growth_minutes"), "{growth}");
        for field in ["pr_ci_minutes", "rebase_minutes", "stack_growth_minutes"] {
            let late = rejected(&with(field, "{ min = 0, max = 307445734561825860 }"));
            assert!(late.contains("world.hours"), "{field}: {late}");
        }
        // A rate small enough to mean "never" is legal.
        SimConfig::parse(&with("repush_per_hour", "1e-30")).unwrap();
    }

    #[test]
    fn unknown_and_missing_fields_are_named() {
        for unknown in [
            "speed = 3",
            "runners = 2",
            "p_stacked = 0.25",
            "p_bad = 0.05",
        ] {
            let name = unknown.split(' ').next().unwrap();
            let text = file(&format!("{WORLD}{unknown}\n"), POLICIES);
            assert!(rejected(&text).contains(name), "{name}");
        }
        for field in [
            "stall_hours",
            "pr_ci_minutes",
            "rebase_minutes",
            "pr_ci_on_base_landed",
            "stack_sizes",
            "p_stack_grows",
            "stack_growth_sizes",
            "stack_growth_minutes",
            "p_atomic",
        ] {
            let line = WORLD.lines().find(|l| l.starts_with(&format!("{field} ")));
            let missing = WORLD.replace(&format!("{}\n", line.unwrap()), "");
            assert!(
                rejected(&file(&missing, POLICIES)).contains(field),
                "{field}"
            );
        }
    }

    #[test]
    fn a_policy_without_a_known_admission_is_rejected_naming_it() {
        let missing = POLICIES.replace("admission = \"label\"\n", "");
        let message = rejected(&file(WORLD, &missing));
        assert!(message.contains("admission"), "{message}");
        let unknown = POLICIES.replace("admission = \"label\"", "admission = \"gitlab\"");
        let message = rejected(&file(WORLD, &unknown));
        assert!(message.contains("gitlab"), "{message}");
        let both = SimConfig::parse(&file(WORLD, POLICIES)).unwrap();
        let admissions: Vec<Admission> = both.policy.iter().map(|p| p.admission).collect();
        assert_eq!(admissions, [Admission::Label, Admission::Github]);
    }

    #[test]
    fn two_policies_with_one_name_are_rejected_naming_it() {
        let twice = POLICIES.replace("name = \"floor\"", "name = \"bisect\"");
        let message = rejected(&file(WORLD, &twice));
        assert!(message.contains("\"bisect\" is named twice"), "{message}");
    }

    #[test]
    fn an_unknown_field_in_a_policy_is_rejected_naming_it() {
        let fielded = POLICIES.replace("max_batch = 16", "max_batch = 16\ndepth = 3");
        assert!(rejected(&file(WORLD, &fielded)).contains("depth"));
        let empty = POLICIES.replace(
            "kind = \"all-at-once\"",
            "kind = \"all-at-once\"\nspeed = 3",
        );
        assert!(rejected(&file(WORLD, &empty)).contains("speed"));
    }

    #[test]
    fn a_policy_failing_validation_is_named_with_its_field() {
        let bad = POLICIES.replace("lanes = 2", "lanes = 0");
        let message = rejected(&file(WORLD, &bad));
        assert!(message.contains("policy \"bisect\": lanes"), "{message}");
    }
}
