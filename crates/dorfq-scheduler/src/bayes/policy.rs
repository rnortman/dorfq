//! Each step of the Bayesian scheduler: ingest, eject, retire, land, start.

use std::collections::{BTreeMap, BTreeSet};

use dorfq_core::{
    BatchId, BatchStart, BatchState, BatchStatus, CiStatus, ConflictWith, Departed, EjectReason,
    Observation, Plan, PrNumber, Scheduler, SchedulerError, Sha,
};

use super::Bayes;
use super::infer::{Inference, Unresolved};
use super::model::Cause;
use crate::stacks::{Footing, Stacks};
use crate::{Ejections, Member, chain, is_pending};

/// What this step does with one tracked batch.
enum Fate {
    /// Landed by this plan.
    Land,
    /// Left running, or waiting for its base to land.
    Wait,
    /// Conflicted while building.
    Conflict(PrNumber, ConflictWith),
    /// Read, dead, landed or refused.
    Retire,
}

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

/// For each cause some run singles out, the last such run in `runs`: the
/// cause's posterior is at least `eject_above` and every other cause of the
/// run is below `others_below`.
fn singled_out<'a>(
    runs: impl Iterator<Item = (Option<BatchId>, &'a BTreeSet<Cause>)>,
    posterior: impl Fn(Cause) -> f64,
    eject_above: f64,
    others_below: f64,
) -> BTreeMap<Cause, Option<BatchId>> {
    let mut out = BTreeMap::new();
    for (batch, causes) in runs {
        let mut suspects = causes
            .iter()
            .copied()
            .filter(|&cause| posterior(cause) >= others_below);
        if let (Some(cause), None) = (suspects.next(), suspects.next())
            && posterior(cause) >= eject_above
        {
            out.insert(cause, batch);
        }
    }
    out
}

/// The pull request ejected for a singled-out cause.
fn culprit(cause: Cause) -> PrNumber {
    match cause {
        Cause::Bad(pr) => pr,
        Cause::Pair(_, newer) => newer,
    }
}

/// Why the [`culprit`] of a cause `batch` singled out is ejected.
fn culprit_reason(cause: Cause, batch: BatchId) -> EjectReason {
    match cause {
        Cause::Bad(_) => EjectReason::BatchFailed { batch },
        Cause::Pair(older, _) => EjectReason::InteractsWith { pr: older, batch },
    }
}

impl Bayes {
    /// Takes in what the observation says: each new batch's tree, the
    /// landings, the queued pull requests, each new verdict and each new
    /// landing's decay. `heads` is each queued pull request's head.
    fn ingest(&mut self, observation: &Observation, heads: &BTreeMap<PrNumber, &Sha>) {
        let tracked: BTreeSet<BatchId> = observation.batches.iter().map(|b| b.id).collect();
        self.finders.retain(|id| tracked.contains(id));
        self.read.retain(|id| tracked.contains(id));
        self.landings.retain(|id| tracked.contains(id));
        self.trees.retain(|id, _| tracked.contains(id));

        let tips: BTreeMap<&Sha, &BatchStatus> = observation
            .batches
            .iter()
            .filter_map(|b| b.state.tip().map(|tip| (tip, b)))
            .collect();
        for batch in &observation.batches {
            self.trees.entry(batch.id).or_insert_with(|| {
                let mut tree: Vec<Member> = Vec::new();
                let mut node = Some(batch);
                while let Some(b) = node {
                    tree.extend(b.members.iter().map(|m| (m.pr, m.head.clone())));
                    // A landed tip is in trunk, and so is everything below it.
                    node = if b.base == observation.trunk {
                        None
                    } else {
                        tips.get(&b.base)
                            .copied()
                            .filter(|p| !matches!(p.state, BatchState::Landed { .. }))
                    };
                }
                tree
            });
        }

        // What reached trunk, landed by the queue or merged by another hand,
        // is trunk now: it leaves every tree, and its causes leave the runs it
        // was in while the runs stay.
        let landed: BTreeSet<Member> = observation
            .departed
            .iter()
            .filter(|d| match d.how {
                Departed::Landed { .. } | Departed::Merged => true,
                Departed::Ejected | Departed::Withdrawn(_) => false,
            })
            .map(|d| (d.pr, d.head.clone()))
            .collect();
        for tree in self.trees.values_mut() {
            tree.retain(|member| !landed.contains(member));
        }
        self.model
            .sync(&observation.prs, &landed.iter().map(|m| m.0).collect());

        for batch in &observation.batches {
            match &batch.state {
                BatchState::Testing {
                    ci: CiStatus::Passed,
                    ..
                } if self.read.insert(batch.id) => self.model.passed(&self.trees[&batch.id]),
                // A failure over a head that has gone teaches nothing.
                BatchState::Testing {
                    ci: CiStatus::Failed(_),
                    ..
                } if self.read.insert(batch.id) => {
                    let tree = &self.trees[&batch.id];
                    if tree.iter().all(|(pr, head)| heads.get(pr) == Some(&head)) {
                        self.model.failed(batch.id, tree);
                    }
                }
                BatchState::Landed { .. } if self.landings.insert(batch.id) => {
                    self.model
                        .decay(&batch.members.iter().map(|m| m.pr).collect());
                }
                BatchState::Testing { .. }
                | BatchState::Landed { .. }
                | BatchState::Conflict { .. }
                | BatchState::LandRefused { .. } => {}
            }
        }
    }

    /// The probability that a batch of `members` passes on a node whose tree
    /// beyond trunk is `below`.
    fn p_pass(&self, inference: &Inference, members: &[Member], below: &[Member]) -> f64 {
        let posterior = |cause| inference.posterior(&self.model, cause);
        let mut p = 1.0 - self.params.p_flake;
        for (n, &(i, _)) in members.iter().enumerate() {
            p *= 1.0 - posterior(Cause::Bad(i));
            for &(j, _) in members[n + 1..].iter().chain(below) {
                p *= 1.0 - posterior(Cause::pair(i, j));
            }
        }
        p
    }

    /// The resolution value of a batch of `members` on a node whose tree
    /// beyond trunk is `below`: `w_reject` per pull request its failure
    /// would single out, beyond those `ejected` already.
    fn resolution(
        &self,
        inference: &Inference,
        members: &[Member],
        below: &[Member],
        ejected: &BTreeSet<PrNumber>,
    ) -> Result<f64, Unresolved> {
        let tree: Vec<Member> = members.iter().chain(below).cloned().collect();
        let extra = self.model.would_blame(&tree);
        if extra.is_empty() {
            return Ok(0.0);
        }
        let (runs, posterior) = inference.with_failure(&self.model, &extra)?;
        let evidence = self.model.evidence();
        let existing = runs
            .iter()
            .map(|&r| (Some(evidence[r].batch), &inference.effective()[r]));
        let resolved: BTreeSet<PrNumber> = singled_out(
            existing.chain([(None, &extra)]),
            |cause| {
                posterior
                    .get(&cause)
                    .copied()
                    .unwrap_or_else(|| self.model.belief(cause))
            },
            self.params.eject_above,
            self.params.others_below,
        )
        .into_keys()
        .map(culprit)
        .filter(|pr| !ejected.contains(pr))
        .collect();
        Ok(self.params.w_reject * resolved.len() as f64)
    }

    /// `P(n) · EV(B, n)` for a batch of `members` on a node whose tree beyond
    /// trunk is `below` and which stands with probability `p_node`.
    fn score(
        &self,
        inference: &Inference,
        members: &[Member],
        below: &[Member],
        p_node: f64,
        ejected: &BTreeSet<PrNumber>,
    ) -> Result<f64, Unresolved> {
        let p = self.p_pass(inference, members, below);
        let resolution = self.resolution(inference, members, below, ejected)?;
        let ev = p * members.len() as f64 + (1.0 - p) * resolution - self.params.run_cost;
        Ok(p_node * ev)
    }

