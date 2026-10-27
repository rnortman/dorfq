//! Choosing and parameterising a scheduler.
//!
//! A change to what a scheduler decides is a flag: an enum field on that
//! scheduler's config, one variant per behaviour, the status quo named for
//! what it is and never `off` or `on`. A variant that carries parameters
//! makes the flag a `kind`-tagged table, as [`SchedulerConfig`] is, so a
//! parameter meaningless under one variant is absent from it; a flag whose
//! variants carry none is a bare string. Every flag is required and has no
//! default, so a configuration names every behaviour it runs with. Each
//! flag's doc comment states which other parameters change meaning under
//! each variant. A flag is retired by deleting the losing variant and the
//! field.

use std::error::Error;
use std::fmt;

use dorfq_core::Scheduler;
use serde::{Deserialize, Serialize};

use crate::bayes::{Bayes, FLOOR, Priors};
use crate::{AllAtOnce, Bisect};

/// The scheduler to build, as a `[scheduler]` table or a simulator policy
/// entry names it by `kind`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SchedulerConfig {
    /// The stub that batches every ready pull request at once.
    AllAtOnce {},
    /// The bisecting baseline.
    Bisect {
        /// Batches kept in flight at once.
        lanes: usize,
        /// Own members of a fresh batch.
        max_batch: usize,
    },
    /// The Bayesian scheduler.
    Bayes(BayesConfig),
}

/// The Bayesian scheduler's parameters.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BayesConfig {
    /// Batches kept pending at once.
    pub lanes: usize,
    /// Own members of a fresh batch.
    pub max_batch: usize,
    /// Prior: a pair fails only together; also the regrowth per pull
    /// request landed, from which every `Bad` starts at its origin.
    pub p_interact: f64,
    /// A run fails for no reason.
    pub p_flake: f64,
    /// Prior: CI misses a real break at this rate, own CI and the queue's
    /// alike, so a passing run lowers the belief in every cause it
    /// exercised by this factor in odds and never to nothing.
    pub p_miss: f64,
    /// Prior: between one passing run on trunk and the next, trunk or CI
    /// breaks for a reason the queue did not test — CI itself going down,
    /// or a change to trunk by another hand, each change the forge reports
    /// counting once more.
    pub p_trunk: f64,
    /// One run, in value units, where a fresh pull request is worth 1.
    pub run_cost: f64,
    /// Resolving a bad pull request, as a multiple of its value.
    pub w_reject: f64,
    /// What each hour a pull request has waited in the queue adds to its
    /// value: a pull request queued `h` hours is worth `1 + w_age·h` to
    /// land, and `w_reject` times that to resolve.
    pub w_age: f64,
    /// The posterior at which a singled-out cause is ejected.
    // TODO(soft-eject): a second, lower threshold at which the author is told
    // the pull request is probably broken, without an ejection.
    pub eject_above: f64,
    /// A run singles a cause out when every other cause in it is below this.
    pub others_below: f64,
    /// A candidate starts when its score exceeds this.
    pub speculate_above: f64,
    /// How batches beyond the first are chosen.
    pub speculation: Speculation,
}

/// How the scheduler chooses batches beyond the first.
///
/// `lanes` bounds the pending batches under both variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Speculation {
    /// One chain of batches built to land, each on the previous tip, and
    /// culprit finders on trunk beside it; at most one extension a step.
    Chain,
    /// Every standing batch's tip is a node, and so is trunk; candidates on
    /// every node are scored at the chance their batch lands, which counts
    /// each older batch on the same node failing, and a batch whose node
    /// dies, or whose chance of landing falls to zero, is cancelled.
    Tree,
}

/// A parameter outside its range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// The offending field.
    pub field: &'static str,
    /// What the field must be.
    pub requirement: &'static str,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.field, self.requirement)
    }
}

impl Error for ConfigError {}

fn at_least_one(field: &'static str, value: usize) -> Result<(), ConfigError> {
    if value >= 1 {
        Ok(())
    } else {
        Err(ConfigError {
            field,
            requirement: "must be at least 1",
        })
    }
}

fn probability(field: &'static str, value: f64) -> Result<(), ConfigError> {
    if value > 0.0 && value < 1.0 {
        Ok(())
    } else {
        Err(ConfigError {
            field,
            requirement: "must be strictly between 0 and 1",
        })
    }
}

