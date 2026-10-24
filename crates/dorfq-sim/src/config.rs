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
    /// The run then drains, and stops at `hours + drain_hours` at the latest.
    pub drain_hours: u64,
    /// Poisson arrival rate.
    pub arrivals_per_hour: f64,
    /// Concurrent CI runs; further runs wait for a runner.
    pub runners: usize,
    /// Uniform duration of a batch's CI run.
    pub ci_minutes: Minutes,
    /// Uniform duration of a pull request's own CI run, which must pass on
    /// its current head before a batch may take it; `{ min = 0, max = 0 }`
    /// costs no runner time, but the run still waits its turn for a runner.
    pub pr_ci_minutes: Minutes,
    /// Uniform delay from a rebase becoming necessary — a conflict, or under
    /// `github` admission a landed base — to the author's push of it.
    pub rebase_minutes: Minutes,
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
    /// An arrival is cut from a recent arrival's branch, and stacks on it
    /// while that one is queued.
    pub p_stacked: f64,
    /// A stacked arrival lands only with its ancestors.
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
    /// Checks every range.
    pub fn validate(&self) -> Result<(), SimConfigError> {
        if self.runners == 0 {
            return Err(invalid("world.runners", "must be at least 1"));
        }
        let longest_run = minutes("ci_minutes", self.ci_minutes)?;
        let longest_pr_ci = minutes("pr_ci_minutes", self.pr_ci_minutes)?;
        let longest_rebase = minutes("rebase_minutes", self.rebase_minutes)?;
        // The clock is u64 seconds: the deadline plus the longest delay must
        // stay below its end, so no delay and no deadline overflows it.
        let longest = longest_run.max(longest_pr_ci).max(longest_rebase);
        let deadline =
            (u128::from(self.hours) + u128::from(self.drain_hours)) * u128::from(crate::HOUR);
        if deadline + longest >= u128::from(u64::MAX) {
            return Err(invalid(
                "world.hours",
                "plus drain_hours and the longest delay must fit the simulator's clock",
            ));
        }
        probability("p_bad", self.p_bad)?;
        probability("p_interact", self.p_interact)?;
        probability("p_flake", self.p_flake)?;
        probability("p_conflict", self.p_conflict)?;
        probability("p_stacked", self.p_stacked)?;
        probability("p_atomic", self.p_atomic)?;
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
pr_ci_minutes = { min = 40, max = 60 }
rebase_minutes = { min = 15, max = 120 }
p_bad = 0.05
p_interact = 0.002
p_flake = 0.02
p_conflict = 0.02
repush_per_hour = 0.0
withdraw_per_hour = 0.0
p_stacked = 0.25
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
        assert_eq!(config.world.drain_hours, 24);
        assert_eq!(config.world.arrivals_per_hour, 0.75);
        assert_eq!(config.world.runners, 2);
        assert_eq!(config.world.ci_minutes, Minutes { min: 150, max: 210 });
        assert_eq!(config.world.pr_ci_minutes, Minutes { min: 40, max: 60 });
        assert_eq!(config.world.rebase_minutes, Minutes { min: 15, max: 120 });
        assert_eq!((config.world.p_stacked, config.world.p_atomic), (0.25, 0.2));
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
                Admission::Label
            ]
        );
        assert_eq!(
            policies,
            [
                ("all-at-once", &SchedulerConfig::AllAtOnce {}),
                (
                    "github-like",
                    &SchedulerConfig::Bisect {
                        lanes: 5,
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
    fn the_busy_world_parses_with_its_four_policies() {
        let config = SimConfig::parse(include_str!("../examples/busy.toml")).unwrap();
        assert_eq!(config.world.arrivals_per_hour, 5.0);
        assert_eq!(config.world.runners, 6);
        assert_eq!(config.world.pr_ci_minutes, Minutes { min: 15, max: 25 });
        assert_eq!(config.world.rebase_minutes, Minutes { min: 15, max: 120 });
        let admissions: Vec<Admission> = config.policy.iter().map(|p| p.admission).collect();
        assert_eq!(
            admissions,
            [
                Admission::Label,
                Admission::Github,
                Admission::Label,
                Admission::Label
            ]
        );
        let names: Vec<&str> = config.policy.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["all-at-once", "github-like", "bisect", "bayes"]);
        assert_eq!(
            config.policy[1].scheduler,
            SchedulerConfig::Bisect {
                lanes: 5,
                max_batch: 1
            }
        );
        assert_eq!(
            config.policy[2].scheduler,
            SchedulerConfig::Bisect {
                lanes: 6,
                max_batch: 1
            }
        );
        let SchedulerConfig::Bayes(bayes) = config.policy[3].scheduler else {
            panic!("{:?}", config.policy[3]);
        };
        assert_eq!((bayes.lanes, bayes.max_batch), (6, 4));
        assert_eq!(
            (bayes.p_bad, bayes.p_interact, bayes.p_flake),
            (
                config.world.p_bad,
                config.world.p_interact,
                config.world.p_flake
            )
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
                "kind": "bisect", "lanes": 5, "max_batch": 1
            })
        );
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
            ("p_stacked", "1.1"),
            ("p_atomic", "-0.5"),
            ("runners", "0"),
            ("arrivals_per_hour", "-1.0"),
            ("repush_per_hour", "-1.0"),
            ("withdraw_per_hour", "inf"),
            ("ci_minutes", "{ min = 3, max = 2 }"),
            ("pr_ci_minutes", "{ min = 3, max = 2 }"),
            ("rebase_minutes", "{ min = 3, max = 2 }"),
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
        for field in ["pr_ci_minutes", "rebase_minutes"] {
            let late = rejected(&with(field, "{ min = 0, max = 307445734561825860 }"));
            assert!(late.contains("world.hours"), "{field}: {late}");
        }
        // A rate small enough to mean "never" is legal.
        SimConfig::parse(&with("repush_per_hour", "1e-30")).unwrap();
    }

    #[test]
    fn unknown_and_missing_fields_are_named() {
        let unknown = file(&format!("{WORLD}speed = 3\n"), POLICIES);
        assert!(rejected(&unknown).contains("speed"));
        for field in [
            "runners",
            "pr_ci_minutes",
            "rebase_minutes",
            "p_stacked",
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
