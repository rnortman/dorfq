//! The scenario file: which pull requests exist, when they move, and what
//! CI makes of them.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use dorfq_core::PrNumber;
use serde::Deserialize;

/// A validated scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    /// Observations between a batch's start and its CI verdict.
    pub(crate) ci_ticks: u64,
    /// Whether trunk's own tree fails CI, so that every batch and every
    /// probe fails.
    pub(crate) trunk_broken: bool,
    /// Every pull request, by number.
    pub(crate) prs: BTreeMap<PrNumber, PrSpec>,
}

/// One pull request of a scenario, with its relations made symmetric.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrSpec {
    pub(crate) arrives_at: u64,
    /// The pull request it is based on; `None` for trunk.
    pub(crate) base: Option<PrNumber>,
    /// Whether it and its ancestors land together or not at all.
    pub(crate) atomic: bool,
    pub(crate) mergeable: bool,
    pub(crate) own_ci: OwnCi,
    /// Trunk changes its first head's own CI did not cover when it arrives.
    pub(crate) behind: u32,
    pub(crate) bad: bool,
    pub(crate) breaks_with: BTreeSet<PrNumber>,
    pub(crate) conflicts_with: BTreeSet<PrNumber>,
    pub(crate) repush_at: Option<u64>,
    pub(crate) dequeue_at: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScenario {
    ci_ticks: u64,
    #[serde(default)]
    trunk_broken: bool,
    #[serde(default, rename = "pr")]
    prs: Vec<RawPr>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPr {
    number: u64,
    #[serde(default)]
    arrives_at: u64,
    base: Option<u64>,
    #[serde(default)]
    atomic: bool,
    #[serde(default = "yes")]
    mergeable: bool,
    #[serde(default)]
    own_ci: OwnCi,
    #[serde(default)]
    behind: u32,
    #[serde(default)]
    bad: bool,
    #[serde(default)]
    breaks_with: Vec<u64>,
    #[serde(default)]
    conflicts_with: Vec<u64>,
    repush_at: Option<u64>,
    dequeue_at: Option<u64>,
}

fn yes() -> bool {
    true
}

/// The state of a pull request's own checks on its head.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OwnCi {
    Pending,
    #[default]
    Passed,
    Failed,
}

/// A scenario file that does not describe a scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScenarioError {
    /// Not TOML, or not the scenario's shape: an unknown or missing field,
    /// a value of the wrong type.
    Parse(String),
    /// Two `[[pr]]` entries share a number.
    DuplicateNumber { pr: u64 },
    /// `field` of `pr` names a pull request the scenario does not have.
    UnknownReference {
        pr: u64,
        field: &'static str,
        target: u64,
    },
    /// `field` of `pr` names `pr` itself.
    SelfReference { pr: u64, field: &'static str },
    /// `field` of `pr` fires at or before the tick the pull request arrives.
    NotAfterArrival { pr: u64, field: &'static str },
    /// Following `base` from `pr` leads back to it.
    BaseCycle { pr: u64 },
}

impl fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScenarioError::Parse(message) => write!(f, "invalid scenario: {message}"),
            ScenarioError::DuplicateNumber { pr } => {
                write!(f, "invalid scenario: pull request {pr} is listed twice")
            }
            ScenarioError::UnknownReference { pr, field, target } => write!(
                f,
                "invalid scenario: pull request {pr}: `{field}` names {target}, which is not in the scenario"
            ),
            ScenarioError::SelfReference { pr, field } => {
                write!(
                    f,
                    "invalid scenario: pull request {pr}: `{field}` names itself"
                )
            }
            ScenarioError::NotAfterArrival { pr, field } => write!(
                f,
                "invalid scenario: pull request {pr}: `{field}` must be later than `arrives_at`"
            ),
            ScenarioError::BaseCycle { pr } => write!(
                f,
                "invalid scenario: pull request {pr}: following `base` leads back to it"
            ),
        }
    }
}

impl Error for ScenarioError {}