/// A prior belief: above the floor below which a pair is no cause, and
/// below 1.
fn belief(field: &'static str, value: f64) -> Result<(), ConfigError> {
    if value > FLOOR && value < 1.0 {
        Ok(())
    } else {
        Err(ConfigError {
            field,
            requirement: "must be above the belief floor 1e-6 and below 1",
        })
    }
}

fn non_negative(field: &'static str, value: f64) -> Result<(), ConfigError> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(ConfigError {
            field,
            requirement: "must be a finite number, zero or more",
        })
    }
}

fn finite(field: &'static str, value: f64) -> Result<(), ConfigError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(ConfigError {
            field,
            requirement: "must be a finite number",
        })
    }
}

impl SchedulerConfig {
    /// Checks every parameter's range.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self {
            SchedulerConfig::AllAtOnce {} => Ok(()),
            SchedulerConfig::Bisect { lanes, max_batch } => {
                at_least_one("lanes", *lanes)?;
                at_least_one("max_batch", *max_batch)
            }
            SchedulerConfig::Bayes(config) => config.validate(),
        }
    }

    /// Builds the scheduler. Call [`SchedulerConfig::validate`] first.
    pub fn build(&self) -> Box<dyn Scheduler> {
        match self {
            SchedulerConfig::AllAtOnce {} => Box::new(AllAtOnce::new()),
            SchedulerConfig::Bisect { lanes, max_batch } => {
                Box::new(Bisect::new(*lanes, *max_batch))
            }
            SchedulerConfig::Bayes(config) => Box::new(Bayes::new(*config)),
        }
    }
}

