//! The simulator's configuration file.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use dorfq_scheduler::SchedulerConfig;
use serde::Deserialize;

/// A whole simulation: the world, the replications, the policies compared.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimConfig {
    /// Master seed; every replication's streams derive from it.
    pub seed: u64,
    pub replications: u64,
    pub world: WorldConfig,
    pub policy: Vec<Policy>,
}

/// The hidden-truth world every policy runs against.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldConfig {
    /// Arrivals stop after this many hours.
    pub hours: u64,
    /// The run then drains, and stops at `hours + drain_hours` at the latest.
    pub drain_hours: u64,
    /// Poisson arrival rate.
    pub arrivals_per_hour: f64,
    /// Concurrent CI runs; further runs wait for a runner.
    pub runners: usize,
    /// Uniform run duration.
    pub ci_minutes: CiMinutes,
    /// A pull request breaks on its own against trunk.
    pub p_bad: f64,
    /// A pair of pull requests whose queue time overlapped break only together.
    pub p_interact: f64,
    /// A run fails for no reason in the tree.
    pub p_flake: f64,
    /// A pull request arrives with a textual conflict against trunk.
    pub p_conflict: f64,
    /// Per queued pull request: head changes, intent kept.
    pub repush_per_hour: f64,
    /// Per queued pull request: intent cleared.
    pub withdraw_per_hour: f64,
}

/// Bounds of the uniform CI run duration, in minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CiMinutes {
    pub min: u64,
    pub max: u64,
}

/// One scheduler under comparison, told apart by `name`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Policy {
    /// The report's column header and the trace's label; unique.
    pub name: String,
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
    /// Checks every range.
    pub fn validate(&self) -> Result<(), SimConfigError> {
        if self.runners == 0 {
            return Err(invalid("world.runners", "must be at least 1"));
        }
        if self.ci_minutes.min > self.ci_minutes.max {
            return Err(invalid(
                "world.ci_minutes",
                "must have min no greater than max",
            ));
        }
        // The clock is u64 seconds: the deadline plus the longest run must
        // stay below its end, so no run and no deadline overflows it.
        let longest_run = u128::from(self.ci_minutes.max) * 60;
        if longest_run >= u128::from(u64::MAX) {
            return Err(invalid(
                "world.ci_minutes",
                "must have a max that fits the simulator's clock",
            ));
        }
        let deadline =
            (u128::from(self.hours) + u128::from(self.drain_hours)) * u128::from(crate::HOUR);
        if deadline + longest_run >= u128::from(u64::MAX) {
            return Err(invalid(
                "world.hours",
                "plus drain_hours and the longest CI run must fit the simulator's clock",
            ));
        }
        probability("p_bad", self.p_bad)?;
        probability("p_interact", self.p_interact)?;
        probability("p_flake", self.p_flake)?;
        probability("p_conflict", self.p_conflict)?;
        rate("arrivals_per_hour", self.arrivals_per_hour)?;
        rate("repush_per_hour", self.repush_per_hour)?;
        rate("withdraw_per_hour", self.withdraw_per_hour)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use dorfq_scheduler::BayesConfig;

    pub(crate) const WORLD: &str = "\
hours = 48
drain_hours = 24
arrivals_per_hour = 4.0
runners = 2
ci_minutes = { min = 150, max = 210 }
p_bad = 0.05
p_interact = 0.002
p_flake = 0.02
p_conflict = 0.02
repush_per_hour = 0.0
withdraw_per_hour = 0.0
";

    fn file(world: &str, policies: &str) -> String {
        format!("seed = 1\nreplications = 4\n\n[world]\n{world}\n{policies}")
    }

    const POLICIES: &str = "\
[[policy]]
name = \"floor\"
kind = \"all-at-once\"

[[policy]]
name = \"bisect\"
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
        assert_eq!(config.world.drain_hours, 24);
        assert_eq!(config.world.arrivals_per_hour, 0.75);
        assert_eq!(config.world.runners, 2);
        assert_eq!(config.world.ci_minutes, CiMinutes { min: 150, max: 210 });
        let policies: Vec<_> = config
            .policy
            .iter()
            .map(|p| (p.name.as_str(), &p.scheduler))
            .collect();
        assert_eq!(
            policies,
            [
                ("all-at-once", &SchedulerConfig::AllAtOnce {}),
                (
                    "bisect",
                    &SchedulerConfig::Bisect {
                        lanes: 2,
                        max_batch: 8
                    }
                ),
                (
                    "bayes",
                    &SchedulerConfig::Bayes(BayesConfig {
                        lanes: 2,
                        max_batch: 24,
                        p_bad: 0.05,
                        p_interact: 0.002,
                        p_flake: 0.02,
                        run_cost: 0.5,
                        w_reject: 1.0,
                        eject_above: 0.9,
                        others_below: 0.1,
                        speculate_above: 0.0,
                    })
                ),
            ]
        );
    }

    #[test]
    fn a_world_without_flakes_bad_pull_requests_or_interactions_is_legal() {
        let world = WORLD
            .replace("p_bad = 0.05", "p_bad = 0.0")
            .replace("p_interact = 0.002", "p_interact = 0.0")
            .replace("p_flake = 0.02", "p_flake = 0.0")
            .replace("p_conflict = 0.02", "p_conflict = 0.0");
        SimConfig::parse(&file(&world, POLICIES)).unwrap();
    }

    #[test]
    fn every_validation_error_names_its_field() {
        for (field, value) in [
            ("p_bad", "1.5"),
            ("p_interact", "-0.1"),
            ("p_flake", "2.0"),
            ("p_conflict", "-1.0"),
            ("runners", "0"),
            ("arrivals_per_hour", "-1.0"),
            ("repush_per_hour", "-1.0"),
            ("withdraw_per_hour", "inf"),
            ("ci_minutes", "{ min = 3, max = 2 }"),
        ] {
            let message = rejected(&with(field, value));
            assert!(message.contains(&format!("world.{field}")), "{message}");
        }
        let zero = file(WORLD, POLICIES).replace("replications = 4", "replications = 0");
        assert!(rejected(&zero).contains("replications"));
        assert!(rejected(&file(WORLD, "")).contains("policy"));
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
        // A rate small enough to mean "never" is legal.
        SimConfig::parse(&with("repush_per_hour", "1e-30")).unwrap();
    }

    #[test]
    fn unknown_and_missing_fields_are_named() {
        let unknown = file(&format!("{WORLD}speed = 3\n"), POLICIES);
        assert!(rejected(&unknown).contains("speed"));
        let missing = file(&WORLD.replace("runners = 2\n", ""), POLICIES);
        assert!(rejected(&missing).contains("runners"));
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