impl Scenario {
    /// Parses and validates a scenario file.
    pub fn from_toml(text: &str) -> Result<Scenario, ScenarioError> {
        let raw: RawScenario =
            toml::from_str(text).map_err(|e| ScenarioError::Parse(e.to_string()))?;

        let mut numbers = BTreeSet::new();
        for pr in &raw.prs {
            if !numbers.insert(pr.number) {
                return Err(ScenarioError::DuplicateNumber { pr: pr.number });
            }
        }

        let mut prs = BTreeMap::new();
        for pr in &raw.prs {
            for (field, at) in [("repush_at", pr.repush_at), ("dequeue_at", pr.dequeue_at)] {
                if at.is_some_and(|at| at <= pr.arrives_at) {
                    return Err(ScenarioError::NotAfterArrival {
                        pr: pr.number,
                        field,
                    });
                }
            }
            for (field, targets) in [
                ("base", &pr.base.into_iter().collect::<Vec<u64>>()),
                ("breaks_with", &pr.breaks_with),
                ("conflicts_with", &pr.conflicts_with),
            ] {
                for &target in targets {
                    if target == pr.number {
                        return Err(ScenarioError::SelfReference {
                            pr: pr.number,
                            field,
                        });
                    }
                    if !numbers.contains(&target) {
                        return Err(ScenarioError::UnknownReference {
                            pr: pr.number,
                            field,
                            target,
                        });
                    }
                }
            }
            prs.insert(
                PrNumber(pr.number),
                PrSpec {
                    arrives_at: pr.arrives_at,
                    base: pr.base.map(PrNumber),
                    atomic: pr.atomic,
                    mergeable: pr.mergeable,
                    own_ci: pr.own_ci,
                    behind: pr.behind,
                    bad: pr.bad,
                    breaks_with: BTreeSet::new(),
                    conflicts_with: BTreeSet::new(),
                    repush_at: pr.repush_at,
                    dequeue_at: pr.dequeue_at,
                },
            );
        }

        for (&number, spec) in &prs {
            let mut at = spec.base;
            let mut steps = 0;
            while let Some(base) = at {
                if base == number || steps > prs.len() {
                    return Err(ScenarioError::BaseCycle { pr: number.0 });
                }
                steps += 1;
                at = prs[&base].base;
            }
        }

        for pr in raw.prs {
            let number = PrNumber(pr.number);
            for target in pr.breaks_with.into_iter().map(PrNumber) {
                insert_pair(&mut prs, number, target, |spec| &mut spec.breaks_with);
            }
            for target in pr.conflicts_with.into_iter().map(PrNumber) {
                insert_pair(&mut prs, number, target, |spec| &mut spec.conflicts_with);
            }
        }

        Ok(Scenario {
            ci_ticks: raw.ci_ticks,
            trunk_broken: raw.trunk_broken,
            prs,
        })
    }
}