    /// Grows a batch on a node from `units`, in order, while its score
    /// rises. A unit joins whole, and only when every member's base is
    /// trunk, an earlier member, or beneath the node in `footing`; a unit
    /// that would take the batch past `max_batch` is skipped unless the
    /// batch is empty, so an `Atomic` stack larger than the cap runs alone.
    fn grow(
        &self,
        inference: &Inference,
        footing: &Footing,
        units: &[Vec<Member>],
        below: &[Member],
        p_node: f64,
        ejected: &BTreeSet<PrNumber>,
    ) -> Result<Option<(Vec<Member>, f64)>, Unresolved> {
        let mut members: Vec<Member> = Vec::new();
        let mut chosen: BTreeSet<PrNumber> = BTreeSet::new();
        let mut best = f64::NEG_INFINITY;
        for unit in units {
            if !members.is_empty() && members.len() + unit.len() > self.params.max_batch {
                continue;
            }
            let Some(with) = footing.extend(&chosen, unit.iter().map(|m| m.0)) else {
                continue;
            };
            members.extend(unit.iter().cloned());
            let score = self.score(inference, &members, below, p_node, ejected)?;
            if score > best {
                best = score;
                chosen = with;
            } else {
                members.truncate(members.len() - unit.len());
                break;
            }
        }
        Ok((!members.is_empty()).then_some((members, best)))
    }

    /// The culprit finders: per component, a batch for each of its three
    /// most probable causes, and one of its two most probable `Bad` suspects
    /// together. A finder runs on trunk, so it holds its suspects' closures:
    /// each suspect with every enqueued ancestor, bases first. Only finders
    /// whose members are all batchable and in `pool` are generated.
    fn finders(&self, inference: &Inference, stacks: &Stacks, pool: &[Member]) -> Vec<Vec<Member>> {
        let available: BTreeMap<PrNumber, &Sha> = pool.iter().map(|(pr, h)| (*pr, h)).collect();
        let closure = |suspects: &[PrNumber]| -> Option<Vec<PrNumber>> {
            let mut all: BTreeSet<PrNumber> = BTreeSet::new();
            for &pr in suspects {
                all.extend(stacks.closure(pr)?);
            }
            Some(stacks.ordered(all))
        };
        let mut out: Vec<Vec<Member>> = Vec::new();
        for runs in inference.components() {
            let causes: BTreeSet<Cause> = runs
                .iter()
                .flat_map(|&r| inference.effective()[r].iter().copied())
                .collect();
            let mut ranked: Vec<(f64, Cause)> = causes
                .into_iter()
                .map(|cause| (inference.posterior(&self.model, cause), cause))
                .collect();
            ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            // TODO(bayes-pair-search): a materialised set of cross pairs is
            // searched one pair per finder, in rank order; cross-subset
            // finders would locate the failing pair in logarithmically many
            // runs.
            // TODO(trunk-broken-probe): every finder runs on trunk; after a
            // flaky pass landed a bad pull request, every one of them fails
            // and each singles out an innocent suspect.
            let mut sets: Vec<Vec<PrNumber>> = ranked
                .iter()
                .take(3)
                .map(|&(_, cause)| match cause {
                    Cause::Bad(i) => vec![i],
                    Cause::Pair(i, j) => vec![i, j],
                })
                .collect();
            let bads: Vec<PrNumber> = ranked
                .iter()
                .filter_map(|&(_, cause)| match cause {
                    Cause::Bad(i) => Some(i),
                    Cause::Pair(..) => None,
                })
                .take(2)
                .collect();
            if let [a, b] = bads[..] {
                sets.push(vec![a, b]);
            }
            for set in sets {
                let members: Option<Vec<Member>> = closure(&set).and_then(|set| {
                    set.iter()
                        .map(|pr| available.get(pr).map(|&head| (*pr, head.clone())))
                        .collect()
                });
                if let Some(members) = members
                    && !out.contains(&members)
                {
                    out.push(members);
                }
            }
        }
        out
    }
}

