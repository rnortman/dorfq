//! The structured trace: what happened in a replication, event by event,
//! as the JSON report carries it and `--trace` prints it.

use std::fmt;

use serde::Serialize;

/// A pull request's hidden truth, as drawn at arrival. The simulator's own:
/// a forge replaying production has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Truth {
    /// Fails on its own against trunk.
    pub bad: bool,
    /// Conflicts textually with trunk.
    pub conflicts: bool,
}

/// One start of a step's plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedStart {
    pub batch: u64,
    pub members: Vec<u64>,
}

/// One ejection of a step's plan, its reason worded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlannedEject {
    pub pr: u64,
    pub reason: String,
}

/// One thing that happened, at `t` seconds on the world's clock. Pull
/// requests and batches are their numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceEvent {
    /// A pull request arrived, stacked on `base` or on trunk.
    Arrive {
        t: u64,
        pr: u64,
        base: Option<u64>,
        atomic: bool,
        truth: Truth,
    },
    /// A head changed, intent kept.
    Repush { t: u64, pr: u64 },
    /// Intent cleared by the world's churn.
    Withdraw { t: u64, pr: u64 },
    /// Left because its base `on` left without landing.
    Blocked { t: u64, pr: u64, on: u64 },
    /// A batch started; `base` is the batch whose tip it runs on, `None`
    /// when that is trunk at the moment of the start.
    Start {
        t: u64,
        batch: u64,
        base: Option<u64>,
        members: Vec<u64>,
    },
    /// The batch's run reached a runner.
    RunBegin { t: u64, batch: u64 },
    /// The run's verdict; `broken` when its tree fails for its content,
    /// `flake` when the flake draw fired.
    RunEnd {
        t: u64,
        batch: u64,
        passed: bool,
        broken: bool,
        flake: bool,
    },
    /// A landing fast-forwarded trunk to the batch's tip.
    Land {
        t: u64,
        batch: u64,
        members: Vec<u64>,
    },
    /// A landing was refused.
    Refused { t: u64, batch: u64, reason: String },
    /// A pull request was ejected; `justified` by the world's truth.
    Eject {
        t: u64,
        pr: u64,
        reason: String,
        justified: bool,
    },
    /// A batch was retired; `cancelled` when its run was waiting or running.
    Retire { t: u64, batch: u64, cancelled: bool },
    /// One scheduler step: the observation's size and the plan.
    Step {
        t: u64,
        queued: usize,
        in_flight: usize,
        land: Vec<u64>,
        start: Vec<PlannedStart>,
        eject: Vec<PlannedEject>,
        retire: Vec<u64>,
    },
}

fn ids(ids: &[u64]) -> String {
    let ids: Vec<String> = ids.iter().map(u64::to_string).collect();
    format!("[{}]", ids.join(","))
}