impl BayesConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        at_least_one("lanes", self.lanes)?;
        at_least_one("max_batch", self.max_batch)?;
        belief("p_interact", self.p_interact)?;
        probability("p_flake", self.p_flake)?;
        probability("p_miss", self.p_miss)?;
        belief("p_trunk", self.p_trunk)?;
        finite("run_cost", self.run_cost)?;
        finite("w_reject", self.w_reject)?;
        non_negative("w_age", self.w_age)?;
        finite("eject_above", self.eject_above)?;
        finite("others_below", self.others_below)?;
        finite("speculate_above", self.speculate_above)?;
        if self.eject_above > self.others_below {
            Ok(())
        } else {
            Err(ConfigError {
                field: "eject_above",
                requirement: "must be greater than others_below",
            })
        }
    }

    /// The model's priors.
    pub(crate) fn priors(&self) -> Priors {
        Priors {
            p_interact: self.p_interact,
            p_flake: self.p_flake,
            p_miss: self.p_miss,
            p_trunk: self.p_trunk,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use dorfq_core::PrNumber;

    fn parse(text: &str) -> Result<SchedulerConfig, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    const BISECT: &str = "kind = \"bisect\"\nlanes = 2\nmax_batch = 16\n";

    const BAYES: &str = "\
kind = \"bayes\"
lanes = 2
max_batch = 24
p_interact = 0.002
p_flake = 0.02
p_miss = 0.02
p_trunk = 0.001
run_cost = 0.5
w_reject = 1.0
w_age = 0.02
eject_above = 0.9
others_below = 0.1
speculate_above = 0.0
speculation = \"chain\"
";

    fn bayes() -> BayesConfig {
        match parse(BAYES).unwrap() {
            SchedulerConfig::Bayes(config) => config,
            other => panic!("{other:?}"),
        }
    }

    /// `bayes()` with one field replaced, validated.
    fn bayes_with(edit: impl FnOnce(&mut BayesConfig)) -> Result<(), ConfigError> {
        let mut config = bayes();
        edit(&mut config);
        SchedulerConfig::Bayes(config).validate()
    }

    #[test]
    fn each_kind_parses_from_its_example() {
        assert_eq!(
            parse("kind = \"all-at-once\"\n").unwrap(),
            SchedulerConfig::AllAtOnce {}
        );
        assert_eq!(
            parse(BISECT).unwrap(),
            SchedulerConfig::Bisect {
                lanes: 2,
                max_batch: 16
            }
        );
        assert_eq!(
            bayes(),
            BayesConfig {
                lanes: 2,
                max_batch: 24,
                p_interact: 0.002,
                p_flake: 0.02,
                p_miss: 0.02,
                p_trunk: 0.001,
                run_cost: 0.5,
                w_reject: 1.0,
                w_age: 0.02,
                eject_above: 0.9,
                others_below: 0.1,
                speculate_above: 0.0,
                speculation: Speculation::Chain,
            }
        );
        assert_eq!(SchedulerConfig::Bayes(bayes()).validate(), Ok(()));
    }

    #[test]
    fn each_flag_parses_every_variant_by_name() {
        let with = |from: &str, to: &str| match parse(&BAYES.replace(from, to)).unwrap() {
            SchedulerConfig::Bayes(config) => config,
            other => panic!("{other:?}"),
        };
        let tree = with("speculation = \"chain\"", "speculation = \"tree\"");
        assert_eq!(tree.speculation, Speculation::Tree);
    }

    #[test]
    fn a_flag_with_an_unknown_variant_or_missing_is_rejected() {
        let line = "speculation = \"chain\"\n";
        // Inside the `kind`-tagged table the error names the variants the
        // flag accepts, not the field.
        let message = parse(&BAYES.replace(line, "speculation = \"on\"\n")).unwrap_err();
        assert!(
            message.contains("unknown variant `on`") && message.contains("`chain` or `tree`"),
            "{message}"
        );
        let message = parse(&BAYES.replace(line, "")).unwrap_err();
        assert!(message.contains("speculation"), "{message}");
    }

    #[test]
    fn a_bayes_config_round_trips_through_toml_with_its_flags() {
        let config = SchedulerConfig::Bayes(BayesConfig {
            speculation: Speculation::Tree,
            ..bayes()
        });
        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("speculation = \"tree\""), "{text}");
        assert_eq!(parse(&text).unwrap(), config);
    }

    #[test]
    fn unknown_kind_is_rejected_by_name() {
        let message = parse("kind = \"clever\"\n").unwrap_err();
        assert!(message.contains("clever"), "{message}");
    }

    #[test]
    fn unknown_field_is_rejected_by_name() {
        let message = parse("kind = \"all-at-once\"\nspeed = 3\n").unwrap_err();
        assert!(message.contains("speed"), "{message}");
        let message = parse(&format!("{BISECT}depth = 3\n")).unwrap_err();
        assert!(message.contains("depth"), "{message}");
        let message = parse(&format!("{BAYES}depth = 3\n")).unwrap_err();
        assert!(message.contains("depth"), "{message}");
        // Retired keys are not keys; a configuration that sets one fails.
        for line in [
            "p_bad = 0.05",
            "charge = \"discounted\"",
            "landing = \"decay\"",
        ] {
            let message = parse(&format!("{BAYES}{line}\n")).unwrap_err();
            let key = line.split(' ').next().unwrap();
            assert!(message.contains(key), "{message}");
        }
    }

    #[test]
    fn missing_field_is_rejected_by_name() {
        let message = parse("kind = \"bisect\"\nlanes = 2\n").unwrap_err();
        assert!(message.contains("max_batch"), "{message}");
        let without = BAYES.replace("w_reject = 1.0\n", "");
        let message = parse(&without).unwrap_err();
        assert!(message.contains("w_reject"), "{message}");
        let without = BAYES.replace("w_age = 0.02\n", "");
        let message = parse(&without).unwrap_err();
        assert!(message.contains("w_age"), "{message}");
        let without = BAYES.replace("p_miss = 0.02\n", "");
        let message = parse(&without).unwrap_err();
        assert!(message.contains("p_miss"), "{message}");
        let without = BAYES.replace("p_trunk = 0.001\n", "");
        let message = parse(&without).unwrap_err();
        assert!(message.contains("p_trunk"), "{message}");
        let message = parse("lanes = 2\n").unwrap_err();
        assert!(message.contains("kind"), "{message}");
    }

    #[test]
    fn ranges_reject_a_value_just_outside() {
        let valid = SchedulerConfig::Bisect {
            lanes: 1,
            max_batch: 1,
        };
        assert_eq!(valid.validate(), Ok(()));
        let error = SchedulerConfig::Bisect {
            lanes: 0,
            max_batch: 1,
        }
        .validate()
        .unwrap_err();
        assert_eq!(error.field, "lanes");
        assert_eq!(error.to_string(), "lanes must be at least 1");
        let error = SchedulerConfig::Bisect {
            lanes: 1,
            max_batch: 0,
        }
        .validate()
        .unwrap_err();
        assert_eq!(error.field, "max_batch");
        assert_eq!(SchedulerConfig::AllAtOnce {}.validate(), Ok(()));
    }

    #[test]
    fn bayes_ranges_reject_a_value_just_outside() {
        let field = |edit: fn(&mut BayesConfig)| bayes_with(edit).unwrap_err().field;
        macro_rules! set {
            ($name:ident = $value:expr) => {
                |config: &mut BayesConfig| config.$name = $value
            };
        }
        assert_eq!(field(set!(lanes = 0)), "lanes");
        assert_eq!(field(set!(max_batch = 0)), "max_batch");
        for p in [0.0, 1.0] {
            let error = bayes_with(|c| set!(p_flake = p)(c)).unwrap_err();
            assert_eq!(
                error.to_string(),
                "p_flake must be strictly between 0 and 1"
            );
        }
        for p in [0.0, 1.0] {
            let error = bayes_with(|c| set!(p_miss = p)(c)).unwrap_err();
            assert_eq!(error.to_string(), "p_miss must be strictly between 0 and 1");
        }
        // `p_interact` at the floor would start every belief at what
        // inference reads as false, and at 1 would make every landing
        // certain breakage.
        for p in [FLOOR, 1.0] {
            let error = bayes_with(|c| set!(p_interact = p)(c)).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("p_interact must be above the belief floor {FLOOR:e} and below 1")
            );
            let error = bayes_with(|c| set!(p_trunk = p)(c)).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("p_trunk must be above the belief floor {FLOOR:e} and below 1")
            );
        }
        assert_eq!(bayes_with(|c| set!(p_trunk = FLOOR * 1.01)(c)), Ok(()));
        // Just inside each open interval is accepted.
        assert_eq!(bayes_with(|c| set!(p_interact = FLOOR * 1.01)(c)), Ok(()));
        assert_eq!(bayes_with(|c| set!(p_interact = 1.0 - 1e-9)(c)), Ok(()));
        assert_eq!(bayes_with(|c| set!(p_flake = 1e-9)(c)), Ok(()));
        assert_eq!(bayes_with(|c| set!(p_flake = 1.0 - 1e-9)(c)), Ok(()));
        // `eject_above` must exceed `others_below`; equal is outside.
        let error = bayes_with(set!(eject_above = 0.1)).unwrap_err();
        assert_eq!(error.field, "eject_above");
        assert_eq!(
            error.to_string(),
            "eject_above must be greater than others_below"
        );
        assert_eq!(bayes_with(set!(eject_above = 0.1 + 1e-9)), Ok(()));
        // TOML allows `inf` and `nan`; neither is a parameter.
        assert_eq!(field(set!(run_cost = f64::INFINITY)), "run_cost");
        assert_eq!(field(set!(w_reject = f64::NAN)), "w_reject");
        assert_eq!(
            field(set!(others_below = f64::NEG_INFINITY)),
            "others_below"
        );
        assert_eq!(field(set!(speculate_above = f64::NAN)), "speculate_above");
        assert_eq!(field(set!(eject_above = f64::INFINITY)), "eject_above");
        // A pull request's value never falls with age; zero is no age term.
        for w in [-1e-9, f64::NAN, f64::INFINITY] {
            let error = bayes_with(|c| set!(w_age = w)(c)).unwrap_err();
            assert_eq!(
                error.to_string(),
                "w_age must be a finite number, zero or more"
            );
        }
        assert_eq!(bayes_with(set!(w_age = 0.0)), Ok(()));
    }

    #[test]
    fn build_gives_the_configured_scheduler() {
        // Three ready pull requests tell the two apart: the stub batches all
        // of them, a bisect of `max_batch = 2` the first two.
        let input = observation(ready_all(&[1, 2, 3]), vec![]);
        let plan = SchedulerConfig::AllAtOnce {}.build().step(&input).unwrap();
        assert_eq!(plan.start.len(), 1);
        assert_eq!(plan.start[0].prs, numbers(&[1, 2, 3]));
        let plan = SchedulerConfig::Bisect {
            lanes: 1,
            max_batch: 2,
        }
        .build()
        .step(&input)
        .unwrap();
        assert_eq!(plan.start.len(), 1);
        assert_eq!(plan.start[0].prs, vec![PrNumber(1), PrNumber(2)]);
    }

    #[test]
    fn build_gives_a_bayes_that_grows_its_batch_by_value() {
        // Thirty ready pull requests: the stub would batch all thirty and a
        // bisect stops at `max_batch`; `Bayes` stops where one more member
        // would lower the batch's value, below `max_batch = 24`.
        let prs: Vec<u64> = (1..=30).collect();
        let plan = SchedulerConfig::Bayes(bayes())
            .build()
            .step(&observation(ready_all(&prs), vec![]))
            .unwrap();
        assert_eq!(plan.start.len(), 1);
        let grown = plan.start[0].prs.len();
        assert!((2..24).contains(&grown), "{grown}");
    }
}