impl Scheduler for Bayes {
    fn step(&mut self, observation: &Observation) -> Result<Plan, SchedulerError> {
        self.ids.raise(observation);
        let heads: BTreeMap<PrNumber, &Sha> = observation
            .prs
            .iter()
            .map(|p| (p.number, &p.head))
            .collect();
        self.ingest(observation, &heads);
        let inference = Inference::of(&self.model).map_err(SchedulerError::new)?;
        let trunk = &observation.trunk;
        let mut plan = Plan::default();

        // A batch holding a member no longer queued at the head it was built
        // with can never land: it is retired, never landed, and never a
        // live tip, so nothing stands on it.
        let doomed = |b: &BatchStatus| b.members.iter().any(|m| heads.get(&m.pr) != Some(&&m.head));
        // Land one chain from trunk; a passed finder lands only on trunk
        // with no chain batch on trunk, and is otherwise retired.
        let chain_on_trunk = observation.batches.iter().any(|b| {
            !self.finders.contains(&b.id)
                && !doomed(b)
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
                BatchState::Testing { .. } if doomed(batch) => Fate::Retire,
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

        let mut ejections = Ejections::new(observation, &mut plan.eject);
        for (batch, fate) in observation.batches.iter().zip(&fates) {
            if matches!(fate, Fate::Land) {
                ejections.landing(batch);
            }
        }
        let evidence = self.model.evidence();
        let runs = inference
            .effective()
            .iter()
            .enumerate()
            .map(|(r, causes)| (Some(evidence[r].batch), causes));
        let singled = singled_out(
            runs,
            |cause| inference.posterior(&self.model, cause),
            self.params.eject_above,
            self.params.others_below,
        );
        for (cause, batch) in singled {
            let batch = batch.expect("every existing run has a batch");
            ejections.eject(culprit(cause), culprit_reason(cause, batch));
        }
        ejections.eject_conflicting(observation);
        for fate in &fates {
            if let Fate::Conflict(pr, with) = fate {
                ejections.eject(*pr, EjectReason::Conflicts { with: *with });
            }
        }

        // A batch left waiting that holds a pull request this plan ejects
        // can never land: it is retired, and nothing builds on it.
        let ejected: BTreeSet<PrNumber> = plan.eject.iter().map(|e| e.pr).collect();
        for (batch, fate) in observation.batches.iter().zip(&mut fates) {
            if matches!(fate, Fate::Wait) && batch.members.iter().any(|m| ejected.contains(&m.pr)) {
                *fate = Fate::Retire;
            }
        }

        let mut in_flight: BTreeSet<PrNumber> = BTreeSet::new();
        let mut pending = 0;
        for (batch, fate) in observation.batches.iter().zip(&fates) {
            match fate {
                Fate::Land | Fate::Wait => {
                    in_flight.extend(batch.members.iter().map(|m| m.pr));
                    if is_pending(&batch.state) {
                        pending += 1;
                    }
                }
                Fate::Conflict(..) | Fate::Retire => plan.retire.push(batch.id),
            }
        }

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
            if is_pending(&batch.state) {
                p_end *= self.p_pass(&inference, &members, &below);
            }
            below.extend(members);
        }

        // The members beneath the chain end beyond the observation's trunk:
        // the standing path, and the batches this plan lands, which are
        // trunk's by the time the starts run.
        let beneath: BTreeSet<PrNumber> = below
            .iter()
            .map(|m| m.0)
            .chain(
                observation
                    .batches
                    .iter()
                    .zip(&fates)
                    .filter(|(_, fate)| matches!(fate, Fate::Land))
                    .flat_map(|(batch, _)| batch.members.iter().map(|m| m.pr)),
            )
            .collect();

        let stacks = Stacks::new(observation);
        let member = |n: PrNumber| (n, heads[&n].clone());
        let pool: Vec<Member> = stacks
            .order()
            .iter()
            .copied()
            .filter(|n| !taken.contains(n))
            .map(member)
            .collect();
        if pool.is_empty() {
            return Ok(plan);
        }
        // Whole units with nothing taken, least likely to fail first.
        // TODO(bayes-atomic-unit-starvation): a large `Atomic` unit sorts
        // late and can be skipped step after step under a standing backlog.
        let risk = |unit: &[Member]| {
            1.0 - unit
                .iter()
                .map(|&(pr, _)| 1.0 - inference.posterior(&self.model, Cause::Bad(pr)))
                .product::<f64>()
        };
        let mut units: Vec<Vec<Member>> = stacks
            .units()
            .iter()
            .filter(|unit| unit.iter().all(|n| !taken.contains(n)))
            .map(|unit| unit.iter().copied().map(member).collect())
            .collect();
        units.sort_by(|a, b| risk(a).total_cmp(&risk(b)));

        let mut candidates: Vec<Candidate> = Vec::new();
        if let Some((members, score)) = self
            .grow(
                &inference,
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
        for members in self.finders(&inference, &stacks, &pool) {
            let score = self
                .score(&inference, &members, &[], 1.0, &ejected)
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

#[cfg(test)]
mod tests {
    use super::super::model::FLOOR;
    use super::super::test_support::{bad, members, pair, priors};
    use super::*;
    use crate::BayesConfig;
    use crate::test_support::*;
    use dorfq_core::{Departure, LandRefusal, PrSnapshot, Withdrawal};

    fn params() -> BayesConfig {
        let priors = priors();
        BayesConfig {
            lanes: 2,
            max_batch: 24,
            p_bad: priors.p_bad,
            p_interact: priors.p_interact,
            p_flake: priors.p_flake,
            run_cost: 0.5,
            w_reject: 1.0,
            eject_above: 0.9,
            others_below: 0.1,
            speculate_above: 0.0,
        }
    }

    fn bayes() -> Bayes {
        Bayes::new(params())
    }

    fn ids(ns: &[u64]) -> Vec<BatchId> {
        ns.iter().copied().map(BatchId).collect()
    }

    /// A trunk the batches in a test were not built on.
    fn moved() -> Sha {
        sha(1001)
    }

    fn belief(bayes: &Bayes, cause: Cause) -> f64 {
        bayes.model.belief(cause)
    }

    /// `bayes` after a chain batch {1, 2} failed: it runs 1 as the chain's
    /// new root (batch 2) and 2 alone as a culprit finder (batch 3), both on
    /// trunk.
    fn with_finder(bayes: &mut Bayes) {
        let plan = step(bayes, &observation(ready_all(&[1, 2]), vec![]));
        assert_eq!(plan.start, vec![start(1, trunk(), &[1, 2])]);
        let failed = testing(1, trunk(), &[1, 2], ci_failed());
        let plan = step(bayes, &observation(ready_all(&[1, 2]), vec![failed]));
        assert_eq!(plan.retire, ids(&[1]));
        assert!(plan.eject.is_empty());
        assert_eq!(
            plan.start,
            vec![start(2, trunk(), &[1]), start(3, trunk(), &[2])]
        );
        assert_eq!(bayes.finders, BTreeSet::from([BatchId(3)]));
    }

    #[test]
    fn ingest_walks_through_probes_on_trunk_and_on_a_tracked_tip() {
        let mut bayes = bayes();
        let batches = vec![
            testing(1, trunk(), &[], CiStatus::Pending),
            testing(2, tip(1), &[1], CiStatus::Pending),
            testing(3, tip(2), &[], CiStatus::Passed),
        ];
        step(&mut bayes, &observation(ready_all(&[1, 2]), batches));
        assert_eq!(bayes.trees[&BatchId(1)], vec![]);
        assert_eq!(bayes.trees[&BatchId(2)], members(&[1]));
        assert_eq!(bayes.trees[&BatchId(3)], members(&[1]));
    }

    #[test]
    fn a_fresh_batch_grows_while_its_value_rises_and_stops_at_max_batch() {
        let prs: Vec<u64> = (1..=30).collect();
        let input = observation(ready_all(&prs), vec![]);
        let plan = step(&mut bayes(), &input);
        assert_eq!(plan.start.len(), 1);
        let grown = plan.start[0].prs.len();
        assert!((2..24).contains(&grown), "{grown}");
        assert_eq!(plan.start[0].prs, numbers(&prs[..grown]));
        // One more member would have lowered the score.
        let scheduler = bayes();
        let inference = Inference::of(&scheduler.model).unwrap();
        let none = BTreeSet::new();
        let score = |n: usize| {
            scheduler
                .score(&inference, &members(&prs[..n]), &[], 1.0, &none)
                .unwrap()
        };
        let mut fresh = bayes();
        fresh.model.sync(&input.prs, &none);
        let inference = Inference::of(&fresh.model).unwrap();
        let score_fresh = |n: usize| {
            fresh
                .score(&inference, &members(&prs[..n]), &[], 1.0, &none)
                .unwrap()
        };
        assert!(score_fresh(grown) > score_fresh(grown - 1));
        assert!(score_fresh(grown + 1) <= score_fresh(grown));
        // An unsynced model knows none of them, so every score is the cost.
        assert_eq!(score(1), 0.98 - 0.5);

        let capped = Bayes::new(BayesConfig {
            max_batch: 5,
            ..params()
        });
        let plan = step(&mut capped.clone(), &input);
        assert_eq!(plan.start, vec![start(1, trunk(), &[1, 2, 3, 4, 5])]);
    }

    #[test]
    fn after_a_fresh_failure_the_top_suspect_runs_alone_beside_the_fresh_chain() {
        let mut bayes = bayes();
        step(&mut bayes, &observation(ready_all(&[1, 2]), vec![]));
        let failed = testing(1, trunk(), &[1, 2], ci_failed());
        let plan = step(
            &mut bayes,
            &observation(ready_all(&[1, 2, 3, 4, 5, 6]), vec![failed]),
        );
        // The fresh pull requests make the chain; the first suspect takes
        // the free lane alone, on trunk, as a finder.
        assert_eq!(
            plan.start,
            vec![start(2, trunk(), &[3, 4, 5, 6]), start(3, trunk(), &[1])]
        );
        assert_eq!(bayes.finders, BTreeSet::from([BatchId(3)]));
        // The suspect's singleton outscores every other batch holding it.
        let inference = Inference::of(&bayes.model).unwrap();
        let none = BTreeSet::new();
        let alone = bayes
            .score(&inference, &members(&[1]), &[], 1.0, &none)
            .unwrap();
        let with_fresh = bayes
            .score(&inference, &members(&[3, 4, 5, 6, 1]), &[], 1.0, &none)
            .unwrap();
        let with_other = bayes
            .score(&inference, &members(&[1, 2]), &[], 1.0, &none)
            .unwrap();
        assert!(alone > 0.0);
        assert!(with_other < alone, "{with_other} against {alone}");
        assert!(
            with_fresh
                < bayes
                    .score(&inference, &members(&[3, 4, 5, 6]), &[], 1.0, &none)
                    .unwrap()
        );
    }

    #[test]
    fn a_cause_is_singled_out_only_when_every_other_cause_of_a_run_is_low() {
        let posterior = |cause: Cause| match cause {
            Cause::Bad(PrNumber(1)) => 0.95,
            Cause::Bad(PrNumber(2)) => 0.1,
            _ => 0.09,
        };
        let shared = BTreeSet::from([bad(1), bad(2)]);
        let lone = BTreeSet::from([bad(1), bad(3)]);
        let runs = [(Some(BatchId(1)), &shared)];
        assert!(singled_out(runs.into_iter(), posterior, 0.9, 0.1).is_empty());
        let runs = [(Some(BatchId(1)), &shared), (Some(BatchId(2)), &lone)];
        assert_eq!(
            singled_out(runs.into_iter(), posterior, 0.9, 0.1),
            BTreeMap::from([(bad(1), Some(BatchId(2)))])
        );
    }

    #[test]
    fn a_suspect_is_ejected_once_a_run_singles_it_out_naming_the_run() {
        let mut bayes = bayes();
        with_finder(&mut bayes);
        // 1's lone run failed; 2's finder passed.
        let plan = step(
            &mut bayes,
            &observation(
                ready_all(&[1, 2]),
                vec![
                    testing(2, trunk(), &[1], ci_failed()),
                    testing(3, trunk(), &[2], CiStatus::Passed),
                ],
            ),
        );
        assert_eq!(plan.eject, vec![ejection(1, failed_by(2))]);
        // No chain batch on trunk: the finder lands.
        assert_eq!(plan.land, ids(&[3]));
        assert_eq!(plan.retire, ids(&[2]));
        // Once 1 is gone, the runs that singled it out go too.
        step(
            &mut bayes,
            &observation(
                vec![],
                vec![batch(3, trunk(), &[2], BatchState::Landed { tip: tip(3) })],
            ),
        );
        assert!(bayes.model.evidence().is_empty(), "{:?}", bayes.model);
    }

    #[test]
    fn an_ejected_culprit_takes_every_run_it_explains_with_it() {
        let mut bayes = bayes();
        // {1, 2, 3, 4} failed, then 1 failed alone twice.
        let batches = vec![
            testing(1, moved(), &[1, 2, 3, 4], ci_failed()),
            testing(2, moved(), &[1], ci_failed()),
            testing(3, moved(), &[1], ci_failed()),
        ];
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2, 3, 4]), batches));
        assert_eq!(plan.eject, vec![ejection(1, failed_by(3))]);
        // With 1 gone, the run of all four does not blame 2, 3 and 4.
        step(&mut bayes, &observation(ready_all(&[2, 3, 4]), vec![]));
        assert!(bayes.model.evidence().is_empty(), "{:?}", bayes.model);
        let inference = Inference::of(&bayes.model).unwrap();
        for n in 2..=4 {
            assert_eq!(inference.posterior(&bayes.model, bad(n)), 0.05);
        }
    }

    #[test]
    fn a_waiting_batch_holding_an_ejected_member_is_retired_and_not_built_on() {
        let mut bayes = Bayes::new(BayesConfig {
            lanes: 3,
            ..params()
        });
        // {1, 2} and {1, 3} failed; 2 and 3 then passed alone, while a
        // chain root {1, 4} pends on trunk.
        let batches = vec![
            testing(1, moved(), &[1, 2], ci_failed()),
            testing(2, moved(), &[1, 3], ci_failed()),
            testing(3, trunk(), &[1, 4], CiStatus::Pending),
            testing(4, moved(), &[2], CiStatus::Passed),
            testing(5, moved(), &[3], CiStatus::Passed),
        ];
        let plan = step(
            &mut bayes,
            &observation(ready_all(&[1, 2, 3, 4, 5, 6]), batches),
        );
        assert_eq!(plan.eject, vec![ejection(1, failed_by(2))]);
        assert_eq!(plan.retire, ids(&[1, 2, 3, 4, 5]));
        assert!(!plan.start.is_empty());
        assert!(plan.start.iter().all(|s| s.base == trunk()), "{plan:?}");
        let started: BTreeSet<PrNumber> = plan.start.iter().flat_map(|s| s.prs.clone()).collect();
        assert!(started.contains(&PrNumber(4)), "{plan:?}");
    }

    fn posterior(bayes: &Bayes, cause: Cause) -> f64 {
        Inference::of(&bayes.model)
            .unwrap()
            .posterior(&bayes.model, cause)
    }

    /// `P(Bad 2)` after one failed run whose only cause above the floor is
    /// `Bad(2)` at belief `p`.
    fn lone(p: f64) -> f64 {
        p / (1.0 - 0.98 * (1.0 - p))
    }

    #[test]
    fn a_repushed_or_withdrawn_member_takes_its_failed_runs_with_it() {
        for (withdrawn, prs) in [(false, vec![repushed(1), ready(2)]), (true, vec![ready(2)])] {
            let mut bayes = bayes();
            let failed = testing(1, trunk(), &[1, 2], ci_failed());
            step(&mut bayes, &observation(ready_all(&[1, 2]), vec![failed]));
            assert!(posterior(&bayes, bad(2)) > 0.4);
            step(&mut bayes, &observation(prs.clone(), vec![]));
            assert!(bayes.model.evidence().is_empty(), "{withdrawn}");
            assert_eq!(posterior(&bayes, bad(2)), 0.05);

            // One lone failure of 2 is not enough to eject it.
            let lone_run = testing(10, moved(), &[2], ci_failed());
            let plan = step(&mut bayes, &observation(prs.clone(), vec![lone_run]));
            assert!(plan.eject.is_empty(), "{withdrawn}: {plan:?}");
            let p = posterior(&bayes, bad(2));
            assert!((p - lone(0.05)).abs() < 1e-9, "{p}");
            assert!((p - 0.725).abs() < 1e-3, "{p}");
            // A second one is.
            let again = testing(11, moved(), &[2], ci_failed());
            let plan = step(&mut bayes, &observation(prs, vec![again]));
            assert_eq!(plan.eject, vec![ejection(2, failed_by(11))], "{withdrawn}");
        }
    }

    #[test]
    fn a_landed_member_strips_its_causes_and_the_run_stays() {
        let mut bayes = bayes();
        let failed = testing(1, trunk(), &[1, 2], ci_failed());
        step(&mut bayes, &observation(ready_all(&[1, 2]), vec![failed]));
        let batch = batch(2, trunk(), &[1], BatchState::Landed { tip: tip(2) });
        let mut input = observation(ready_all(&[2]), vec![batch]);
        input.trunk = tip(2);
        input.departed = vec![landed(1, 2)];
        step(&mut bayes, &input);
        let exercised: Vec<&BTreeSet<Cause>> = bayes
            .model
            .evidence()
            .iter()
            .map(|run| &run.exercised)
            .collect();
        assert_eq!(exercised, vec![&BTreeSet::from([bad(2)])]);
        // The landing of 1 regrew Bad(2)'s prior before the lone run's
        // posterior is taken.
        let p = posterior(&bayes, bad(2));
        assert!((p - lone(1.0 - 0.95 * 0.998)).abs() < 1e-9, "{p}");
    }

    #[test]
    fn a_member_merged_by_another_hand_strips_its_causes_like_a_landing() {
        let failed = || testing(1, trunk(), &[1, 2], ci_failed());
        let exercised = |how| {
            let mut bayes = bayes();
            step(&mut bayes, &observation(ready_all(&[1, 2]), vec![failed()]));
            let mut input = observation(ready_all(&[2]), vec![failed()]);
            input.trunk = tip(9);
            input.departed = vec![Departure::new(PrNumber(1), sha(1), how)];
            step(&mut bayes, &input);
            let runs: Vec<BTreeSet<Cause>> = bayes
                .model
                .evidence()
                .iter()
                .map(|run| run.exercised.clone())
                .collect();
            runs
        };
        assert_eq!(exercised(Departed::Merged), vec![BTreeSet::from([bad(2)])]);
        // Withdrawn, the run that exercised it goes with it.
        assert_eq!(
            exercised(Departed::Withdrawn(Withdrawal::IntentCleared)),
            Vec::<BTreeSet<Cause>>::new()
        );
    }

    #[test]
    fn a_failure_seen_with_a_members_repush_teaches_nothing_and_is_retired() {
        let mut bayes = bayes();
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2]), vec![]));
        assert_eq!(plan.start, vec![start(1, trunk(), &[1, 2])]);
        let failed = testing(1, trunk(), &[1, 2], ci_failed());
        let plan = step(
            &mut bayes,
            &observation(vec![repushed(1), ready(2)], vec![failed]),
        );
        assert!(bayes.model.evidence().is_empty(), "{:?}", bayes.model);
        assert_eq!(posterior(&bayes, bad(2)), 0.05);
        assert_eq!(plan.retire, ids(&[1]));
        assert!(plan.eject.is_empty());
    }

    #[test]
    fn a_chain_childs_failure_after_its_parent_landed_is_evidence_about_its_own_members() {
        let mut bayes = bayes();
        let parent = testing(1, trunk(), &[1], CiStatus::Pending);
        let child = |ci| testing(2, tip(1), &[2], ci);
        step(
            &mut bayes,
            &observation(ready_all(&[1, 2]), vec![parent, child(CiStatus::Pending)]),
        );
        let parent = batch(1, trunk(), &[1], BatchState::Landed { tip: tip(1) });
        let mut input = observation(ready_all(&[2]), vec![parent, child(CiStatus::Pending)]);
        input.trunk = tip(1);
        input.departed = vec![landed(1, 1)];
        let plan = step(&mut bayes, &input);
        assert_eq!(plan.retire, ids(&[1]));
        let mut input = observation(ready_all(&[2]), vec![child(CiStatus::Pending)]);
        input.trunk = tip(1);
        step(&mut bayes, &input);
        let mut input = observation(ready_all(&[2]), vec![child(ci_failed())]);
        input.trunk = tip(1);
        step(&mut bayes, &input);
        let exercised: Vec<&BTreeSet<Cause>> = bayes
            .model
            .evidence()
            .iter()
            .map(|run| &run.exercised)
            .collect();
        assert_eq!(exercised, vec![&BTreeSet::from([bad(2)])]);
    }

    #[test]
    fn a_chain_batch_holding_a_repushed_member_is_retired_never_landed_nor_built_on() {
        for ci in [CiStatus::Pending, CiStatus::Passed] {
            let mut bayes = bayes();
            let doomed = testing(1, trunk(), &[1, 2], ci.clone());
            let plan = step(
                &mut bayes,
                &observation(vec![repushed(1), ready(2)], vec![doomed]),
            );
            assert!(plan.land.is_empty(), "{ci:?}: {plan:?}");
            assert_eq!(plan.retire, ids(&[1]), "{ci:?}");
            assert!(plan.start.iter().all(|s| s.base == trunk()), "{plan:?}");
            let started: BTreeSet<PrNumber> =
                plan.start.iter().flat_map(|s| s.prs.clone()).collect();
            assert_eq!(
                started,
                BTreeSet::from([PrNumber(1), PrNumber(2)]),
                "{ci:?}"
            );
        }
    }

    #[test]
    fn a_pending_finder_holding_a_repushed_member_is_retired() {
        let mut bayes = bayes();
        with_finder(&mut bayes);
        // Finder 3 holds 2, which is re-pushed; the chain root 2 holds 1.
        let plan = step(
            &mut bayes,
            &observation(
                vec![ready(1), repushed(2)],
                vec![
                    testing(2, trunk(), &[1], CiStatus::Pending),
                    testing(3, trunk(), &[2], CiStatus::Pending),
                ],
            ),
        );
        assert_eq!(plan.retire, ids(&[3]));
        assert!(bayes.model.evidence().is_empty(), "{:?}", bayes.model);
        assert!(plan.start.iter().all(|s| s.base != tip(3)), "{plan:?}");
        let started: BTreeSet<PrNumber> = plan.start.iter().flat_map(|s| s.prs.clone()).collect();
        assert!(started.contains(&PrNumber(2)), "{plan:?}");
    }

    #[test]
    fn an_ejected_pair_member_takes_every_run_that_held_it() {
        let mut bayes = bayes();
        // {1, 2} failed three times and each passed alone; {2, 3} failed
        // once.
        let batches = vec![
            testing(1, moved(), &[1, 2], ci_failed()),
            testing(2, moved(), &[1, 2], ci_failed()),
            testing(3, moved(), &[1, 2], ci_failed()),
            testing(4, moved(), &[1], CiStatus::Passed),
            testing(5, moved(), &[2], CiStatus::Passed),
            testing(6, moved(), &[2, 3], ci_failed()),
        ];
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2, 3]), batches));
        assert_eq!(plan.eject, vec![ejection(2, interacts_with(1, 3))]);
        assert_eq!(bayes.model.evidence().len(), 4);
        step(&mut bayes, &observation(ready_all(&[1, 3]), vec![]));
        assert!(bayes.model.evidence().is_empty(), "{:?}", bayes.model);
    }

    #[test]
    fn an_unconfirmed_suspect_is_not_ejected() {
        let mut bayes = bayes();
        with_finder(&mut bayes);
        let p = Inference::of(&bayes.model)
            .unwrap()
            .posterior(&bayes.model, bad(1));
        assert!(p > 0.4 && p < 0.9, "{p}");
        let plan = step(
            &mut bayes,
            &observation(
                ready_all(&[1, 2]),
                vec![
                    testing(2, trunk(), &[1], CiStatus::Pending),
                    testing(3, trunk(), &[2], CiStatus::Pending),
                ],
            ),
        );
        assert_eq!(plan, Plan::default());
    }

    #[test]
    fn a_singled_out_pair_ejects_the_newer_member() {
        let mut bayes = bayes();
        // {1, 2} failed three times; each passed alone, on a base gone since.
        let batches = vec![
            testing(1, trunk(), &[1, 2], ci_failed()),
            testing(2, trunk(), &[1, 2], ci_failed()),
            testing(3, trunk(), &[1, 2], ci_failed()),
            testing(4, moved(), &[1], CiStatus::Passed),
            testing(5, moved(), &[2], CiStatus::Passed),
        ];
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2]), batches));
        assert!(
            Inference::of(&bayes.model)
                .unwrap()
                .posterior(&bayes.model, pair(1, 2))
                > 0.99
        );
        assert_eq!(plan.eject, vec![ejection(2, interacts_with(1, 3))]);
        assert_eq!(plan.retire, ids(&[1, 2, 3, 4, 5]));
        assert_eq!(plan.start, vec![start(6, trunk(), &[1])]);
    }

    #[test]
    fn resolution_counts_what_a_failure_would_single_out() {
        let mut bayes = bayes();
        with_finder(&mut bayes);
        let inference = Inference::of(&bayes.model).unwrap();
        let none = BTreeSet::new();
        assert_eq!(
            bayes
                .resolution(&inference, &members(&[1]), &[], &none)
                .unwrap(),
            1.0
        );
        assert_eq!(
            bayes
                .resolution(&inference, &members(&[1, 2]), &[], &none)
                .unwrap(),
            0.0
        );
        // A pull request already ejected by this plan is not counted again.
        let ejected = BTreeSet::from([PrNumber(1)]);
        assert_eq!(
            bayes
                .resolution(&inference, &members(&[1]), &[], &ejected)
                .unwrap(),
            0.0
        );
        let weighted = Bayes {
            params: BayesConfig {
                w_reject: 3.0,
                ..params()
            },
            ..bayes.clone()
        };
        assert_eq!(
            weighted
                .resolution(&inference, &members(&[1]), &[], &none)
                .unwrap(),
            3.0
        );
    }

    #[test]
    fn tied_cross_pairs_are_searched_one_finder_at_a_time() {
        let mut bayes = bayes();
        // {1..6} failed; {1, 2, 3} and {4, 5, 6} each passed: nine cross
        // pairs share the blame with a flake.
        let batches = vec![
            testing(1, trunk(), &[1, 2, 3, 4, 5, 6], ci_failed()),
            testing(2, moved(), &[1, 2, 3], CiStatus::Passed),
            testing(3, moved(), &[4, 5, 6], CiStatus::Passed),
        ];
        let prs = ready_all(&[1, 2, 3, 4, 5, 6]);
        let plan = step(&mut bayes, &observation(prs.clone(), batches));
        assert!(plan.eject.is_empty());
        let inference = Inference::of(&bayes.model).unwrap();
        let tied = inference.posterior(&bayes.model, pair(1, 4));
        for (a, b) in [(1, 5), (2, 4), (3, 6)] {
            assert_eq!(inference.posterior(&bayes.model, pair(a, b)), tied);
        }
        let pool = members(&[1, 2, 3, 4, 5, 6]);
        let stacks = Stacks::new(&observation(prs.clone(), vec![]));
        assert_eq!(
            bayes.finders(&inference, &stacks, &pool),
            vec![members(&[1, 4]), members(&[1, 5]), members(&[1, 6])]
        );

        // The finder of {1, 4} passes: that pair alone is cleared, and the
        // next pair comes up.
        let finder = testing(9, moved(), &[1, 4], CiStatus::Passed);
        let plan = step(&mut bayes, &observation(prs, vec![finder]));
        assert!(plan.eject.is_empty());
        assert_eq!(belief(&bayes, pair(1, 4)), FLOOR);
        assert!(belief(&bayes, pair(1, 5)) > FLOOR);
        assert!(belief(&bayes, pair(2, 4)) > FLOOR);
        let inference = Inference::of(&bayes.model).unwrap();
        assert_eq!(
            bayes.finders(&inference, &stacks, &pool),
            vec![members(&[1, 5]), members(&[1, 6]), members(&[2, 4])]
        );
    }

    #[test]
    fn the_chain_is_extended_once_on_its_end_never_on_a_finder() {
        let mut bayes = Bayes::new(BayesConfig {
            lanes: 4,
            ..params()
        });
        with_finder(&mut bayes);
        // The finder (3) is the newest live tip; the chain's end is 2.
        let plan = step(
            &mut bayes,
            &observation(
                ready_all(&[1, 2, 4, 5]),
                vec![
                    testing(2, trunk(), &[1], CiStatus::Pending),
                    testing(3, trunk(), &[2], CiStatus::Pending),
                ],
            ),
        );
        assert_eq!(plan.start, vec![start(4, tip(2), &[4, 5])]);
        assert!(bayes.finders.contains(&BatchId(3)));
        assert!(!bayes.finders.contains(&BatchId(4)));
    }

    #[test]
    fn beside_a_pending_root_the_chain_is_extended_and_nothing_starts_on_trunk() {
        let mut bayes = Bayes::new(BayesConfig {
            lanes: 3,
            ..params()
        });
        // A pending chain root holds 1 at the prior; 2 and 3 are ready, and
        // two lanes are free.
        let chain = testing(1, trunk(), &[1], CiStatus::Pending);
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2, 3]), vec![chain]));
        // The fresh pull requests extend the chain on the root's tip; with
        // no evidence there is no finder, so the other lane stays free.
        assert_eq!(plan.start, vec![start(2, tip(1), &[2, 3])]);
        assert!(bayes.finders.is_empty());
    }

    #[test]
    fn after_a_failed_pair_one_fresh_pull_request_takes_the_top_suspect_into_the_root() {
        let mut bayes = bayes();
        step(&mut bayes, &observation(ready_all(&[1, 2]), vec![]));
        let failed = testing(1, trunk(), &[1, 2], ci_failed());
        let plan = step(
            &mut bayes,
            &observation(ready_all(&[1, 2, 3]), vec![failed]),
        );
        // Adding 1 to {3} raises its score, so the root is {3, 1}; 1 is in
        // flight, so the free lane runs 2's finder.
        assert_eq!(
            plan.start,
            vec![start(2, trunk(), &[1, 3]), start(3, trunk(), &[2])]
        );
        assert_eq!(bayes.finders, BTreeSet::from([BatchId(3)]));
        let inference = Inference::of(&bayes.model).unwrap();
        let none = BTreeSet::new();
        let score = |prs: &[u64]| {
            bayes
                .score(&inference, &members(prs), &[], 1.0, &none)
                .unwrap()
        };
        assert!(
            score(&[3, 1]) > score(&[3]),
            "{} {}",
            score(&[3, 1]),
            score(&[3])
        );
    }

    #[test]
    fn liveness_beside_a_chain_runs_a_finder_on_trunk_or_nothing() {
        let lazy = || {
            Bayes::new(BayesConfig {
                speculate_above: 1e9,
                ..params()
            })
        };
        // A root of 1 at the prior pends and 2 is ready: the only candidate
        // is the extension, on the root's tip, and nothing starts.
        let root = testing(10, trunk(), &[1], CiStatus::Pending);
        let plan = step(
            &mut lazy(),
            &observation(ready_all(&[1, 2]), vec![root.clone()]),
        );
        assert_eq!(plan, Plan::default());

        // After {1, 2} failed, the same root beside a ready 2: 2's finder is
        // a candidate on trunk, and liveness starts it.
        let mut bayes = lazy();
        let failed = testing(1, trunk(), &[1, 2], ci_failed());
        step(&mut bayes, &observation(ready_all(&[1, 2]), vec![failed]));
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2]), vec![root]));
        assert_eq!(plan.start, vec![start(11, trunk(), &[2])]);
        assert_eq!(bayes.finders, BTreeSet::from([BatchId(11)]));
    }

    #[test]
    fn liveness_runs_the_best_trunk_level_candidate_when_none_is_worth_it() {
        let mut bayes = Bayes::new(BayesConfig {
            speculate_above: 1e9,
            ..params()
        });
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2]), vec![]));
        assert_eq!(plan.start, vec![start(1, trunk(), &[1, 2])]);
        // With every lane busy, nothing.
        let mut bayes = Bayes::new(BayesConfig {
            speculate_above: 1e9,
            lanes: 1,
            ..params()
        });
        let busy = testing(1, trunk(), &[1], CiStatus::Pending);
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2]), vec![busy]));
        assert_eq!(plan, Plan::default());
    }

    #[test]
    fn runs_started_beside_a_landing_are_based_on_its_tip() {
        let mut bayes = bayes();
        let batches = vec![
            testing(1, trunk(), &[1], CiStatus::Passed),
            testing(2, moved(), &[4, 5], ci_failed()),
        ];
        let plan = step(
            &mut bayes,
            &observation(ready_all(&[1, 2, 3, 4, 5]), batches),
        );
        assert_eq!(plan.land, ids(&[1]));
        assert_eq!(plan.retire, ids(&[2]));
        assert_eq!(
            plan.start,
            vec![start(3, tip(1), &[2, 3]), start(4, tip(1), &[4])]
        );
        assert_eq!(bayes.finders, BTreeSet::from([BatchId(4)]));
    }

    #[test]
    fn dead_chain_batches_are_retired_and_their_members_rejoin_the_pool() {
        let mut bayes = bayes();
        let batches = vec![
            testing(1, trunk(), &[1], ci_failed()),
            testing(2, tip(1), &[2], CiStatus::Pending),
            testing(3, tip(1), &[3], CiStatus::Passed),
        ];
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2, 3]), batches));
        assert_eq!(plan.retire, ids(&[1, 2, 3]));
        assert!(plan.land.is_empty());
        // 3's dead pass was still an alibi; 2 and 3 rejoin the pool.
        assert_eq!(belief(&bayes, bad(3)), FLOOR);
        let started: BTreeSet<PrNumber> = plan.start.iter().flat_map(|s| s.prs.clone()).collect();
        assert!(started.contains(&PrNumber(2)) && started.contains(&PrNumber(3)));
    }

    #[test]
    fn a_chain_batch_on_a_stale_trunk_is_cancelled_and_its_members_rejoin_the_pool() {
        let mut bayes = bayes();
        let stale = testing(1, moved(), &[1, 2], CiStatus::Pending);
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2]), vec![stale]));
        assert_eq!(plan.retire, ids(&[1]));
        assert_eq!(plan.start, vec![start(2, trunk(), &[1, 2])]);
    }

    #[test]
    fn conflicts_landings_and_refusals_are_handled() {
        let mut bayes = bayes();
        let prs = vec![ready(1), ready(2), conflicting(3), ready(4), ready(5)];
        let batches = vec![
            batch(
                1,
                trunk(),
                &[1, 2],
                BatchState::Conflict {
                    pr: PrNumber(2),
                    with: ConflictWith::Pr(PrNumber(1)),
                },
            ),
            batch(2, moved(), &[6], BatchState::Landed { tip: trunk() }),
            batch(
                3,
                moved(),
                &[4],
                BatchState::LandRefused {
                    tip: tip(3),
                    reason: LandRefusal::TrunkMoved { trunk: trunk() },
                },
            ),
        ];
        bayes.model.sync(&prs, &BTreeSet::new());
        let plan = step(&mut bayes, &observation(prs, batches));
        assert_eq!(
            plan.eject,
            vec![
                ejection(3, conflicts_with_trunk()),
                ejection(2, conflicts_with(1)),
            ]
        );
        assert_eq!(plan.retire, ids(&[1, 2, 3]));
        // The landing of 6 regrew every queued belief, once.
        let regrown = 1.0 - 0.95 * 0.998;
        assert!((belief(&bayes, bad(1)) - regrown).abs() < 1e-15);
        assert_eq!(plan.start.len(), 1);
        assert_eq!(plan.start[0].prs, numbers(&[1, 4, 5]));
    }

    #[test]
    fn members_of_a_landing_batch_are_never_ejected() {
        let prs = vec![conflicting(1)];
        let passed = testing(1, trunk(), &[1], CiStatus::Passed);
        let plan = step(&mut bayes(), &observation(prs, vec![passed]));
        assert_eq!(plan.land, ids(&[1]));
        assert!(plan.eject.is_empty());
    }

    #[test]
    fn a_pending_finder_on_a_stale_base_holds_its_lane_and_is_read() {
        let mut bayes = bayes();
        with_finder(&mut bayes);
        // The chain's root landed: trunk moved under the finder.
        let root = batch(2, trunk(), &[1], BatchState::Landed { tip: tip(2) });
        let finder = testing(3, trunk(), &[2], CiStatus::Pending);
        let mut input = observation(ready_all(&[2, 4]), vec![root, finder]);
        input.trunk = tip(2);
        input.departed = vec![landed(1, 2)];
        let plan = step(&mut bayes, &input);
        assert_eq!(plan.retire, ids(&[2]));
        // One lane is the finder's; 4 takes the other.
        assert_eq!(plan.start, vec![start(4, tip(2), &[4])]);

        let finder = testing(3, trunk(), &[2], ci_failed());
        let mut input = observation(ready_all(&[2, 4]), vec![finder]);
        input.trunk = tip(2);
        let plan = step(&mut bayes, &input);
        assert_eq!(plan.retire, ids(&[3]));
        assert_eq!(plan.eject, vec![ejection(2, failed_by(3))]);
    }

    #[test]
    fn a_passed_finder_waits_for_no_chain_and_is_retired_beside_a_pending_one() {
        let mut bayes = bayes();
        with_finder(&mut bayes);
        let plan = step(
            &mut bayes,
            &observation(
                ready_all(&[1, 2]),
                vec![
                    testing(2, trunk(), &[1], CiStatus::Pending),
                    testing(3, trunk(), &[2], CiStatus::Passed),
                ],
            ),
        );
        assert!(plan.land.is_empty());
        assert_eq!(plan.retire, ids(&[3]));
        assert_eq!(belief(&bayes, bad(2)), FLOOR);
        // 2, cleared, joins the chain.
        assert_eq!(plan.start, vec![start(4, tip(2), &[2])]);
    }

    #[test]
    fn a_materialised_cross_pair_is_priced_at_its_posterior() {
        let mut bayes = bayes();
        with_finder(&mut bayes);
        step(
            &mut bayes,
            &observation(
                ready_all(&[1, 2]),
                vec![
                    testing(2, trunk(), &[1], CiStatus::Pending),
                    testing(3, trunk(), &[2], CiStatus::Passed),
                ],
            ),
        );
        let inference = Inference::of(&bayes.model).unwrap();
        // {1, 2} failed and 2's finder passed: Bad(2) is floored and the
        // pair keeps the failure's share.
        let pair_posterior = 0.002 / (1.0 - 0.98 * 0.95 * 0.998);
        let expected = 0.98 * (1.0 - FLOOR) * (1.0 - pair_posterior);
        let priced = bayes.p_pass(&inference, &members(&[2]), &members(&[1]));
        assert!(
            (priced - expected).abs() < 1e-9,
            "{priced} against {expected}"
        );
        assert!((priced - 0.952).abs() < 5e-4, "{priced}");
    }

    #[test]
    fn a_passed_finder_below_a_passed_chain_root_is_retired_and_the_root_lands() {
        let mut bayes = bayes();
        with_finder(&mut bayes);
        // The root failed alone: 1 is ejected, and 5 roots the chain anew.
        let plan = step(
            &mut bayes,
            &observation(
                ready_all(&[1, 2, 5]),
                vec![
                    testing(2, trunk(), &[1], ci_failed()),
                    testing(3, trunk(), &[2], CiStatus::Pending),
                ],
            ),
        );
        assert_eq!(plan.eject, vec![ejection(1, failed_by(2))]);
        assert_eq!(plan.start, vec![start(4, trunk(), &[5])]);
        let plan = step(
            &mut bayes,
            &observation(
                ready_all(&[2, 5]),
                vec![
                    testing(3, trunk(), &[2], CiStatus::Passed),
                    testing(4, trunk(), &[5], CiStatus::Passed),
                ],
            ),
        );
        assert_eq!(plan.land, ids(&[4]));
        assert_eq!(plan.retire, ids(&[3]));
    }

    #[test]
    fn a_passed_finder_on_a_stale_base_is_retired_with_its_alibi() {
        for chain in [false, true] {
            let mut bayes = bayes();
            with_finder(&mut bayes);
            let landed = batch(2, trunk(), &[1], BatchState::Landed { tip: tip(2) });
            let finder = testing(3, trunk(), &[2], CiStatus::Passed);
            let mut batches = vec![landed, finder];
            let mut prs = ready_all(&[2]);
            if chain {
                batches.push(testing(4, tip(2), &[7], CiStatus::Pending));
                prs.push(ready(7));
            }
            let mut input = observation(prs.clone(), batches);
            input.trunk = tip(2);
            let plan = step(&mut bayes, &input);
            assert!(plan.land.is_empty(), "{chain}");
            assert_eq!(plan.retire, ids(&[2, 3]));
            assert!(belief(&bayes, bad(2)) < 0.01, "{chain}");

            let mut batches: Vec<BatchStatus> = plan
                .start
                .iter()
                .map(|s| testing(s.id.0, s.base.clone(), &[2], CiStatus::Pending))
                .collect();
            if chain {
                batches.insert(0, testing(4, tip(2), &[7], CiStatus::Pending));
            }
            let mut input = observation(prs, batches);
            input.trunk = tip(2);
            assert!(step(&mut bayes, &input).land.is_empty());
        }
    }

    #[test]
    fn a_fresh_scheduler_starts_above_every_tracked_id() {
        let mut bayes = bayes();
        let landed = batch(7, trunk(), &[9], BatchState::Landed { tip: tip(7) });
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2]), vec![landed]));
        assert_eq!(plan.start, vec![start(8, trunk(), &[1, 2])]);
    }

    #[test]
    fn a_component_too_improbable_to_resolve_is_a_scheduler_error() {
        // Eleven chained runs whose causes all sit near the floor: inference
        // cannot resolve them, and the step reports it instead of a plan.
        let mut bayes = Bayes::new(BayesConfig {
            p_bad: 0.002,
            ..params()
        });
        let prs: Vec<u64> = (1..=12).collect();
        let input = observation(ready_all(&prs), vec![]);
        bayes.model.sync(&input.prs, &BTreeSet::new());
        for r in 1..=11 {
            bayes.model.failed(BatchId(100 + r), &members(&[r, r + 1]));
        }
        let error = bayes.step(&input).unwrap_err();
        assert!(error.to_string().contains("cannot be resolved"), "{error}");
    }

    #[test]
    fn equal_inputs_give_equal_plans() {
        let inputs = [
            observation(ready_all(&[1, 2, 3]), vec![]),
            observation(
                ready_all(&[1, 2, 3]),
                vec![testing(1, trunk(), &[1, 2, 3], ci_failed())],
            ),
            observation(
                ready_all(&[1, 2, 3]),
                vec![
                    testing(2, trunk(), &[1], ci_failed()),
                    testing(3, trunk(), &[2], CiStatus::Passed),
                ],
            ),
            observation(
                vec![ready(1), stacked(2, 1), ready(3), atomic_stacked(4, 3)],
                vec![testing(4, trunk(), &[1, 2, 3, 4], ci_failed())],
            ),
            observation(
                vec![ready(1), stacked(2, 1), ready(3), atomic_stacked(4, 3)],
                vec![
                    testing(5, trunk(), &[1, 2], CiStatus::Pending),
                    testing(6, trunk(), &[3, 4], ci_failed()),
                ],
            ),
        ];
        let (mut a, mut b) = (bayes(), bayes());
        for input in &inputs {
            assert_eq!(a.step(input).unwrap(), b.step(input).unwrap());
        }
        assert_eq!(a, b);
    }

    #[test]
    fn stacked_pull_requests_are_batched_and_emitted_base_first() {
        // 1 on 3, `Atomic` 2 on 1, and 4 on trunk: units {3, 1, 2} and {4}.
        let prs = vec![stacked(1, 3), atomic_stacked(2, 1), ready(3), ready(4)];
        let plan = step(&mut bayes(), &observation(prs, vec![]));
        assert_eq!(plan.start, vec![start(1, trunk(), &[3, 1, 2, 4])]);
    }

    #[test]
    fn an_atomic_unit_larger_than_max_batch_runs_alone_and_whole() {
        let mut bayes = Bayes::new(BayesConfig {
            max_batch: 2,
            ..params()
        });
        let prs = vec![ready(1), stacked(2, 1), atomic_stacked(3, 2)];
        let plan = step(&mut bayes, &observation(prs, vec![]));
        assert_eq!(plan.start, vec![start(1, trunk(), &[1, 2, 3])]);
    }

    #[test]
    fn a_child_joins_the_extension_on_a_tree_that_holds_its_base() {
        // Standing beneath the chain's end.
        let prs = vec![ready(1), stacked(2, 1)];
        let pending = testing(1, trunk(), &[1], CiStatus::Pending);
        let plan = step(&mut bayes(), &observation(prs.clone(), vec![pending]));
        assert_eq!(plan.start, vec![start(2, tip(1), &[2])]);

        // Landed by this plan.
        let passed = testing(1, trunk(), &[1], CiStatus::Passed);
        let plan = step(&mut bayes(), &observation(prs, vec![passed]));
        assert_eq!(plan.land, ids(&[1]));
        assert_eq!(plan.start, vec![start(2, tip(1), &[2])]);
    }

    #[test]
    fn a_child_whose_base_is_not_beneath_the_chain_end_waits() {
        // 1 runs as a finder, off the chain: 2 on 1 cannot extend the chain.
        let mut bayes = bayes();
        bayes.finders.insert(BatchId(1));
        let prs = vec![ready(1), stacked(2, 1)];
        let finder = testing(1, trunk(), &[1], CiStatus::Pending);
        let plan = step(&mut bayes, &observation(prs, vec![finder]));
        assert!(plan.start.is_empty(), "{plan:?}");
    }

    #[test]
    fn no_pair_within_a_stack_is_counted_in_p_pass() {
        let mut bayes = bayes();
        let prs = vec![ready(1), stacked(2, 1), ready(3)];
        bayes.model.sync(&prs, &BTreeSet::new());
        let inference = Inference::of(&bayes.model).unwrap();
        let p = |m: &[u64], b: &[u64]| bayes.p_pass(&inference, &members(m), &members(b));
        let (good, flake) = (0.95f64, 0.98);
        assert!((p(&[1, 2], &[]) - flake * good * good).abs() < 1e-12);
        assert!((p(&[2], &[1]) - flake * good).abs() < 1e-12);
        assert!((p(&[1, 3], &[]) - flake * good * good * 0.998).abs() < 1e-12);
    }

    /// `bayes` after the batch {1, 2, 3}, with 2 on 1, failed on trunk.
    fn failed_stack(bayes: &mut Bayes) -> Vec<PrSnapshot> {
        let prs = vec![ready(1), stacked(2, 1), ready(3)];
        let failed = testing(1, trunk(), &[1, 2, 3], ci_failed());
        step(bayes, &observation(prs.clone(), vec![failed]));
        prs
    }

    #[test]
    fn a_stacked_suspects_finder_is_its_closure() {
        let mut bayes = bayes();
        let prs = failed_stack(&mut bayes);
        let model = &bayes.model;
        assert_eq!(model.evidence().len(), 1);
        assert!(!model.evidence()[0].exercised.contains(&pair(1, 2)));
        let inference = Inference::of(model).unwrap();
        let stacks = Stacks::new(&observation(prs, vec![]));
        assert_eq!(
            bayes.finders(&inference, &stacks, &members(&[1, 2, 3])),
            vec![members(&[1]), members(&[1, 2]), members(&[3])]
        );
        // With 1 in flight, neither finder holding it is generated.
        assert_eq!(
            bayes.finders(&inference, &stacks, &members(&[2, 3])),
            vec![members(&[3])]
        );
    }

    #[test]
    fn a_finder_whose_ancestor_is_in_flight_is_never_started() {
        // Only one lane, and nothing worth its run: the liveness fallback
        // starts the best candidate on trunk, which must not be 2 alone.
        let mut bayes = Bayes::new(BayesConfig {
            lanes: 2,
            speculate_above: 100.0,
            ..params()
        });
        let prs = failed_stack(&mut bayes);
        let running = testing(5, trunk(), &[1], CiStatus::Pending);
        let plan = step(&mut bayes, &observation(prs, vec![running]));
        assert!(
            plan.start.iter().all(|s| s.prs != numbers(&[2])),
            "{plan:?}"
        );
    }

    #[test]
    fn a_passed_closure_finder_under_an_atomic_tip_is_retired_and_alibis_its_members() {
        let mut bayes = bayes();
        failed_stack(&mut bayes);
        bayes.finders.insert(BatchId(9));
        let prs = vec![ready(1), stacked(2, 1), ready(3), atomic_stacked(4, 2)];
        let finder = testing(9, trunk(), &[1, 2], CiStatus::Passed);
        let plan = step(&mut bayes, &observation(prs, vec![finder]));
        assert!(plan.land.is_empty(), "{plan:?}");
        assert_eq!(plan.retire, ids(&[9]));
        assert_eq!(belief(&bayes, bad(1)), FLOOR);
        assert_eq!(belief(&bayes, bad(2)), FLOOR);
        assert!(belief(&bayes, bad(3)) > FLOOR);
    }

    #[test]
    fn an_ejected_ancestor_and_its_blocked_children_take_only_the_runs_it_was_in() {
        let mut bayes = bayes();
        failed_stack(&mut bayes);
        let other = testing(2, moved(), &[3, 4], ci_failed());
        let prs = vec![ready(1), stacked(2, 1), ready(3), ready(4)];
        step(&mut bayes, &observation(prs, vec![other]));
        assert_eq!(bayes.model.evidence().len(), 2);
        // 1 is ejected and 2 withdrawn as blocked on it.
        let mut input = observation(ready_all(&[3, 4]), vec![]);
        input.departed = vec![
            Departure::new(PrNumber(1), sha(1), Departed::Ejected),
            Departure::new(
                PrNumber(2),
                sha(2),
                Departed::Withdrawn(Withdrawal::Blocked { on: PrNumber(1) }),
            ),
        ];
        step(&mut bayes, &input);
        let runs: Vec<BatchId> = bayes.model.evidence().iter().map(|r| r.batch).collect();
        assert_eq!(runs, ids(&[2]));
    }

    #[test]
    fn a_passed_batch_whose_member_gained_an_atomic_child_is_retired() {
        let mut bayes = bayes();
        let prs = vec![ready(1), atomic(stacked(2, 1))];
        let passed = testing(1, trunk(), &[1], CiStatus::Passed);
        let plan = step(&mut bayes, &observation(prs, vec![passed]));
        assert_eq!(plan.retire, ids(&[1]));
        assert!(plan.land.is_empty(), "{plan:?}");
        // The member is free again, and runs with its `Atomic` child.
        assert_eq!(plan.start, vec![start(2, trunk(), &[1, 2])]);
    }
}