impl fmt::Display for TraceEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TraceEvent::Arrive {
                pr, base, atomic, ..
            } => {
                write!(f, "arrive {pr}")?;
                if let Some(base) = base {
                    write!(f, " on {base}")?;
                }
                if *atomic {
                    write!(f, " atomic")?;
                }
                Ok(())
            }
            TraceEvent::Repush { pr, .. } => write!(f, "repush {pr}"),
            TraceEvent::Withdraw { pr, .. } => write!(f, "withdraw {pr}"),
            TraceEvent::Blocked { pr, on, .. } => write!(f, "blocked {pr} on {on}"),
            TraceEvent::Start {
                batch,
                base,
                members,
                ..
            } => {
                write!(f, "start {batch} {}", ids(members))?;
                match base {
                    Some(base) => write!(f, " on {base}"),
                    None => write!(f, " on trunk"),
                }
            }
            TraceEvent::RunBegin { batch, .. } => write!(f, "ci {batch} begins"),
            TraceEvent::RunEnd {
                batch,
                passed,
                broken,
                flake,
                ..
            } => {
                let verdict = if *passed { "passed" } else { "failed" };
                write!(f, "ci {batch} {verdict}")?;
                if *broken {
                    write!(f, " broken")?;
                }
                if *flake {
                    write!(f, " flake")?;
                }
                Ok(())
            }
            TraceEvent::Land { batch, members, .. } => {
                write!(f, "land {batch} {}", ids(members))
            }
            TraceEvent::Refused { batch, reason, .. } => write!(f, "refused {batch}: {reason}"),
            TraceEvent::Eject {
                pr,
                reason,
                justified,
                ..
            } => {
                let truth = if *justified {
                    "justified"
                } else {
                    "unjustified"
                };
                write!(f, "eject {pr} {truth}: {reason}")
            }
            TraceEvent::Retire {
                batch, cancelled, ..
            } => {
                write!(f, "retire {batch}")?;
                if *cancelled {
                    write!(f, " cancelled")?;
                }
                Ok(())
            }
            TraceEvent::Step {
                t,
                queued,
                in_flight,
                land,
                start,
                eject,
                retire,
            } => {
                let starts: Vec<String> = start
                    .iter()
                    .map(|s| format!("{}{}", s.batch, ids(&s.members)))
                    .collect();
                let ejects: Vec<String> = eject
                    .iter()
                    .map(|e| format!("{}({})", e.pr, e.reason))
                    .collect();
                write!(
                    f,
                    "t={t} queued={queued} in_flight={in_flight} | land {} | start {} | eject {} | retire {}",
                    ids(land),
                    starts.join(" "),
                    ejects.join(" "),
                    ids(retire),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn every_kind() -> Vec<TraceEvent> {
        vec![
            TraceEvent::Arrive {
                t: 1,
                pr: 7,
                base: Some(3),
                atomic: true,
                truth: Truth {
                    bad: false,
                    conflicts: false,
                },
            },
            TraceEvent::Arrive {
                t: 1,
                pr: 8,
                base: None,
                atomic: false,
                truth: Truth {
                    bad: true,
                    conflicts: true,
                },
            },
            TraceEvent::Repush { t: 2, pr: 7 },
            TraceEvent::Withdraw { t: 3, pr: 3 },
            TraceEvent::Blocked { t: 3, pr: 7, on: 3 },
            TraceEvent::Start {
                t: 4,
                batch: 2,
                base: None,
                members: vec![1, 2],
            },
            TraceEvent::Start {
                t: 4,
                batch: 3,
                base: Some(2),
                members: vec![5],
            },
            TraceEvent::RunBegin { t: 4, batch: 2 },
            TraceEvent::RunEnd {
                t: 5,
                batch: 2,
                passed: true,
                broken: false,
                flake: false,
            },
            TraceEvent::RunEnd {
                t: 5,
                batch: 3,
                passed: false,
                broken: true,
                flake: true,
            },
            TraceEvent::Land {
                t: 6,
                batch: 2,
                members: vec![1, 2],
            },
            TraceEvent::Refused {
                t: 6,
                batch: 4,
                reason: "#9 is gone".to_owned(),
            },
            TraceEvent::Eject {
                t: 6,
                pr: 5,
                reason: "batch 3 failed".to_owned(),
                justified: true,
            },
            TraceEvent::Eject {
                t: 6,
                pr: 6,
                reason: "batch 3 failed".to_owned(),
                justified: false,
            },
            TraceEvent::Retire {
                t: 6,
                batch: 3,
                cancelled: false,
            },
            TraceEvent::Retire {
                t: 6,
                batch: 4,
                cancelled: true,
            },
            TraceEvent::Step {
                t: 6,
                queued: 4,
                in_flight: 1,
                land: vec![2],
                start: vec![
                    PlannedStart {
                        batch: 5,
                        members: vec![6, 7],
                    },
                    PlannedStart {
                        batch: 6,
                        members: vec![8],
                    },
                ],
                eject: vec![PlannedEject {
                    pr: 5,
                    reason: "batch 3 failed".to_owned(),
                }],
                retire: vec![3, 4],
            },
        ]
    }

    #[test]
    fn every_kind_renders_as_a_short_line() {
        let lines: Vec<String> = every_kind().iter().map(ToString::to_string).collect();
        assert_eq!(
            lines,
            [
                "arrive 7 on 3 atomic",
                "arrive 8",
                "repush 7",
                "withdraw 3",
                "blocked 7 on 3",
                "start 2 [1,2] on trunk",
                "start 3 [5] on 2",
                "ci 2 begins",
                "ci 2 passed",
                "ci 3 failed broken flake",
                "land 2 [1,2]",
                "refused 4: #9 is gone",
                "eject 5 justified: batch 3 failed",
                "eject 6 unjustified: batch 3 failed",
                "retire 3",
                "retire 4 cancelled",
                "t=6 queued=4 in_flight=1 | land [2] | start 5[6,7] 6[8] | eject 5(batch 3 failed) | retire [3,4]",
            ]
        );
    }

    #[test]
    fn an_empty_step_keeps_every_section() {
        let step = TraceEvent::Step {
            t: 0,
            queued: 0,
            in_flight: 0,
            land: Vec::new(),
            start: Vec::new(),
            eject: Vec::new(),
            retire: Vec::new(),
        };
        assert_eq!(
            step.to_string(),
            "t=0 queued=0 in_flight=0 | land [] | start  | eject  | retire []"
        );
    }

    #[test]
    fn every_kind_serialises_tagged_with_its_snake_case_name() {
        let kinds: Vec<serde_json::Value> = every_kind()
            .iter()
            .map(|e| serde_json::to_value(e).unwrap()["kind"].clone())
            .collect();
        let expected = [
            "arrive",
            "arrive",
            "repush",
            "withdraw",
            "blocked",
            "start",
            "start",
            "run_begin",
            "run_end",
            "run_end",
            "land",
            "refused",
            "eject",
            "eject",
            "retire",
            "retire",
            "step",
        ];
        assert_eq!(kinds, expected.map(|k| json!(k)));
        let events = every_kind();
        assert_eq!(
            serde_json::to_value(&events[0]).unwrap(),
            json!({
                "kind": "arrive", "t": 1, "pr": 7, "base": 3, "atomic": true,
                "truth": { "bad": false, "conflicts": false }
            })
        );
        assert_eq!(
            serde_json::to_value(&events[5]).unwrap(),
            json!({ "kind": "start", "t": 4, "batch": 2, "base": null, "members": [1, 2] })
        );
        assert_eq!(
            serde_json::to_value(&events[16]).unwrap(),
            json!({
                "kind": "step", "t": 6, "queued": 4, "in_flight": 1, "land": [2],
                "start": [{ "batch": 5, "members": [6, 7] }, { "batch": 6, "members": [8] }],
                "eject": [{ "pr": 5, "reason": "batch 3 failed" }],
                "retire": [3, 4]
            })
        );
    }
}