/// Records a symmetric relation on both of its pull requests.
fn insert_pair(
    prs: &mut BTreeMap<PrNumber, PrSpec>,
    a: PrNumber,
    b: PrNumber,
    relation: impl Fn(&mut PrSpec) -> &mut BTreeSet<PrNumber>,
) {
    for (from, to) in [(a, b), (b, a)] {
        if let Some(spec) = prs.get_mut(&from) {
            relation(spec).insert(to);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_err(text: &str) -> ScenarioError {
        Scenario::from_toml(text).expect_err("scenario should be rejected")
    }

    fn parse_message(text: &str) -> String {
        match parse_err(text) {
            ScenarioError::Parse(message) => message,
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    #[test]
    fn defaults_apply() {
        let scenario = Scenario::from_toml("ci_ticks = 2\n[[pr]]\nnumber = 1\n").unwrap();
        assert_eq!(scenario.ci_ticks, 2);
        assert!(!scenario.trunk_broken);
        assert_eq!(
            scenario.prs[&PrNumber(1)],
            PrSpec {
                arrives_at: 0,
                base: None,
                atomic: false,
                mergeable: true,
                own_ci: OwnCi::Passed,
                behind: 0,
                bad: false,
                breaks_with: BTreeSet::new(),
                conflicts_with: BTreeSet::new(),
                repush_at: None,
                dequeue_at: None,
            }
        );
    }

    #[test]
    fn every_field_parses_and_relations_are_symmetric() {
        let scenario = Scenario::from_toml(
            r#"
            ci_ticks = 3
            trunk_broken = true
            [[pr]]
            number = 1
            arrives_at = 2
            base = 3
            atomic = true
            mergeable = false
            own_ci = "failed"
            behind = 3
            bad = true
            breaks_with = [2]
            conflicts_with = [3]
            repush_at = 4
            dequeue_at = 5
            [[pr]]
            number = 2
            own_ci = "pending"
            [[pr]]
            number = 3
            "#,
        )
        .unwrap();
        assert!(scenario.trunk_broken);
        let one = &scenario.prs[&PrNumber(1)];
        assert_eq!(one.arrives_at, 2);
        assert_eq!(one.base, Some(PrNumber(3)));
        assert!(one.atomic);
        assert!(!one.mergeable);
        assert_eq!(one.own_ci, OwnCi::Failed);
        assert_eq!(one.behind, 3);
        assert!(one.bad);
        assert_eq!(one.repush_at, Some(4));
        assert_eq!(one.dequeue_at, Some(5));
        assert_eq!(one.breaks_with, BTreeSet::from([PrNumber(2)]));
        assert_eq!(one.conflicts_with, BTreeSet::from([PrNumber(3)]));
        assert_eq!(scenario.prs[&PrNumber(2)].own_ci, OwnCi::Pending);
        assert_eq!(
            scenario.prs[&PrNumber(2)].breaks_with,
            BTreeSet::from([PrNumber(1)])
        );
        assert_eq!(
            scenario.prs[&PrNumber(3)].conflicts_with,
            BTreeSet::from([PrNumber(1)])
        );
    }

    #[test]
    fn no_pull_requests_is_a_scenario() {
        let scenario = Scenario::from_toml("ci_ticks = 1\n").unwrap();
        assert!(scenario.prs.is_empty());
    }

    #[test]
    fn missing_ci_ticks_is_rejected_by_name() {
        let message = parse_message("[[pr]]\nnumber = 1\n");
        assert!(message.contains("ci_ticks"), "{message}");
    }

    #[test]
    fn unknown_fields_are_rejected_by_name() {
        let message = parse_message("ci_ticks = 1\n[[pr]]\nnumber = 1\nflaky = true\n");
        assert!(message.contains("flaky"), "{message}");
        let message = parse_message("ci_ticks = 1\nspeed = 2\n");
        assert!(message.contains("speed"), "{message}");
    }

    #[test]
    fn missing_number_is_rejected_by_name() {
        let message = parse_message("ci_ticks = 1\n[[pr]]\nbad = true\n");
        assert!(message.contains("number"), "{message}");
    }

    #[test]
    fn unknown_own_ci_is_rejected() {
        parse_message("ci_ticks = 1\n[[pr]]\nnumber = 1\nown_ci = \"green\"\n");
    }

    #[test]
    fn duplicate_number_is_rejected() {
        assert_eq!(
            parse_err("ci_ticks = 1\n[[pr]]\nnumber = 4\n[[pr]]\nnumber = 4\n"),
            ScenarioError::DuplicateNumber { pr: 4 }
        );
    }

    #[test]
    fn dangling_references_are_rejected() {
        assert_eq!(
            parse_err("ci_ticks = 1\n[[pr]]\nnumber = 1\nbreaks_with = [9]\n"),
            ScenarioError::UnknownReference {
                pr: 1,
                field: "breaks_with",
                target: 9
            }
        );
        assert_eq!(
            parse_err("ci_ticks = 1\n[[pr]]\nnumber = 1\nconflicts_with = [9]\n"),
            ScenarioError::UnknownReference {
                pr: 1,
                field: "conflicts_with",
                target: 9
            }
        );
    }

    #[test]
    fn a_base_outside_the_scenario_or_on_itself_is_rejected() {
        assert_eq!(
            parse_err("ci_ticks = 1\n[[pr]]\nnumber = 1\nbase = 9\n"),
            ScenarioError::UnknownReference {
                pr: 1,
                field: "base",
                target: 9
            }
        );
        assert_eq!(
            parse_err("ci_ticks = 1\n[[pr]]\nnumber = 1\nbase = 1\n"),
            ScenarioError::SelfReference {
                pr: 1,
                field: "base"
            }
        );
    }

    #[test]
    fn a_cycle_of_bases_is_rejected() {
        let text = "ci_ticks = 1\n[[pr]]\nnumber = 1\nbase = 3\n[[pr]]\nnumber = 2\nbase = 1\n[[pr]]\nnumber = 3\nbase = 2\n[[pr]]\nnumber = 4\nbase = 1\n";
        assert_eq!(parse_err(text), ScenarioError::BaseCycle { pr: 1 });
        let message = ScenarioError::BaseCycle { pr: 1 }.to_string();
        assert!(message.contains("base"), "{message}");
    }

    #[test]
    fn self_references_are_rejected() {
        assert_eq!(
            parse_err("ci_ticks = 1\n[[pr]]\nnumber = 1\nbreaks_with = [1]\n"),
            ScenarioError::SelfReference {
                pr: 1,
                field: "breaks_with"
            }
        );
        assert_eq!(
            parse_err("ci_ticks = 1\n[[pr]]\nnumber = 1\nconflicts_with = [1]\n"),
            ScenarioError::SelfReference {
                pr: 1,
                field: "conflicts_with"
            }
        );
    }

    #[test]
    fn events_at_or_before_arrival_are_rejected() {
        assert_eq!(
            parse_err("ci_ticks = 1\n[[pr]]\nnumber = 1\narrives_at = 3\nrepush_at = 3\n"),
            ScenarioError::NotAfterArrival {
                pr: 1,
                field: "repush_at"
            }
        );
        assert_eq!(
            parse_err("ci_ticks = 1\n[[pr]]\nnumber = 1\ndequeue_at = 0\n"),
            ScenarioError::NotAfterArrival {
                pr: 1,
                field: "dequeue_at"
            }
        );
    }

    #[test]
    fn errors_name_the_field() {
        let message = ScenarioError::UnknownReference {
            pr: 1,
            field: "breaks_with",
            target: 9,
        }
        .to_string();
        assert!(
            message.contains("breaks_with") && message.contains('9'),
            "{message}"
        );
    }
}
