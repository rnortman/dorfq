//! The structured trace: what happened in a replication, event by event,
//! as the JSON report carries it and `--trace` prints it.

use std::fmt;

use serde::Serialize;

/// Why a pull request's author rebases it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rebase {
    /// It conflicts with trunk.
    Conflict,
    /// Its base landed, under `github` admission.
    BaseLanded,
    /// Its own CI failed on its head: it is broken against trunk, and the
    /// push is its author's fix.
    Broken,
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
    /// A pull request arrived, stacked on `base` or on trunk; `conflicts`
    /// is the hidden truth that it conflicts textually with trunk.
    Arrive {
        t: u64,
        pr: u64,
        base: Option<u64>,
        atomic: bool,
        conflicts: bool,
    },
    /// Hidden truth: the landing of `by` broke the pull request against
    /// trunk.
    Broken { t: u64, pr: u64, by: u64 },
    /// Hidden truth: its author's fix push cleared its breakage; the push
    /// follows as `repush`.
    Fixed { t: u64, pr: u64 },
    /// Hidden truth: the landing of `batch`, through a run that missed a
    /// break, put a bad pull request on trunk; every run on trunk fails
    /// until the repair.
    TrunkBroken { t: u64, batch: u64 },
    /// Hidden truth: a push repaired trunk in place, ending one break.
    TrunkRepaired { t: u64 },
    /// A head changed, intent kept.
    Repush { t: u64, pr: u64 },
    /// Intent cleared by the world's churn.
    Withdraw { t: u64, pr: u64 },
    /// Out of the queue with its base `on`: for good when `final`, its base
    /// having left without landing; otherwise its base is out for a rebase
    /// and it returns, traced `admit`, at the base's push. `bad` when it was
    /// bad against trunk at that instant, its base's badness included.
    Blocked {
        t: u64,
        pr: u64,
        on: u64,
        r#final: bool,
        bad: bool,
    },
    /// A batch started; `base` is the batch whose tip it runs on, `None`
    /// when that is trunk at the moment of the start.
    Start {
        t: u64,
        batch: u64,
        base: Option<u64>,
        members: Vec<u64>,
    },
    /// The run's verdict; `broken` when its tree fails for its content,
    /// `flake` when the flake draw fired, `outage` when the run touched a
    /// CI outage.
    RunEnd {
        t: u64,
        batch: u64,
        passed: bool,
        broken: bool,
        flake: bool,
        outage: bool,
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
    /// A batch was retired; `cancelled` when its run was still running.
    Retire { t: u64, batch: u64, cancelled: bool },
    /// A pull request's own CI run started on its current head; `billed`
    /// when the queue caused it by landing the pull request's direct base
    /// without it.
    PrCi { t: u64, pr: u64, billed: bool },
    /// The pull request's own CI run ended: passed, `failed` with its head
    /// broken against trunk, or `cancelled` by a head change.
    PrCiEnd {
        t: u64,
        pr: u64,
        cancelled: bool,
        failed: bool,
    },
    /// The pull request's author starts rebasing it, for `why`; the push
    /// follows as `repush`.
    Rebase { t: u64, pr: u64, why: Rebase },
    /// Its own CI failed for trunk's or an outage's reason, not its own: its
    /// author will re-run it on the same head once no outage holds and trunk
    /// is green. It is held meanwhile — in the queue under `label`, out
    /// checking under `github` — until the re-run's `pr_ci`, or a push,
    /// a `rebase`, an `eject`, a `blocked` or a withdrawal ends the wait.
    Rerun { t: u64, pr: u64 },
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
            TraceEvent::Broken { pr, by, .. } => write!(f, "broken {pr} by {by}"),
            TraceEvent::Fixed { pr, .. } => write!(f, "fixed {pr}"),
            TraceEvent::TrunkBroken { batch, .. } => write!(f, "trunk broken by {batch}"),
            TraceEvent::TrunkRepaired { .. } => write!(f, "trunk repaired"),
            TraceEvent::Repush { pr, .. } => write!(f, "repush {pr}"),
            TraceEvent::Withdraw { pr, .. } => write!(f, "withdraw {pr}"),
            TraceEvent::Blocked {
                pr,
                on,
                r#final,
                bad,
                ..
            } => {
                let how = if *r#final { "on" } else { "with" };
                write!(f, "blocked {pr} {how} {on}")?;
                if *bad {
                    write!(f, " bad")?;
                }
                Ok(())
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
            TraceEvent::RunEnd {
                batch,
                passed,
                broken,
                flake,
                outage,
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
                if *outage {
                    write!(f, " outage")?;
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
            TraceEvent::PrCi { pr, billed, .. } => {
                write!(f, "pr-ci {pr}")?;
                if *billed {
                    write!(f, " billed")?;
                }
                Ok(())
            }
            TraceEvent::PrCiEnd {
                pr,
                cancelled,
                failed,
                ..
            } => {
                let end = match (cancelled, failed) {
                    (true, _) => "cancelled",
                    (false, true) => "failed",
                    (false, false) => "passed",
                };
                write!(f, "pr-ci {pr} {end}")
            }
            TraceEvent::Rebase { pr, why, .. } => match why {
                Rebase::Conflict => write!(f, "rebase {pr} conflict"),
                Rebase::BaseLanded => write!(f, "rebase {pr} base landed"),
                Rebase::Broken => write!(f, "rebase {pr} broken"),
            },
            TraceEvent::Rerun { pr, .. } => write!(f, "re-run {pr}"),
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
/// `rebase` for a conflict or a landed base comes its push or a withdrawal;
/// after a `rebase` for breakage, its fix push or whatever ends the pending
/// fix: a push re-heading it, another `rebase`, an `eject`, a `blocked` or a
/// withdrawal; after a `rerun`, the run that is its re-run (a `pr_ci` while
/// it is not broken against trunk) or whatever ends the pending re-run: a
/// push re-heading it, a `rebase`, an `eject`, a `blocked` or a withdrawal;
/// after a `wait` a rebase, a
/// push, an `admit`, a final `blocked` or a withdrawal; after a final exit that is not
/// a landing at most one `pr_ci_end`, its last head's run's; after a
/// landing one `pr_ci_end` if a billed run was in flight at the landing,
/// and nothing else; and no `start` names it unless its current head, from
/// its `arrive` or latest `repush`, has passed its own CI (a failed run does
/// not count).
#[cfg(test)]
pub(crate) fn check_lives(events: &[TraceEvent]) -> Result<(), String> {
    use std::collections::BTreeMap;

    /// What the pending event allows next: a `rebase` for a conflict or a
    /// landed base, one for breakage, a `rerun`, or a `wait`.
    enum Expect {
        Rebase,
        Fix,
        Rerun,
        Wait,
    }

    #[derive(Default)]
    struct Life {
        /// Its current head has passed its own CI.
        checked: bool,
        /// Broken against trunk, by a `broken` not yet followed by its
        /// `fixed`.
        bad: bool,
        /// Out with a return pending, since this event.
        returning: Option<String>,
        /// The last `rebase` or `wait` and its text, until the next event
        /// of its own.
        expecting: Option<(Expect, String)>,
        /// A billed run is in flight: requested and not yet ended.
        billed: bool,
        /// Left for good, by this event, and whether by landing; `ends`
        /// counts its run's ends since.
        exit: Option<(String, bool)>,
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
            | TraceEvent::PrCiEnd { pr, .. }
            | TraceEvent::Rebase { pr, .. }
            | TraceEvent::Rerun { pr, .. }
            | TraceEvent::Admit { pr, .. }
            | TraceEvent::Recheck { pr, .. }
            | TraceEvent::Wait { pr, .. }
            | TraceEvent::Broken { pr, .. }
            | TraceEvent::Fixed { pr, .. } => vec![*pr],
            TraceEvent::Land { members, .. } => members.clone(),
            _ => Vec::new(),
        };
        for pr in prs {
            let life = lives.entry(pr).or_default();
            let fail = |why: &str| Err(format!("pr {pr}: {why} at `{text}`"));
            match event {
                TraceEvent::Broken { .. } => {
                    life.bad = true;
                    continue;
                }
                TraceEvent::Fixed { .. } => {
                    life.bad = false;
                    continue;
                }
                _ => {}
            }
            if let Some((exit, landed)) = &life.exit {
                let allowed = match event {
                    TraceEvent::PrCiEnd { .. } if *landed => std::mem::take(&mut life.billed),
                    TraceEvent::PrCiEnd { .. } => {
                        life.ends += 1;
                        life.ends == 1
                    }
                    _ => false,
                };
                if !allowed {
                    return fail(&format!("after its exit `{exit}`"));
                }
                continue;
            }
            match event {
                TraceEvent::PrCi { billed, .. } => {
                    if !life.bad && matches!(life.expecting, Some((Expect::Rerun, _))) {
                        life.expecting = None;
                    }
                    life.billed = *billed;
                    continue;
                }
                TraceEvent::PrCiEnd {
                    cancelled, failed, ..
                } => {
                    life.checked |= !cancelled && !failed;
                    life.billed = false;
                    continue;
                }
                _ => {}
            }
            if let Some((expect, after)) = life.expecting.take() {
                let fits = match (expect, event) {
                    (_, TraceEvent::Repush { .. } | TraceEvent::Withdraw { .. }) => true,
                    (Expect::Wait, TraceEvent::Rebase { .. } | TraceEvent::Admit { .. }) => true,
                    (Expect::Wait, TraceEvent::Blocked { r#final, .. }) => *r#final,
                    (
                        Expect::Fix | Expect::Rerun,
                        TraceEvent::Rebase { .. }
                        | TraceEvent::Eject { .. }
                        | TraceEvent::Blocked { .. },
                    ) => true,
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
                TraceEvent::Arrive { .. } | TraceEvent::Repush { .. } => life.checked = false,
                TraceEvent::Start { .. } if !life.checked => {
                    return fail("a batch takes an unchecked head");
                }
                TraceEvent::Blocked { r#final: false, .. }
                | TraceEvent::Eject { r#final: false, .. } => life.returning = Some(text.clone()),
                TraceEvent::Rebase {
                    why: Rebase::Broken,
                    ..
                } => life.expecting = Some((Expect::Fix, text.clone())),
                TraceEvent::Rerun { .. } => life.expecting = Some((Expect::Rerun, text.clone())),
                TraceEvent::Rebase { .. } => {
                    life.expecting = Some((Expect::Rebase, text.clone()));
                }
                TraceEvent::Wait { .. } => life.expecting = Some((Expect::Wait, text.clone())),
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
                conflicts: false,
            },
            TraceEvent::Arrive {
                t: 1,
                pr: 8,
                base: None,
                atomic: false,
                conflicts: true,
            },
            TraceEvent::Repush { t: 2, pr: 7 },
            TraceEvent::Withdraw { t: 3, pr: 3 },
            TraceEvent::Blocked {
                t: 3,
                pr: 7,
                on: 3,
                r#final: true,
                bad: true,
            },
            TraceEvent::Blocked {
                t: 3,
                pr: 8,
                on: 3,
                r#final: false,
                bad: false,
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
            TraceEvent::RunEnd {
                t: 5,
                batch: 2,
                passed: true,
                broken: false,
                flake: false,
                outage: false,
            },
            TraceEvent::RunEnd {
                t: 5,
                batch: 3,
                passed: false,
                broken: true,
                flake: true,
                outage: false,
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
            TraceEvent::PrCi {
                t: 7,
                pr: 7,
                billed: false,
            },
            TraceEvent::PrCi {
                t: 7,
                pr: 7,
                billed: true,
            },
            TraceEvent::PrCiEnd {
                t: 8,
                pr: 7,
                cancelled: false,
                failed: false,
            },
            TraceEvent::PrCiEnd {
                t: 8,
                pr: 8,
                cancelled: true,
                failed: false,
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
            TraceEvent::Broken {
                t: 12,
                pr: 7,
                by: 2,
            },
            TraceEvent::PrCiEnd {
                t: 13,
                pr: 7,
                cancelled: false,
                failed: true,
            },
            TraceEvent::Rebase {
                t: 13,
                pr: 7,
                why: Rebase::Broken,
            },
            TraceEvent::Fixed { t: 14, pr: 7 },
            TraceEvent::TrunkBroken { t: 15, batch: 2 },
            TraceEvent::TrunkRepaired { t: 16 },
            TraceEvent::RunEnd {
                t: 17,
                batch: 4,
                passed: false,
                broken: false,
                flake: false,
                outage: true,
            },
            TraceEvent::Rerun { t: 18, pr: 7 },
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
                "blocked 7 on 3 bad",
                "blocked 8 with 3",
                "start 2 [1,2] on trunk",
                "start 3 [5] on 2",
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
                "pr-ci 7 billed",
                "pr-ci 7 passed",
                "pr-ci 8 cancelled",
                "rebase 9 conflict",
                "rebase 7 base landed",
                "admit 9",
                "admit 7 on 3",
                "recheck 7",
                "wait 7 on 3",
                "t=6 queued=4 in_flight=1 | land [2] | start 5[6,7] 6[8] | eject 5(batch 3 failed) | retire [3,4]",
                "broken 7 by 2",
                "pr-ci 7 failed",
                "rebase 7 broken",
                "fixed 7",
                "trunk broken by 2",
                "trunk repaired",
                "ci 4 failed outage",
                "re-run 7",
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
        let pr_ci = TraceEvent::PrCi {
            t: 0,
            pr: 7,
            billed: false,
        };
        let passed = TraceEvent::PrCiEnd {
            t: 1,
            pr: 7,
            cancelled: false,
            failed: false,
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
        // A billed re-run on the head that passed leaves it checked.
        let rerun = TraceEvent::PrCi {
            t: 1,
            pr: 7,
            billed: true,
        };
        let same_head = [pr_ci.clone(), passed.clone(), rerun, start.clone()];
        assert_eq!(check_lives(&same_head), Ok(()));
        let repush = TraceEvent::Repush { t: 1, pr: 7 };
        for (events, why) in [
            (vec![pr_ci.clone(), start.clone()], "unchecked head"),
            (
                vec![pr_ci.clone(), passed.clone(), repush, start.clone()],
                "unchecked head",
            ),
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
    fn after_a_landing_only_the_end_of_a_billed_run_in_flight_is_admitted() {
        let pr_ci = |billed| TraceEvent::PrCi {
            t: 0,
            pr: 7,
            billed,
        };
        let end = TraceEvent::PrCiEnd {
            t: 3,
            pr: 7,
            cancelled: false,
            failed: false,
        };
        let start = TraceEvent::Start {
            t: 1,
            batch: 1,
            base: None,
            members: vec![7],
        };
        let land = TraceEvent::Land {
            t: 2,
            batch: 1,
            members: vec![7],
        };
        let checked = [pr_ci(false), end.clone(), start];
        let mut ok = checked.to_vec();
        ok.extend([pr_ci(true), land.clone(), end.clone()]);
        assert_eq!(check_lives(&ok), Ok(()));
        let repush = TraceEvent::Repush { t: 3, pr: 7 };
        for tail in [
            vec![land.clone(), end.clone()],
            vec![pr_ci(false), land.clone(), end.clone()],
            vec![pr_ci(true), land.clone(), end.clone(), end.clone()],
            vec![pr_ci(true), land.clone(), repush],
        ] {
            let mut events = checked.to_vec();
            events.extend(tail);
            let error = check_lives(&events).unwrap_err();
            assert!(error.contains("after its exit `land 1 [7]`"), "{error}");
        }
    }

    #[test]
    fn a_failed_own_run_leaves_its_head_unchecked() {
        let pr_ci = TraceEvent::PrCi {
            t: 0,
            pr: 7,
            billed: false,
        };
        let failed = TraceEvent::PrCiEnd {
            t: 1,
            pr: 7,
            cancelled: false,
            failed: true,
        };
        let start = TraceEvent::Start {
            t: 2,
            batch: 1,
            base: None,
            members: vec![7],
        };
        let error = check_lives(&[pr_ci, failed, start]).unwrap_err();
        assert!(error.contains("unchecked head"), "{error}");
    }

    #[test]
    fn a_pending_fix_ends_only_as_the_fix_says() {
        let rebase = |why| TraceEvent::Rebase { t: 1, pr: 7, why };
        let eject = |r#final| TraceEvent::Eject {
            t: 2,
            pr: 7,
            reason: "batch 3 failed".to_owned(),
            justified: true,
            r#final,
        };
        let blocked = |r#final| TraceEvent::Blocked {
            t: 2,
            pr: 7,
            on: 3,
            r#final,
            bad: false,
        };
        let repush = TraceEvent::Repush { t: 2, pr: 7 };
        let withdraw = TraceEvent::Withdraw { t: 2, pr: 7 };
        let fixed = TraceEvent::Fixed { t: 2, pr: 7 };
        for next in [
            vec![fixed, repush.clone()],
            vec![repush.clone()],
            vec![rebase(Rebase::BaseLanded), repush.clone()],
            vec![rebase(Rebase::Conflict), repush.clone()],
            vec![withdraw],
            vec![eject(true)],
            vec![eject(false)],
            vec![blocked(true)],
            vec![blocked(false)],
        ] {
            let mut events = vec![rebase(Rebase::Broken)];
            events.extend(next);
            assert_eq!(check_lives(&events), Ok(()), "{events:?}");
        }
        let admit = TraceEvent::Admit {
            t: 2,
            pr: 7,
            base: None,
        };
        for (events, after) in [
            (
                vec![rebase(Rebase::Broken), admit],
                "after `rebase 7 broken`",
            ),
            (
                vec![rebase(Rebase::Conflict), eject(true)],
                "after `rebase 7 conflict`",
            ),
            (
                vec![
                    rebase(Rebase::Broken),
                    rebase(Rebase::BaseLanded),
                    eject(true),
                ],
                "after `rebase 7 base landed`",
            ),
        ] {
            let error = check_lives(&events).unwrap_err();
            assert!(error.contains(after), "{error}");
        }
    }

    #[test]
    fn a_pending_re_run_ends_only_as_the_re_run_says() {
        let rerun = TraceEvent::Rerun { t: 1, pr: 7 };
        let rebase = |why| TraceEvent::Rebase { t: 2, pr: 7, why };
        let pr_ci = |billed| TraceEvent::PrCi {
            t: 2,
            pr: 7,
            billed,
        };
        let eject = |r#final| TraceEvent::Eject {
            t: 2,
            pr: 7,
            reason: "batch 3 failed".to_owned(),
            justified: true,
            r#final,
        };
        let blocked = |r#final| TraceEvent::Blocked {
            t: 2,
            pr: 7,
            on: 3,
            r#final,
            bad: false,
        };
        let repush = TraceEvent::Repush { t: 2, pr: 7 };
        let withdraw = TraceEvent::Withdraw { t: 2, pr: 7 };
        let broken = TraceEvent::Broken { t: 2, pr: 7, by: 3 };
        let fixed = TraceEvent::Fixed { t: 2, pr: 7 };
        let admit = TraceEvent::Admit {
            t: 3,
            pr: 7,
            base: None,
        };
        let wait = TraceEvent::Wait { t: 3, pr: 7, on: 3 };
        let start = TraceEvent::Start {
            t: 3,
            batch: 1,
            base: None,
            members: vec![7],
        };
        for next in [
            vec![pr_ci(false), admit.clone()],
            vec![pr_ci(true), admit.clone()],
            vec![repush.clone()],
            vec![rebase(Rebase::BaseLanded), repush.clone()],
            vec![rebase(Rebase::Conflict), repush.clone()],
            vec![rebase(Rebase::Broken), fixed.clone(), repush.clone()],
            vec![withdraw],
            vec![eject(true)],
            vec![eject(false)],
            vec![blocked(true)],
            vec![blocked(false)],
            // The landing that broke it turns the wait into a fix push.
            vec![
                broken.clone(),
                pr_ci(true),
                rebase(Rebase::Broken),
                fixed.clone(),
                repush.clone(),
            ],
            // Fixed meanwhile, the next run is the re-run again.
            vec![broken.clone(), fixed, pr_ci(false), admit.clone()],
        ] {
            let mut events = vec![rerun.clone()];
            events.extend(next);
            assert_eq!(check_lives(&events), Ok(()), "{events:?}");
        }
        for (events, after) in [
            (vec![rerun.clone(), admit.clone()], "after `re-run 7`"),
            (vec![rerun.clone(), wait], "after `re-run 7`"),
            (vec![rerun.clone(), start], "after `re-run 7`"),
            (
                vec![rerun, broken, pr_ci(true), admit.clone()],
                "after `re-run 7`",
            ),
            (
                vec![rebase(Rebase::Broken), pr_ci(false), admit],
                "after `rebase 7 broken`",
            ),
        ] {
            let error = check_lives(&events).unwrap_err();
            assert!(error.contains(after), "{error}");
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
            "pr_ci",
            "pr_ci_end",
            "pr_ci_end",
            "rebase",
            "rebase",
            "admit",
            "admit",
            "recheck",
            "wait",
            "step",
            "broken",
            "pr_ci_end",
            "rebase",
            "fixed",
            "trunk_broken",
            "trunk_repaired",
            "run_end",
            "rerun",
        ];
        assert_eq!(kinds, expected.map(|k| json!(k)));
        let events = every_kind();
        assert_eq!(
            serde_json::to_value(&events[0]).unwrap(),
            json!({
                "kind": "arrive", "t": 1, "pr": 7, "base": 3, "atomic": true,
                "conflicts": false
            })
        );
        assert_eq!(
            serde_json::to_value(&events[6]).unwrap(),
            json!({ "kind": "start", "t": 4, "batch": 2, "base": null, "members": [1, 2] })
        );
        assert_eq!(
            serde_json::to_value(&events[5]).unwrap(),
            json!({ "kind": "blocked", "t": 3, "pr": 8, "on": 3, "final": false, "bad": false })
        );
        assert_eq!(
            serde_json::to_value(&events[14]).unwrap(),
            json!({
                "kind": "eject", "t": 6, "pr": 9, "reason": "conflicts with trunk",
                "justified": false, "final": false
            })
        );
        assert_eq!(
            serde_json::to_value(&events[17]).unwrap(),
            json!({ "kind": "pr_ci", "t": 7, "pr": 7, "billed": false })
        );
        assert_eq!(
            serde_json::to_value(&events[18]).unwrap(),
            json!({ "kind": "pr_ci", "t": 7, "pr": 7, "billed": true })
        );
        assert_eq!(
            serde_json::to_value(&events[20]).unwrap(),
            json!({ "kind": "pr_ci_end", "t": 8, "pr": 8, "cancelled": true, "failed": false })
        );
        assert_eq!(
            serde_json::to_value(&events[22]).unwrap(),
            json!({ "kind": "rebase", "t": 9, "pr": 7, "why": "base_landed" })
        );
        assert_eq!(
            serde_json::to_value(&events[24]).unwrap(),
            json!({ "kind": "admit", "t": 10, "pr": 7, "base": 3 })
        );
        assert_eq!(
            serde_json::to_value(&events[26]).unwrap(),
            json!({ "kind": "wait", "t": 11, "pr": 7, "on": 3 })
        );
        assert_eq!(
            serde_json::to_value(&events[27]).unwrap(),
            json!({
                "kind": "step", "t": 6, "queued": 4, "in_flight": 1, "land": [2],
                "start": [{ "batch": 5, "members": [6, 7] }, { "batch": 6, "members": [8] }],
                "eject": [{ "pr": 5, "reason": "batch 3 failed" }],
                "retire": [3, 4]
            })
        );
        assert_eq!(
            serde_json::to_value(&events[28]).unwrap(),
            json!({ "kind": "broken", "t": 12, "pr": 7, "by": 2 })
        );
        assert_eq!(
            serde_json::to_value(&events[29]).unwrap(),
            json!({ "kind": "pr_ci_end", "t": 13, "pr": 7, "cancelled": false, "failed": true })
        );
        assert_eq!(
            serde_json::to_value(&events[30]).unwrap(),
            json!({ "kind": "rebase", "t": 13, "pr": 7, "why": "broken" })
        );
        assert_eq!(
            serde_json::to_value(&events[31]).unwrap(),
            json!({ "kind": "fixed", "t": 14, "pr": 7 })
        );
        assert_eq!(
            serde_json::to_value(&events[32]).unwrap(),
            json!({ "kind": "trunk_broken", "t": 15, "batch": 2 })
        );
        assert_eq!(
            serde_json::to_value(&events[33]).unwrap(),
            json!({ "kind": "trunk_repaired", "t": 16 })
        );
        assert_eq!(
            serde_json::to_value(&events[34]).unwrap(),
            json!({
                "kind": "run_end", "t": 17, "batch": 4, "passed": false, "broken": false,
                "flake": false, "outage": true
            })
        );
        assert_eq!(
            serde_json::to_value(&events[35]).unwrap(),
            json!({ "kind": "rerun", "t": 18, "pr": 7 })
        );
    }
}
