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

/// Why a pull request's author rebases it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rebase {
    /// It conflicts with trunk.
    Conflict,
    /// Its base landed, under `github` admission.
    BaseLanded,
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
    /// Out of the queue with its base `on`: for good when `final`, its base
    /// having left without landing; otherwise its base is out for a rebase
    /// and it returns, traced `admit`, at the base's push.
    Blocked {
        t: u64,
        pr: u64,
        on: u64,
        r#final: bool,
    },
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
    /// A pull request was ejected; `justified` by the world's truth. Not
    /// `final` only for a conflict with trunk that it has: it goes out for a
    /// rebase and returns, traced `admit`.
    Eject {
        t: u64,
        pr: u64,
        reason: String,
        justified: bool,
        r#final: bool,
    },
    /// A batch was retired; `cancelled` when its run was waiting or running.
    Retire { t: u64, batch: u64, cancelled: bool },
    /// A pull request's own CI was requested for its current head; it waits
    /// for a runner.
    PrCi { t: u64, pr: u64 },
    /// The pull request's own CI run reached a runner.
    PrCiBegin { t: u64, pr: u64 },
    /// The pull request's own CI run ended: passed, or `cancelled` by a
    /// head change, waiting or running.
    PrCiEnd { t: u64, pr: u64, cancelled: bool },
    /// The pull request's author starts rebasing it, for `why`; the push
    /// follows as `repush`.
    Rebase { t: u64, pr: u64, why: Rebase },
    /// The queue sees the pull request from now, stacked on `base` or on
    /// trunk.
    Admit { t: u64, pr: u64, base: Option<u64> },
    /// The pull request left the queue for its own CI on a new head.
    Recheck { t: u64, pr: u64 },
    /// `github` admission: checked, it waits for its base `on` to land.
    Wait { t: u64, pr: u64, on: u64 },
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
            TraceEvent::Blocked {
                pr, on, r#final, ..
            } => {
                let how = if *r#final { "on" } else { "with" };
                write!(f, "blocked {pr} {how} {on}")
            }
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
                r#final,
                ..
            } => {
                let truth = if *justified {
                    "justified"
                } else {
                    "unjustified"
                };
                write!(f, "eject {pr} {truth}: {reason}")?;
                if !*r#final {
                    write!(f, ", rebasing")?;
                }
                Ok(())
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
            TraceEvent::PrCi { pr, .. } => write!(f, "pr-ci {pr}"),
            TraceEvent::PrCiBegin { pr, .. } => write!(f, "pr-ci {pr} begins"),
            TraceEvent::PrCiEnd { pr, cancelled, .. } => {
                let end = if *cancelled { "cancelled" } else { "passed" };
                write!(f, "pr-ci {pr} {end}")
            }
            TraceEvent::Rebase { pr, why, .. } => match why {
                Rebase::Conflict => write!(f, "rebase {pr} conflict"),
                Rebase::BaseLanded => write!(f, "rebase {pr} base landed"),
            },
            TraceEvent::Admit { pr, base, .. } => {
                write!(f, "admit {pr}")?;
                if let Some(base) = base {
                    write!(f, " on {base}")?;
                }
                Ok(())
            }
            TraceEvent::Recheck { pr, .. } => write!(f, "recheck {pr}"),
            TraceEvent::Wait { pr, on, .. } => write!(f, "wait {pr} on {on}"),
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

