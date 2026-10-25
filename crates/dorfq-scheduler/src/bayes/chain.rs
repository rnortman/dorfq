//! `speculation = "chain"`: one chain of batches built to land, each on the
//! previous tip, and culprit finders on trunk beside it.

use std::collections::{BTreeMap, BTreeSet};

use dorfq_core::{
    BatchStart, BatchState, CiStatus, Observation, Plan, PrNumber, SchedulerError, Sha,
};

use super::Bayes;
use super::infer::Inference;
use super::policy::{Fate, doomed, landing_members};
use crate::stacks::Stacks;
use crate::{Member, chain};

/// What a candidate batch is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Built on the chain's end, to land; on an empty chain, on trunk.
    Extension,
    /// Built on trunk for its verdict about a suspect.
    Finder,
}

/// A batch the step could start.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    kind: Kind,
    base: Sha,
    members: Vec<Member>,
    score: f64,
}

impl Bayes {
    /// The plan under `speculation = "chain"`, once the observation is
    /// ingested and inferred over.
    pub(super) fn chain_plan(
        &mut self,
        observation: &Observation,
        heads: &BTreeMap<PrNumber, &Sha>,
        inference: &Inference,
    ) -> Result<Plan, SchedulerError> {
        let trunk = &observation.trunk;
        let mut plan = Plan::default();

        // Land one chain from trunk; a passed finder lands only on trunk
        // with no chain batch on trunk, and is otherwise retired.
        let chain_on_trunk = observation.batches.iter().any(|b| {
            !self.finders.contains(&b.id)
                && !doomed(heads, b)
                && b.base == *trunk
                && matches!(
                    b.state,
                    BatchState::Testing {
                        ci: CiStatus::Pending | CiStatus::Passed,
                        ..
                    }
                )
        });
        let mut live: BTreeSet<&Sha> = BTreeSet::new();
        let mut after = trunk;
        let mut fates = Vec::with_capacity(observation.batches.len());
        for batch in &observation.batches {
            let finder = self.finders.contains(&batch.id);
            let base_live = batch.base == *trunk || live.contains(&batch.base);
            let fate = match &batch.state {
                BatchState::Testing { .. } if doomed(heads, batch) => Fate::Retire,
                BatchState::Testing {
                    ci: CiStatus::Pending,
                    ..
                } if finder => Fate::Wait,
                BatchState::Testing {
                    tip,
                    ci: CiStatus::Passed,
                } if finder => {
                    if batch.base == *after
                        && !chain_on_trunk
                        && observation.split_atomic(batch).is_none()
                    {
                        after = tip;
                        plan.land.push(batch.id);
                        Fate::Land
                    } else {
                        Fate::Retire
                    }
                }
                BatchState::Testing {
                    tip,
                    ci: CiStatus::Pending,
                } if base_live => {
                    live.insert(tip);
                    Fate::Wait
                }
                BatchState::Testing {
                    tip,
                    ci: CiStatus::Passed,
                } if base_live => {
                    // An `Atomic` pull request above a member arrived after
                    // the batch started: the members go back to the pool.
                    if batch.base == *after && observation.split_atomic(batch).is_some() {
                        Fate::Retire
                    } else {
                        live.insert(tip);
                        if batch.base == *after {
                            after = tip;
                            plan.land.push(batch.id);
                            Fate::Land
                        } else {
                            Fate::Wait
                        }
                    }
                }
                BatchState::Conflict { pr, with } => Fate::Conflict(*pr, *with),
                BatchState::Testing { .. }
                | BatchState::Landed { .. }
                | BatchState::LandRefused { .. } => Fate::Retire,
            };
            fates.push(fate);
        }

        let ejected = self.eject(observation, inference, &mut fates, &mut plan);
        let (in_flight, pending) = self.settle(observation, &fates, &mut plan);
        if pending >= self.params.lanes {
            return Ok(plan);
        }
        let free = self.params.lanes - pending;
        let mut taken: BTreeSet<PrNumber> = in_flight.union(&ejected).copied().collect();

        // The chain's end: the newest chain tip still standing once this
        // plan's landings run. `below` is its tree beyond trunk, and
        // `p_end` the chance that every pending batch beneath it passes.
        let (chain_end, path) = chain(
            after,
            observation
                .batches
                .iter()
                .zip(&fates)
                .filter(|(batch, fate)| {
                    matches!(fate, Fate::Wait) && !self.finders.contains(&batch.id)
                })
                .map(|(batch, _)| batch),
        );
        let mut below: Vec<Member> = Vec::new();
        let mut p_end = 1.0;
        for batch in path.iter().rev() {
            let members: Vec<Member> = batch
                .members
                .iter()
                .map(|m| (m.pr, m.head.clone()))
                .collect();
            if crate::is_pending(&batch.state) {
                p_end *= self.p_pass(inference, &members, &below);
            }
            below.extend(members);
        }

        // The members beneath the chain end beyond the observation's trunk:
        // the standing path, and the batches this plan lands, which are
        // trunk's by the time the starts run.
        let beneath: BTreeSet<PrNumber> = below
            .iter()
            .map(|m| m.0)
            .chain(landing_members(observation, &fates))
            .collect();

        let stacks = Stacks::new(observation);
        let pool = self.pool(&stacks, heads, &taken);
        if pool.is_empty() {
            return Ok(plan);
        }
        let units = self.units(inference, &stacks, heads, &taken);

        let mut candidates: Vec<Candidate> = Vec::new();
        if let Some((members, score)) = self
            .grow(
                inference,
                &stacks.on(&beneath),
                &units,
                &below,
                p_end,
                &ejected,
            )
            .map_err(SchedulerError::new)?
        {
            candidates.push(Candidate {
                kind: Kind::Extension,
                base: chain_end.clone(),
                members,
                score,
            });
        }
        for members in self.suspects(inference, &stacks, &pool, &BTreeSet::new()) {
            let score = self
                .score(inference, &members, &[], 1.0, &ejected)
                .map_err(SchedulerError::new)?;
            candidates.push(Candidate {
                kind: Kind::Finder,
                base: after.clone(),
                members,
                score,
            });
        }
        candidates.sort_by(|a, b| b.score.total_cmp(&a.score));

        let mut chosen: Vec<&Candidate> = Vec::new();
        for candidate in &candidates {
            if chosen.len() == free {
                break;
            }
            let extends = candidate.kind == Kind::Extension;
            if candidate.score > self.params.speculate_above
                && !(extends && chosen.iter().any(|c| c.kind == Kind::Extension))
                && candidate.members.iter().all(|m| !taken.contains(&m.0))
            {
                taken.extend(candidate.members.iter().map(|m| m.0));
                chosen.push(candidate);
            }
        }
        // Liveness: a free lane with ready work runs the best candidate on
        // trunk even when none is worth its run: the fresh batch on an empty
        // chain, a culprit finder beside a non-empty one. With neither,
        // nothing starts, and the chain drains.
        if chosen.is_empty()
            && let Some(candidate) = candidates.iter().find(|c| c.base == *after)
        {
            chosen.push(candidate);
        }

        for candidate in chosen {
            let id = self.ids.mint();
            if candidate.kind == Kind::Finder {
                self.finders.insert(id);
            }
            let prs = stacks.ordered(candidate.members.iter().map(|m| m.0));
            plan.start
                .push(BatchStart::new(id, candidate.base.clone(), prs));
        }
        Ok(plan)
    }
}