/// Checks every pull request's life along its own events: after a
/// `blocked` or `eject` that is not final only `rebase` and `repush` come
/// before its return (`admit`), a `withdraw` or a final `blocked`; after a
/// `rebase` comes its push or a withdrawal; after a `wait` a rebase, a
/// push, a final `blocked` or a withdrawal; after a final exit only the
/// run of its last head begins and ends, and after a landing nothing; and
/// no `start` names it unless its latest own-CI event is a pass.
#[cfg(test)]
pub(crate) fn check_lives(events: &[TraceEvent]) -> Result<(), String> {
    use std::collections::BTreeMap;

    /// What a pending `rebase` or `wait` allows next.
    enum Expect {
        AfterRebase,
        AfterWait,
    }

    #[derive(Default)]
    struct Life {
        /// Its latest own-CI event was a pass.
        checked: bool,
        /// Out with a return pending, since this event.
        returning: Option<String>,
        /// The last `rebase` or `wait` and its text, until the next event
        /// of its own.
        expecting: Option<(Expect, String)>,
        /// Left for good, by this event; `begins` and `ends` count its last
        /// run's events since.
        exit: Option<(String, bool)>,
        begins: u32,
        ends: u32,
    }

    let mut lives: BTreeMap<u64, Life> = BTreeMap::new();
    for event in events {
        let text = event.to_string();
        let prs: Vec<u64> = match event {
            TraceEvent::Start { members, .. } => members.clone(),
            TraceEvent::Arrive { pr, .. }
            | TraceEvent::Repush { pr, .. }
            | TraceEvent::Withdraw { pr, .. }
            | TraceEvent::Blocked { pr, .. }
            | TraceEvent::Eject { pr, .. }
            | TraceEvent::PrCi { pr, .. }
            | TraceEvent::PrCiBegin { pr, .. }
            | TraceEvent::PrCiEnd { pr, .. }
            | TraceEvent::Rebase { pr, .. }
            | TraceEvent::Admit { pr, .. }
            | TraceEvent::Recheck { pr, .. }
            | TraceEvent::Wait { pr, .. } => vec![*pr],
            TraceEvent::Land { members, .. } => members.clone(),
            _ => Vec::new(),
        };
        for pr in prs {
            let life = lives.entry(pr).or_default();
            let fail = |why: &str| Err(format!("pr {pr}: {why} at `{text}`"));
            if let Some((exit, landed)) = &life.exit {
                let run = match event {
                    TraceEvent::PrCiBegin { .. } => {
                        life.begins += 1;
                        true
                    }
                    TraceEvent::PrCiEnd { .. } => {
                        life.ends += 1;
                        true
                    }
                    _ => false,
                };
                if !run || *landed || life.begins > 1 || life.ends > 1 {
                    return fail(&format!("after its exit `{exit}`"));
                }
                continue;
            }
            match event {
                TraceEvent::PrCi { .. } => {
                    life.checked = false;
                    continue;
                }
                TraceEvent::PrCiEnd { cancelled, .. } => {
                    life.checked |= !cancelled;
                    continue;
                }
                TraceEvent::PrCiBegin { .. } => continue,
                _ => {}
            }
            if let Some((expect, after)) = life.expecting.take() {
                let fits = match (expect, event) {
                    (_, TraceEvent::Repush { .. } | TraceEvent::Withdraw { .. }) => true,
                    (Expect::AfterWait, TraceEvent::Rebase { .. }) => true,
                    (Expect::AfterWait, TraceEvent::Blocked { r#final, .. }) => *r#final,
                    _ => false,
                };
                if !fits {
                    return fail(&format!("after `{after}`"));
                }
            }
            if let Some(after) = &life.returning {
                match event {
                    TraceEvent::Rebase { .. } | TraceEvent::Repush { .. } => {}
                    TraceEvent::Admit { .. } => life.returning = None,
                    TraceEvent::Withdraw { .. } | TraceEvent::Blocked { r#final: true, .. } => {}
                    _ => return fail(&format!("before returning from `{after}`")),
                }
            }
            match event {
                TraceEvent::Start { .. } if !life.checked => {
                    return fail("a batch takes an unchecked head");
                }
                TraceEvent::Blocked { r#final: false, .. }
                | TraceEvent::Eject { r#final: false, .. } => life.returning = Some(text.clone()),
                TraceEvent::Rebase { .. } => {
                    life.expecting = Some((Expect::AfterRebase, text.clone()));
                }
                TraceEvent::Wait { .. } => life.expecting = Some((Expect::AfterWait, text.clone())),
                TraceEvent::Blocked { r#final: true, .. }
                | TraceEvent::Eject { r#final: true, .. }
                | TraceEvent::Withdraw { .. } => life.exit = Some((text.clone(), false)),
                TraceEvent::Land { .. } => life.exit = Some((text.clone(), true)),
                _ => {}
            }
        }
    }
    Ok(())
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
            TraceEvent::Blocked {
                t: 3,
                pr: 7,
                on: 3,
                r#final: true,
            },
            TraceEvent::Blocked {
                t: 3,
                pr: 8,
                on: 3,
                r#final: false,
            },
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
                r#final: true,
            },
            TraceEvent::Eject {
                t: 6,
                pr: 6,
                reason: "batch 3 failed".to_owned(),
                justified: false,
                r#final: true,
            },
            TraceEvent::Eject {
                t: 6,
                pr: 9,
                reason: "conflicts with trunk".to_owned(),
                justified: false,
                r#final: false,
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
            TraceEvent::PrCi { t: 7, pr: 7 },
            TraceEvent::PrCiBegin { t: 7, pr: 7 },
            TraceEvent::PrCiEnd {
                t: 8,
                pr: 7,
                cancelled: false,
            },
            TraceEvent::PrCiEnd {
                t: 8,
                pr: 8,
                cancelled: true,
            },
            TraceEvent::Rebase {
                t: 9,
                pr: 9,
                why: Rebase::Conflict,
            },
            TraceEvent::Rebase {
                t: 9,
                pr: 7,
                why: Rebase::BaseLanded,
            },
            TraceEvent::Admit {
                t: 10,
                pr: 9,
                base: None,
            },
            TraceEvent::Admit {
                t: 10,
                pr: 7,
                base: Some(3),
            },
            TraceEvent::Recheck { t: 11, pr: 7 },
            TraceEvent::Wait {
                t: 11,
                pr: 7,
                on: 3,
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
                "blocked 8 with 3",
                "start 2 [1,2] on trunk",
                "start 3 [5] on 2",
                "ci 2 begins",
                "ci 2 passed",
                "ci 3 failed broken flake",
                "land 2 [1,2]",
                "refused 4: #9 is gone",
                "eject 5 justified: batch 3 failed",
                "eject 6 unjustified: batch 3 failed",
                "eject 9 unjustified: conflicts with trunk, rebasing",
                "retire 3",
                "retire 4 cancelled",
                "pr-ci 7",
                "pr-ci 7 begins",
                "pr-ci 7 passed",
                "pr-ci 8 cancelled",
                "rebase 9 conflict",
                "rebase 7 base landed",
                "admit 9",
                "admit 7 on 3",
                "recheck 7",
                "wait 7 on 3",
                "t=6 queued=4 in_flight=1 | land [2] | start 5[6,7] 6[8] | eject 5(batch 3 failed) | retire [3,4]",
            ]
        );
    }

    #[test]
    fn a_life_out_of_order_is_named() {
        let start = TraceEvent::Start {
            t: 1,
            batch: 1,
            base: None,
            members: vec![7],
        };
        let pr_ci = TraceEvent::PrCi { t: 0, pr: 7 };
        let passed = TraceEvent::PrCiEnd {
            t: 1,
            pr: 7,
            cancelled: false,
        };
        let land = TraceEvent::Land {
            t: 2,
            batch: 1,
            members: vec![7],
        };
        let rebase = TraceEvent::Rebase {
            t: 2,
            pr: 7,
            why: Rebase::Conflict,
        };
        let admit = TraceEvent::Admit {
            t: 2,
            pr: 7,
            base: None,
        };
        let ok = [pr_ci.clone(), passed.clone(), start.clone(), land.clone()];
        assert_eq!(check_lives(&ok), Ok(()));
        for (events, why) in [
            (vec![pr_ci.clone(), start.clone()], "unchecked head"),
            (
                vec![pr_ci, passed, start, land, rebase.clone()],
                "after its exit",
            ),
            (vec![rebase, admit], "after `rebase 7 conflict`"),
        ] {
            let error = check_lives(&events).unwrap_err();
            assert!(error.contains(why), "{error}");
        }
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
            "eject",
            "retire",
            "retire",
            "pr_ci",
            "pr_ci_begin",
            "pr_ci_end",
            "pr_ci_end",
            "rebase",
            "rebase",
            "admit",
            "admit",
            "recheck",
            "wait",
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
            serde_json::to_value(&events[6]).unwrap(),
            json!({ "kind": "start", "t": 4, "batch": 2, "base": null, "members": [1, 2] })
        );
        assert_eq!(
            serde_json::to_value(&events[5]).unwrap(),
            json!({ "kind": "blocked", "t": 3, "pr": 8, "on": 3, "final": false })
        );
        assert_eq!(
            serde_json::to_value(&events[15]).unwrap(),
            json!({
                "kind": "eject", "t": 6, "pr": 9, "reason": "conflicts with trunk",
                "justified": false, "final": false
            })
        );
        assert_eq!(
            serde_json::to_value(&events[21]).unwrap(),
            json!({ "kind": "pr_ci_end", "t": 8, "pr": 8, "cancelled": true })
        );
        assert_eq!(
            serde_json::to_value(&events[23]).unwrap(),
            json!({ "kind": "rebase", "t": 9, "pr": 7, "why": "base_landed" })
        );
        assert_eq!(
            serde_json::to_value(&events[25]).unwrap(),
            json!({ "kind": "admit", "t": 10, "pr": 7, "base": 3 })
        );
        assert_eq!(
            serde_json::to_value(&events[27]).unwrap(),
            json!({ "kind": "wait", "t": 11, "pr": 7, "on": 3 })
        );
        assert_eq!(
            serde_json::to_value(&events[28]).unwrap(),
            json!({
                "kind": "step", "t": 6, "queued": 4, "in_flight": 1, "land": [2],
                "start": [{ "batch": 5, "members": [6, 7] }, { "batch": 6, "members": [8] }],
                "eject": [{ "pr": 5, "reason": "batch 3 failed" }],
                "retire": [3, 4]
            })
        );
    }
}
