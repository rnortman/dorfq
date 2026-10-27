//! Each step of the Bayesian scheduler: ingest and infer, then plan under
//! the configured speculation; and the pieces both plans share.

use std::collections::{BTreeMap, BTreeSet};

use dorfq_core::{
    BatchId, BatchState, BatchStatus, CiStatus, ConflictWith, Departed, EjectReason, Observation,
    Plan, PrNumber, Scheduler, SchedulerError, Sha,
};

use super::infer::{Inference, RunPosteriors, Unresolved};
use super::model::Cause;
use super::{Bayes, Tracked};
use crate::stacks::{Footing, Stacks};
use crate::{Ejections, Member, Speculation, is_pending};

/// What a step does with one tracked batch.
pub(super) enum Fate {
    /// Landed by this plan.
    Land,
    /// Left running, or waiting for its base to land.
    Wait,
    /// Conflicted while building.
    Conflict(PrNumber, ConflictWith),
    /// Read, dead, landed or refused.
    Retire,
}

/// Whether `batch` is a probe: a run of trunk alone, with no members.
pub(super) fn is_probe(batch: &BatchStatus) -> bool {
    batch.members.is_empty()
}

/// A probe's fate under either speculation, given its CI: waited for while
/// it runs on the observation's `trunk`, and retired on its verdict or once
/// trunk has moved past its base, whose verdict could no longer say whether
/// trunk is broken; never landed nor built on.
pub(super) fn probe_fate(batch: &BatchStatus, ci: &CiStatus, trunk: &Sha) -> Fate {
    match ci {
        CiStatus::Pending if batch.base == *trunk => Fate::Wait,
        CiStatus::Pending | CiStatus::Passed | CiStatus::Failed(_) => Fate::Retire,
    }
}

/// Whether `batch` holds a member no longer queued at the head it was
/// built with: such a batch can never land, and nothing stands on it.
pub(super) fn doomed(heads: &BTreeMap<PrNumber, &Sha>, batch: &BatchStatus) -> bool {
    batch
        .members
        .iter()
        .any(|m| heads.get(&m.pr) != Some(&&m.head))
}

/// The members of the batches `fates` lands.
pub(super) fn landing_members<'a>(
    observation: &'a Observation,
    fates: &'a [Fate],
) -> impl Iterator<Item = PrNumber> + 'a {
    observation
        .batches
        .iter()
        .zip(fates)
        .filter(|(_, fate)| matches!(fate, Fate::Land))
        .flat_map(|(batch, _)| batch.members.iter().map(|m| m.pr))
}

/// For each cause some run singles out, the last such run in `runs`: the
/// cause's posterior is at least `eject_above` and every other cause of the
/// run is below `others_below`. Each run carries its causes' posteriors as
/// of the trunk it started on: a pull request bad then is bad now.
// TODO(bayes-atomic-unit-resolution): the members of an `Atomic` unit share
// every run, so when the unit's runs fail their `Bad` causes keep equal
// posteriors and none is ever singled out.
fn singled_out(
    runs: impl Iterator<Item = (Option<BatchId>, RunPosteriors)>,
    eject_above: f64,
    others_below: f64,
) -> BTreeMap<Cause, Option<BatchId>> {
    let mut out = BTreeMap::new();
    for (batch, causes) in runs {
        let mut suspects = causes.iter().filter(|&&(_, p)| p >= others_below);
        if let (Some(&(cause, p)), None) = (suspects.next(), suspects.next())
            && p >= eject_above
        {
            out.insert(cause, batch);
        }
    }
    out
}

/// Whether `observation` holds a pending probe, a batch with no members,
/// on its trunk: the one a step waits on.
pub(super) fn probe_pending(observation: &Observation) -> bool {
    observation
        .batches
        .iter()
        .any(|b| is_probe(b) && b.base == observation.trunk && is_pending(&b.state))
}

/// The pull request ejected for a singled-out cause; trunk has none.
// TODO(bayes-pair-eject-timing): a pair singled out while both are queued
// ejects the newer at once, before either is broken against trunk; one
// candidate is to never eject on a pair and let the older's landing turn the
// run's blame onto the newer.
fn culprit(cause: Cause) -> Option<PrNumber> {
    match cause {
        Cause::Bad(pr) => Some(pr),
        Cause::Pair(_, newer) => Some(newer),
        Cause::Trunk(_) => None,
    }
}

/// Why the [`culprit`] of a cause `batch` singled out is ejected.
fn culprit_reason(cause: Cause, batch: BatchId) -> EjectReason {
    match cause {
        Cause::Bad(_) => EjectReason::BatchFailed { batch },
        Cause::Pair(older, _) => EjectReason::InteractsWith { pr: older, batch },
        Cause::Trunk(_) => unreachable!("trunk has no culprit"),
    }
}

/// The causes a run of `members` exercises on a node whose tree beyond
/// trunk is `below`: `Bad` of each member, each pair of members and each
/// member with each pull request below.
fn exercised_by(members: &[Member], below: &[Member]) -> BTreeSet<Cause> {
    let mut exercised: BTreeSet<Cause> = BTreeSet::new();
    for (n, &(i, _)) in members.iter().enumerate() {
        exercised.insert(Cause::Bad(i));
        for &(j, _) in members[n + 1..].iter().chain(below) {
            exercised.insert(Cause::pair(i, j));
        }
    }
    exercised
}

/// Whether each batch's tree stands on the observation's trunk: its base is
/// trunk, or the tip of a `Testing` or `Landed` batch whose tree does.
fn on_trunk(observation: &Observation) -> BTreeSet<BatchId> {
    let mut live: BTreeSet<&Sha> = BTreeSet::from([&observation.trunk]);
    let mut on = BTreeSet::new();
    for batch in &observation.batches {
        if !live.contains(&batch.base) {
            continue;
        }
        on.insert(batch.id);
        if let BatchState::Testing { tip, .. } | BatchState::Landed { tip } = &batch.state {
            live.insert(tip);
        }
    }
    on
}

impl Bayes {
    /// Takes in what the observation says: each new batch's tree, the
    /// landings and the trunk position they bring, the queued pull requests
    /// and each new verdict, read at the position its run started on.
    /// `heads` is each queued pull request's head.
    fn ingest(&mut self, observation: &Observation, heads: &BTreeMap<PrNumber, &Sha>) {
        let observed: BTreeSet<BatchId> = observation.batches.iter().map(|b| b.id).collect();
        self.finders.retain(|id| observed.contains(id));
        self.read.retain(|id| observed.contains(id));
        self.tracked.retain(|id, _| observed.contains(id));
        self.landing_breaks.retain(|id, _| observed.contains(id));
        let position =
            i64::try_from(observation.trunk_position).expect("a trunk position fits i64");
        // The changes to trunk since the previous observation, whose position
        // `last_trunk` holds, that the queue did not land; a first
        // observation compares with nothing.
        let foreign = match self.last_trunk {
            Some((_, at)) => {
                let landings = observation
                    .departed
                    .iter()
                    .filter(|d| matches!(d.how, Departed::Landed { .. }))
                    .count();
                let landings = i64::try_from(landings).expect("landings fit i64");
                let foreign = (position - at - landings).max(0);
                u32::try_from(foreign).unwrap_or(u32::MAX)
            }
            None => 0,
        };

        // What reached trunk, landed by the queue or merged by another hand,
        // is trunk now: it leaves every tree, crediting the tree's run with
        // it, and its causes leave the runs it was in while the runs stay.
        let landed: BTreeSet<Member> = observation
            .departed
            .iter()
            .filter(|d| match d.how {
                Departed::Landed { .. } | Departed::Merged => true,
                Departed::Ejected | Departed::Withdrawn(_) => false,
            })
            .map(|d| (d.pr, d.head.clone()))
            .collect();
        for tracked in self.tracked.values_mut() {
            let before = tracked.tree.len();
            tracked.tree.retain(|member| !landed.contains(member));
            tracked.covered +=
                i64::try_from(before - tracked.tree.len()).expect("landings fit i64");
        }

        let tips: BTreeMap<&Sha, &BatchStatus> = observation
            .batches
            .iter()
            .filter_map(|b| b.state.tip().map(|tip| (tip, b)))
            .collect();
        for batch in &observation.batches {
            if self.tracked.contains_key(&batch.id) {
                continue;
            }
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
            tree.retain(|member| !landed.contains(member));
            // On trunk, the run tests the position now; on a tracked tip,
            // the trunk that tip's run tests; on the previous observation's
            // trunk, that trunk's position. Any other base is one this
            // process never saw, a batch inherited at a restart, whose first
            // observation is at the lowest position: it is taken at the
            // position now.
            let covered = if batch.base == observation.trunk {
                position
            } else if let Some(base) = tips
                .get(&batch.base)
                .and_then(|base| self.tracked.get(&base.id))
            {
                base.covered
            } else {
                match &self.last_trunk {
                    Some((trunk, at)) if *trunk == batch.base => *at,
                    _ => position,
                }
            };
            self.tracked.insert(batch.id, Tracked { tree, covered });
        }
        self.last_trunk = Some((observation.trunk.clone(), position));

        self.model.sync(
            &observation.prs,
            &landed.iter().map(|m| m.0).collect(),
            position,
            foreign,
        );
        self.age(observation);

        // A landing's chance of breaking trunk goes to the stretch current
        // when the landing is read, before any pass read now opens another:
        // the runs started after the landing fall in it.
        for batch in &observation.batches {
            if matches!(batch.state, BatchState::Landed { .. })
                && let Some(p) = self.landing_breaks.remove(&batch.id)
            {
                self.model.broke(self.model.current(), p);
            }
        }

        let on_trunk = on_trunk(observation);
        for batch in &observation.batches {
            let Tracked { tree, covered } = &self.tracked[&batch.id];
            match &batch.state {
                BatchState::Testing {
                    ci: CiStatus::Passed,
                    ..
                } if self.read.insert(batch.id) => {
                    self.model
                        .passed(batch.id, tree, *covered, on_trunk.contains(&batch.id));
                }
                // A failure over a head that has gone teaches nothing.
                BatchState::Testing {
                    ci: CiStatus::Failed(_),
                    ..
                } if self.read.insert(batch.id) => {
                    if tree.iter().all(|(pr, head)| heads.get(pr) == Some(&head)) {
                        self.model.failed(batch.id, tree, *covered);
                    }
                }
                BatchState::Testing { .. }
                | BatchState::Landed { .. }
                | BatchState::Conflict { .. }
                | BatchState::LandRefused { .. } => {}
            }
        }
    }

    /// Keeps when each queued pull request first appeared, and computes each
    /// one's value at the observation's `now`; a clock that did not advance
    /// ages nothing.
    pub(super) fn age(&mut self, observation: &Observation) {
        let now = observation.now;
        let queued: BTreeSet<PrNumber> = observation.prs.iter().map(|p| p.number).collect();
        self.since.retain(|pr, _| queued.contains(pr));
        for pr in queued {
            self.since.entry(pr).or_insert(now);
        }
        let w_age = self.params.w_age;
        self.values = self
            .since
            .iter()
            .map(|(&pr, since)| {
                let hours = now.0.saturating_sub(since.0) as f64 / 3600.0;
                (pr, 1.0 + w_age * hours)
            })
            .collect();
    }

    /// The value of queued pull request `pr` this step.
    pub(super) fn value(&self, pr: PrNumber) -> f64 {
        *self
            .values
            .get(&pr)
            .expect("evidence and candidates name only queued pull requests")
    }

    /// The probability that a batch of `members` passes on a node whose tree
    /// beyond trunk is `below`: no flake, and none of the causes its run
    /// exercises holds given every failed run, [`Inference::p_none`] of
    /// `Bad` of each member, each pair of members, each member with each
    /// pull request below, and the current stretch's trunk.
    pub(super) fn p_pass(
        &self,
        inference: &Inference,
        members: &[Member],
        below: &[Member],
    ) -> f64 {
        let mut exercised = exercised_by(members, below);
        exercised.insert(Cause::Trunk(self.model.current()));
        (1.0 - self.params.p_flake) * inference.p_none(&self.model, &exercised)
    }

    /// The resolution value of a batch of `members` on a node whose tree
    /// beyond trunk is `below`: `w_reject` times the value of each pull
    /// request its failure would single out, beyond those `ejected` already;
    /// and for a probe whose failure would single out the current stretch's
    /// trunk, `w_reject` times the value of every queued pull request not
    /// `ejected`, all of which a broken trunk holds up.
    pub(super) fn resolution(
        &self,
        inference: &Inference,
        members: &[Member],
        below: &[Member],
        ejected: &BTreeSet<PrNumber>,
    ) -> Result<f64, Unresolved> {
        let tree: Vec<Member> = members.iter().chain(below).cloned().collect();
        let extra = self.model.would_blame(&tree);
        let runs = inference.with_failure(&self.model, &extra)?;
        let singled = singled_out(
            runs.into_iter(),
            self.params.eject_above,
            self.params.others_below,
        );
        // The hypothetical run is read last and has no batch, so a cause it
        // singles out maps to `None`.
        let trunk = Cause::Trunk(self.model.current());
        let resolved: BTreeSet<PrNumber> =
            if members.is_empty() && singled.get(&trunk) == Some(&None) {
                self.values.keys().copied().collect()
            } else {
                singled.into_keys().filter_map(culprit).collect()
            };
        Ok(self.params.w_reject
            * resolved
                .difference(ejected)
                .map(|&pr| self.value(pr))
                .sum::<f64>())
    }

    /// The score of a batch of `members` on a node whose tree beyond trunk is
    /// `below` and which stands with probability `p_node`: its value,
    /// `p·V(B) + (1 − p)·R` for `V(B)` its members' values, weighted by
    /// `p_node`, less `run_cost` in full: a run occupies its lane whether or
    /// not its base stands.
    pub(super) fn score(
        &self,
        inference: &Inference,
        members: &[Member],
        below: &[Member],
        p_node: f64,
        ejected: &BTreeSet<PrNumber>,
    ) -> Result<f64, Unresolved> {
        let p = self.p_pass(inference, members, below);
        let resolution = self.resolution(inference, members, below, ejected)?;
        // TODO(bayes-landing-term): the landing term credits a batch whose
        // pass will not land — a chain finder while a chain batch holds trunk
        // — with its members' full value.
        let landing: f64 = members.iter().map(|&(pr, _)| self.value(pr)).sum();
        let value = p * landing + (1.0 - p) * resolution;
        // TODO(bayes-landing-term): `p_node · p` treats the node standing
        // and this batch passing as independent; batches whose trees share a
        // failed run are not.
        Ok(p_node * value - self.params.run_cost)
    }

    /// Grows a batch on a node from `units`, in order, while its score
    /// rises. A unit joins whole, and only when every member's base is
    /// trunk, an earlier member, or beneath the node in `footing`; a unit
    /// that would take the batch past `max_batch` is skipped unless the
    /// batch is empty, so an `Atomic` stack larger than the cap runs alone.
    pub(super) fn grow(
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

    /// The suspect batches for a node whose tree beyond the observation's
    /// trunk holds `beneath`. A suspect's closure is the suspect with every
    /// enqueued ancestor not beneath the node, bases first; it can run when
    /// no suspect is beneath the node and every member is batchable and in
    /// `pool`. Per component, walking its causes from the most probable:
    /// the closures of the first three whose closures can run, passing over
    /// those that cannot and every `Bad` suspect or pair covered — by an
    /// alibi of the pull request, or a pass of the pair since it was
    /// materialised, at or after the run's trunk — in every failed run that
    /// blames it; right after each such `Bad` suspect's closure, its
    /// ancestors alone, while a failed run still blames one of them beside
    /// the suspect and no alibi of that ancestor covers the run — their
    /// pass discounts them there; and the closure of the two most probable
    /// such `Bad` suspects together. A suspect's closure, its ancestors and
    /// a pair's closure are each proposed once per trunk position. A
    /// stretch's trunk is passed over, except that with `probe` the current
    /// stretch's is a suspect whose set is empty: a probe, which tests trunk
    /// alone. A set already generated is not repeated.
    pub(super) fn suspects(
        &self,
        inference: &Inference,
        stacks: &Stacks,
        pool: &[Member],
        beneath: &BTreeSet<PrNumber>,
        probe: bool,
    ) -> Vec<Vec<Member>> {
        let available: BTreeMap<PrNumber, &Sha> = pool.iter().map(|(pr, h)| (*pr, h)).collect();
        let closure = |suspects: &[PrNumber]| -> Option<Vec<Member>> {
            let mut all: BTreeSet<PrNumber> = BTreeSet::new();
            for &pr in suspects {
                if beneath.contains(&pr) {
                    return None;
                }
                all.extend(stacks.closure(pr)?);
            }
            stacks
                .ordered(all.difference(beneath).copied())
                .into_iter()
                .map(|pr| available.get(&pr).map(|&head| (pr, head.clone())))
                .collect()
        };
        let uncovered_in = |r: usize, cause: Cause| {
            inference.run_causes(r).contains(&cause)
                && !self.model.covered(cause, inference.run_at(r))
        };
        let mut out: Vec<Vec<Member>> = Vec::new();
        for runs in inference.components() {
            let mut ranked: Vec<(f64, Cause)> = inference
                .component_causes(runs)
                .into_iter()
                .map(|cause| (inference.posterior(&self.model, cause), cause))
                .collect();
            ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            // TODO(bayes-pair-search): a materialised set of cross pairs is
            // searched one pair per finder, in rank order; cross-subset
            // finders would locate the failing pair in logarithmically many
            // runs.
            let mut sets: Vec<Vec<Member>> = Vec::new();
            let mut counted = 0;
            let mut bads: Vec<PrNumber> = Vec::new();
            for (_, cause) in &ranked {
                if counted == 3 {
                    break;
                }
                if let Cause::Trunk(stretch) = *cause {
                    if probe && stretch == self.model.current() {
                        counted += 1;
                        sets.push(Vec::new());
                    }
                    continue;
                }
                let Some(set) = closure(&cause.prs()) else {
                    continue;
                };
                if !runs.iter().any(|&r| uncovered_in(r, *cause)) {
                    continue;
                }
                let Cause::Bad(suspect) = *cause else {
                    counted += 1;
                    sets.push(set);
                    continue;
                };
                counted += 1;
                bads.push(suspect);
                let ancestors: Vec<Member> =
                    set.iter().filter(|m| m.0 != suspect).cloned().collect();
                let blamed_beside = runs.iter().any(|&r| {
                    inference.run_causes(r).contains(&Cause::Bad(suspect))
                        && ancestors.iter().any(|m| uncovered_in(r, Cause::Bad(m.0)))
                });
                sets.push(set);
                if blamed_beside {
                    sets.push(ancestors);
                }
            }
            if let [a, b, ..] = bads[..]
                && let Some(set) = closure(&[a, b])
            {
                sets.push(set);
            }
            for set in sets {
                if !out.contains(&set) {
                    out.push(set);
                }
            }
        }
        out
    }

    /// The ejections every plan makes: each culprit a run singles out and
    /// each conflict, never a member of a batch `fates` lands. A batch left
    /// waiting that holds an ejected pull request can never land, and its
    /// fate becomes `Retire`. Returns the ejected pull requests.
    pub(super) fn eject(
        &self,
        observation: &Observation,
        inference: &Inference,
        fates: &mut [Fate],
        plan: &mut Plan,
    ) -> BTreeSet<PrNumber> {
        let mut ejections = Ejections::new(observation, &mut plan.eject);
        for (batch, fate) in observation.batches.iter().zip(fates.iter()) {
            if matches!(fate, Fate::Land) {
                ejections.landing(batch);
            }
        }
        let evidence = self.model.evidence();
        let runs =
            (0..evidence.len()).map(|r| (Some(evidence[r].batch), inference.run_posteriors(r)));
        let singled = singled_out(runs, self.params.eject_above, self.params.others_below);
        for (cause, batch) in singled {
            let batch = batch.expect("every existing run has a batch");
            if let Some(pr) = culprit(cause) {
                ejections.eject(pr, culprit_reason(cause, batch));
            }
        }
        ejections.eject_conflicting(observation);
        for fate in fates.iter() {
            if let Fate::Conflict(pr, with) = fate {
                ejections.eject(*pr, EjectReason::Conflicts { with: *with });
            }
        }

        let ejected: BTreeSet<PrNumber> = plan.eject.iter().map(|e| e.pr).collect();
        for (batch, fate) in observation.batches.iter().zip(fates.iter_mut()) {
            if matches!(fate, Fate::Wait) && batch.members.iter().any(|m| ejected.contains(&m.pr)) {
                *fate = Fate::Retire;
            }
        }
        ejected
    }

    /// Retires every batch whose fate is neither `Land` nor `Wait`. Returns
    /// the members of the batches that stay, and how many of them pend.
    pub(super) fn settle(
        &self,
        observation: &Observation,
        fates: &[Fate],
        plan: &mut Plan,
    ) -> (BTreeSet<PrNumber>, usize) {
        let mut in_flight: BTreeSet<PrNumber> = BTreeSet::new();
        let mut pending = 0;
        for (batch, fate) in observation.batches.iter().zip(fates) {
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
        (in_flight, pending)
    }

    /// The batchable pull requests not `taken`, at their heads, bases first.
    pub(super) fn pool(
        &self,
        stacks: &Stacks,
        heads: &BTreeMap<PrNumber, &Sha>,
        taken: &BTreeSet<PrNumber>,
    ) -> Vec<Member> {
        stacks
            .order()
            .iter()
            .copied()
            .filter(|n| !taken.contains(n))
            .map(|n| (n, heads[&n].clone()))
            .collect()
    }

    /// The units with nothing `taken`, at their heads, by the chance one
    /// fails given every failed run ([`Inference::p_none`] of its members'
    /// `Bad`) over its members' mean value, least first.
    pub(super) fn units(
        &self,
        inference: &Inference,
        stacks: &Stacks,
        heads: &BTreeMap<PrNumber, &Sha>,
        taken: &BTreeSet<PrNumber>,
    ) -> Vec<Vec<Member>> {
        // TODO(bayes-atomic-unit-starvation): a large `Atomic` unit sorts
        // late and can be skipped step after step under a standing backlog.
        let risk = |unit: &[Member]| {
            let causes: BTreeSet<Cause> = unit.iter().map(|&(pr, _)| Cause::Bad(pr)).collect();
            let fails = 1.0 - inference.p_none(&self.model, &causes);
            let value: f64 = unit.iter().map(|&(pr, _)| self.value(pr)).sum();
            fails / (value / unit.len() as f64)
        };
        let mut units: Vec<(f64, Vec<Member>)> = stacks
            .units()
            .iter()
            .filter(|unit| unit.iter().all(|n| !taken.contains(n)))
            .map(|unit| {
                let unit: Vec<Member> = unit.iter().map(|&n| (n, heads[&n].clone())).collect();
                (risk(&unit), unit)
            })
            .collect();
        units.sort_by(|a, b| a.0.total_cmp(&b.0));
        let units: Vec<Vec<Member>> = units.into_iter().map(|(_, unit)| unit).collect();
        units
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
        let plan = match self.params.speculation {
            Speculation::Chain => self.chain_plan(observation, &heads, &inference),
            Speculation::Tree => self.tree_plan(observation, &heads, &inference),
        }?;
        // What each landing batch's run exercised, by every verdict read now,
        // its own pass included: the chance it lands a break.
        for &id in &plan.land {
            let batch = observation
                .batches
                .iter()
                .find(|b| b.id == id)
                .expect("a plan lands an observed batch");
            let members: Vec<Member> = batch
                .members
                .iter()
                .map(|m| (m.pr, m.head.clone()))
                .collect();
            let below: Vec<Member> = self.tracked[&id]
                .tree
                .iter()
                .filter(|m| !members.contains(m))
                .cloned()
                .collect();
            let p = 1.0 - inference.p_none(&self.model, &exercised_by(&members, &below));
            self.landing_breaks.insert(id, p);
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::FLOOR;
    use super::super::test_support::{
        bad, failed_alone, informative, members, pair, params, prior,
    };
    use super::*;
    use crate::BayesConfig;
    use crate::test_support::*;
    use dorfq_core::{BatchStart, Departure, LandRefusal, PrSnapshot, Timestamp, Withdrawal};

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

    /// The belief in a pull request `k` landings past its origin, with no
    /// alibi.
    fn unpassed(bayes: &Bayes, k: i64) -> f64 {
        let content = bayes.model.discounted(params().p_interact, 1);
        1.0 - (1.0 - content) * (1.0 - bayes.model.regrown(k))
    }

    /// The belief in a pull request [`BEHIND`] landings past its origin
    /// after one pass at the trunk position.
    fn passed_once(bayes: &Bayes) -> f64 {
        let m = &bayes.model;
        let content = m.discounted(params().p_interact, 2);
        1.0 - (1.0 - content) * (1.0 - m.discounted(m.regrown(i64::from(BEHIND)), 1))
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

    /// [`with_finder`]'s chain root (batch 2) and finder (batch 3) seen
    /// running on trunk, before anything lands.
    fn both_running(bayes: &mut Bayes) {
        let running = vec![
            testing(2, trunk(), &[1], CiStatus::Pending),
            testing(3, trunk(), &[2], CiStatus::Pending),
        ];
        let plan = step(bayes, &observation(ready_all(&[1, 2]), running));
        assert!(plan.start.is_empty(), "{plan:?}");
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
        assert_eq!(bayes.tracked[&BatchId(1)].tree, vec![]);
        assert_eq!(bayes.tracked[&BatchId(2)].tree, members(&[1]));
        assert_eq!(bayes.tracked[&BatchId(3)].tree, members(&[1]));
    }

    /// An observation at `hours` past the start of the clock.
    fn at(hours: u64, prs: Vec<PrSnapshot>) -> Observation {
        let mut input = observation(prs, vec![]);
        input.now = Timestamp(hours * 3600);
        input
    }

    /// A scheduler at `w_age` that saw `old` at hour 0 and both `old` and
    /// `new` at hour 2, with the beliefs both report.
    fn aged(w_age: f64, old: &[PrSnapshot], new: &[PrSnapshot]) -> (Bayes, Observation) {
        let mut bayes = Bayes::new(BayesConfig { w_age, ..params() });
        bayes.age(&at(0, old.to_vec()));
        let mut prs = [old, new].concat();
        prs.sort_by_key(|pr| pr.number);
        let input = at(2, prs);
        bayes.model.sync(&input.prs, &BTreeSet::new(), 0, 0);
        bayes.age(&input);
        (bayes, input)
    }

    #[test]
    fn a_pull_request_ages_from_first_sight_across_repushes_until_it_leaves() {
        let mut bayes = Bayes::new(BayesConfig {
            w_age: 0.5,
            ..params()
        });
        bayes.age(&at(1, ready_all(&[1])));
        bayes.age(&at(2, ready_all(&[1, 2])));
        assert_eq!(bayes.value(PrNumber(1)), 1.5);
        assert_eq!(bayes.value(PrNumber(2)), 1.0);
        // A re-push keeps the wait; a clock that went back ages nothing.
        bayes.age(&at(5, vec![repushed(1), ready(2)]));
        assert_eq!(bayes.value(PrNumber(1)), 3.0);
        bayes.age(&at(0, vec![repushed(1), ready(2)]));
        assert_eq!(bayes.value(PrNumber(1)), 1.0);
        // Gone and back is a new wait.
        bayes.age(&at(6, ready_all(&[2])));
        bayes.age(&at(7, ready_all(&[1, 2])));
        assert_eq!(bayes.value(PrNumber(1)), 1.0);
        assert_eq!(bayes.value(PrNumber(2)), 3.5);
        assert_eq!(bayes.values.len(), 2);
    }

    #[test]
    fn an_older_pull_request_is_worth_more_to_land_and_to_resolve() {
        // 1 has waited two hours, 2 has just arrived: worth 2 and 1.
        let (mut bayes, _) = aged(0.5, &[ready(1)], &[ready(2)]);
        let none = BTreeSet::new();
        let inference = Inference::of(&bayes.model).unwrap();
        let score = |n| {
            bayes
                .score(&inference, &members(&[n]), &[], 1.0, &none)
                .unwrap()
        };
        let p = bayes.p_pass(&inference, &members(&[1]), &[]);
        assert_eq!(p, bayes.p_pass(&inference, &members(&[2]), &[]));
        assert!((score(1) - score(2) - p).abs() < 1e-12);
        let both = bayes
            .score(&inference, &members(&[1, 2]), &[], 1.0, &none)
            .unwrap();
        let p = bayes.p_pass(&inference, &members(&[1, 2]), &[]);
        assert!((both - (p * 3.0 - 0.5)).abs() < 1e-12, "{both}");

        // One lone failure each, a pass on trunk between them: a second
        // would single either out. In one stretch, trunk would explain both
        // failures and a third alike.
        bayes.model.failed(BatchId(9), &members(&[1]), 0);
        bayes.model.passed(BatchId(10), &[], 0, true);
        bayes.model.failed(BatchId(11), &members(&[2]), 0);
        bayes.params.w_reject = 3.0;
        let inference = Inference::of(&bayes.model).unwrap();
        let resolution = |n| {
            bayes
                .resolution(&inference, &members(&[n]), &[], &none)
                .unwrap()
        };
        assert_eq!(resolution(1), 6.0);
        assert_eq!(resolution(2), 3.0);
    }

    #[test]
    fn units_order_by_risk_over_value() {
        let order = |bayes: &Bayes, input: &Observation| {
            let inference = Inference::of(&bayes.model).unwrap();
            let heads: BTreeMap<PrNumber, &Sha> =
                input.prs.iter().map(|p| (p.number, &p.head)).collect();
            let units = bayes.units(&inference, &Stacks::new(input), &heads, &BTreeSet::new());
            units.iter().map(|u| u[0].0.0).collect::<Vec<u64>>()
        };
        // Equal risk: the older pair, 3 and 4, first; without age, by number.
        let (old, new) = (ready_all(&[3, 4]), ready_all(&[1, 2]));
        let (bayes, input) = aged(0.5, &old, &new);
        assert_eq!(order(&bayes, &input), vec![3, 4, 1, 2]);
        let (bayes, input) = aged(0.0, &old, &new);
        assert_eq!(order(&bayes, &input), vec![1, 2, 3, 4]);
        // 3, worth 2, is more than twice as likely to fail as 1, worth 1.
        let (bayes, input) = aged(0.5, &[stale(ready(3), 100)], &[ready(1)]);
        assert_eq!(order(&bayes, &input), vec![1, 3]);
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
        let mut scheduler = bayes();
        scheduler.age(&input);
        let inference = Inference::of(&scheduler.model).unwrap();
        let none = BTreeSet::new();
        let score = |n: usize| {
            scheduler
                .score(&inference, &members(&prs[..n]), &[], 1.0, &none)
                .unwrap()
        };
        let mut fresh = bayes();
        fresh.model.sync(&input.prs, &none, 0, 0);
        fresh.age(&input);
        let inference = Inference::of(&fresh.model).unwrap();
        let score_fresh = |n: usize| {
            fresh
                .score(&inference, &members(&prs[..n]), &[], 1.0, &none)
                .unwrap()
        };
        assert!(score_fresh(grown) > score_fresh(grown - 1));
        assert!(score_fresh(grown + 1) <= score_fresh(grown));
        // A model holding none of them sees a pass but for a flake or a
        // broken trunk, less the cost.
        assert_eq!(score(1), 0.98 * 0.999 - 0.5);

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
        let shared = vec![(bad(1), 0.95), (bad(2), 0.1)];
        let lone = vec![(bad(1), 0.95), (bad(3), 0.09)];
        let runs = [(Some(BatchId(1)), shared.clone())];
        assert!(singled_out(runs.into_iter(), 0.9, 0.1).is_empty());
        let runs = [(Some(BatchId(1)), shared), (Some(BatchId(2)), lone)];
        assert_eq!(
            singled_out(runs.into_iter(), 0.9, 0.1),
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
            assert_eq!(inference.posterior(&bayes.model, bad(n)), prior());
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

    /// `P(Bad 2)` after one failed run whose only causes above the floor
    /// are `Bad(2)` at belief `p` and its stretch's trunk at `p_trunk`.
    fn lone(p: f64) -> f64 {
        p / (1.0 - 0.98 * 0.999 * (1.0 - p))
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
            assert_eq!(posterior(&bayes, bad(2)), prior());

            // One lone failure of 2 is not enough to eject it.
            let lone_run = testing(10, moved(), &[2], ci_failed());
            let plan = step(&mut bayes, &observation(prs.clone(), vec![lone_run]));
            assert!(plan.eject.is_empty(), "{withdrawn}: {plan:?}");
            let p = posterior(&bayes, bad(2));
            assert!((p - lone(prior())).abs() < 1e-9, "{p}");
            assert!((p - 0.71).abs() < 1e-3, "{p}");
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
        let mut input = observation(ready_after(&[2], 1), vec![batch]);
        input.trunk = tip(2);
        input.trunk_position = 1;
        input.departed = vec![landed(1, 2)];
        step(&mut bayes, &input);
        let exercised: Vec<&BTreeSet<Cause>> = bayes
            .model
            .evidence()
            .iter()
            .map(|run| &run.exercised)
            .collect();
        assert_eq!(exercised, vec![&BTreeSet::from([bad(2)])]);
        // The landing of 1 came after the run started: it adds its own
        // chance of having broken 2 and leaves the run's share alone.
        let p = posterior(&bayes, bad(2));
        let since = bayes.model.regrown(1);
        assert!(
            (p - (1.0 - (1.0 - lone(prior())) * (1.0 - since))).abs() < 1e-9,
            "{p}"
        );
    }

    #[test]
    fn a_landing_regrows_what_was_queued_and_not_what_arrives_with_it() {
        let mut bayes = bayes();
        step(&mut bayes, &observation(ready_all(&[1, 3]), vec![]));
        let batch = batch(2, trunk(), &[1], BatchState::Landed { tip: tip(2) });
        let mut input = observation(vec![fresh(2), stale(ready(3), BEHIND + 1)], vec![batch]);
        input.trunk = tip(2);
        input.trunk_position = 1;
        input.departed = vec![landed(1, 2)];
        step(&mut bayes, &input);
        // 2's own CI started on the new trunk; 3's staleness counts 1.
        assert_eq!(bayes.model.position(), 1);
        assert_eq!(bayes.model.origin(PrNumber(2)), Some(1));
        let content = bayes.model.discounted(params().p_interact, 1);
        assert!((belief(&bayes, bad(2)) - content).abs() < 1e-15);
        let regrown = bayes.model.regrown(i64::from(BEHIND) + 1);
        assert_eq!(
            belief(&bayes, bad(3)),
            1.0 - (1.0 - content) * (1.0 - regrown)
        );
    }

    #[test]
    fn a_member_merged_by_another_hand_strips_its_causes_like_a_landing() {
        let failed = || testing(1, trunk(), &[1, 2], ci_failed());
        let exercised = |how| {
            let mut bayes = bayes();
            step(&mut bayes, &observation(ready_all(&[1, 2]), vec![failed()]));
            let mut input = observation(ready_all(&[2]), vec![failed()]);
            input.trunk = tip(9);
            input.trunk_position = u64::from(matches!(how, Departed::Merged));
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
    fn a_landing_or_a_merge_regrows_a_survivor_once() {
        // 1 lands by the queue: Bad(2) regrows by one landing.
        let mut queue = bayes();
        step(&mut queue, &observation(ready_all(&[1, 2]), vec![]));
        let batch = batch(1, trunk(), &[1], BatchState::Landed { tip: tip(1) });
        let mut input = observation(ready_after(&[2], 1), vec![batch]);
        input.trunk = tip(1);
        input.trunk_position = 1;
        input.departed = vec![landed(1, 1)];
        step(&mut queue, &input);
        let regrown = 1.0 - (1.0 - prior()) * 0.998;
        assert!((belief(&queue, bad(2)) - regrown).abs() < 1e-15);
        // 1 merged by another hand: the same.
        let merged = |mut bayes: Bayes| {
            step(&mut bayes, &observation(ready_all(&[1, 2]), vec![]));
            let mut input = observation(ready_after(&[2], 1), vec![]);
            input.trunk = tip(9);
            input.trunk_position = 1;
            input.departed = vec![Departure::new(PrNumber(1), sha(1), Departed::Merged)];
            step(&mut bayes, &input);
            belief(&bayes, bad(2))
        };
        assert!((merged(bayes()) - regrown).abs() < 1e-15);
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
        assert_eq!(posterior(&bayes, bad(2)), prior());
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
        input.trunk_position = 1;
        input.departed = vec![landed(1, 1)];
        let plan = step(&mut bayes, &input);
        assert_eq!(plan.retire, ids(&[1]));
        let mut input = observation(ready_all(&[2]), vec![child(CiStatus::Pending)]);
        input.trunk = tip(1);
        input.trunk_position = 1;
        step(&mut bayes, &input);
        let mut input = observation(ready_all(&[2]), vec![child(ci_failed())]);
        input.trunk = tip(1);
        input.trunk_position = 1;
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
        // {1, 2} failed three times and each passed alone on trunk three
        // times, a pass between every two failures; {2, 3} failed once,
        // which keeps Bad(2) above the floor of a singled-out pair's others
        // until its third pass.
        let model = &mut bayes.model;
        model.sync(&ready_all(&[1, 2, 3]), &BTreeSet::new(), 0, 0);
        for k in 0..3 {
            model.failed(BatchId(3 * k + 1), &members(&[1, 2]), 0);
            model.passed(BatchId(3 * k + 2), &members(&[1]), 0, true);
            model.passed(BatchId(3 * k + 3), &members(&[2]), 0, true);
        }
        model.failed(BatchId(10), &members(&[2, 3]), 0);
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2, 3]), vec![]));
        assert_eq!(plan.eject, vec![ejection(2, interacts_with(1, 7))]);
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
        // {1, 2} failed three times; each passed alone on trunk twice, the
        // passes between the failures: one pass each would leave each `Bad`
        // near the pair, and failures in one stretch would leave the pair
        // near its trunk.
        let model = &mut bayes.model;
        model.sync(&ready_all(&[1, 2]), &BTreeSet::new(), 0, 0);
        model.failed(BatchId(1), &members(&[1, 2]), 0);
        for k in 0..2 {
            model.passed(BatchId(3 * k + 2), &members(&[1]), 0, true);
            model.passed(BatchId(3 * k + 3), &members(&[2]), 0, true);
            model.failed(BatchId(3 * k + 4), &members(&[1, 2]), 0);
        }
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2]), vec![]));
        let p = Inference::of(&bayes.model)
            .unwrap()
            .posterior(&bayes.model, pair(1, 2));
        assert!(p > 0.95, "{p}");
        assert_eq!(plan.eject, vec![ejection(2, interacts_with(1, 7))]);
        assert_eq!(plan.start, vec![start(1, trunk(), &[1])]);
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
        // Pair finders whose members are both discounted: each pass
        // discounts a pair the failed run still blames.
        let sets = bayes.suspects(&inference, &stacks, &pool, &BTreeSet::new(), false);
        assert_eq!(
            sets,
            vec![members(&[1, 4]), members(&[1, 5]), members(&[1, 6])]
        );
        informative(&bayes.model, &inference, &sets);

        // The finder of {1, 4} passes: that pair alone is discounted, and
        // the next pair comes up.
        let finder = testing(9, moved(), &[1, 4], CiStatus::Passed);
        let plan = step(&mut bayes, &observation(prs, vec![finder]));
        assert!(plan.eject.is_empty());
        let once = bayes.model.discounted(params().p_interact, 1);
        assert_eq!(belief(&bayes, pair(1, 4)), once);
        assert!(belief(&bayes, pair(1, 5)) > once);
        assert!(belief(&bayes, pair(2, 4)) > FLOOR);
        let inference = Inference::of(&bayes.model).unwrap();
        // The finder's pass covers {1, 4} at the failed run's trunk, so the
        // pair is passed over there, not merely outranked.
        assert!(bayes.model.covered(pair(1, 4), inference.run_at(0)));
        let sets = bayes.suspects(&inference, &stacks, &pool, &BTreeSet::new(), false);
        assert_eq!(
            sets,
            vec![members(&[1, 5]), members(&[1, 6]), members(&[2, 4])]
        );
        informative(&bayes.model, &inference, &sets);
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
    fn an_extension_is_kept_off_an_unlikely_root() {
        // A pending root of ten at the prior passes about half the time; one
        // fresh pull request could extend it.
        let mut full = Bayes::new(BayesConfig {
            run_cost: 0.6,
            ..params()
        });
        let root: Vec<u64> = (1..=10).collect();
        let prs: Vec<u64> = (1..=11).collect();
        let input = observation(
            ready_all(&prs),
            vec![testing(1, trunk(), &root, CiStatus::Pending)],
        );
        assert_eq!(step(&mut full, &input), Plan::default());

        let inference = Inference::of(&full.model).unwrap();
        let none = BTreeSet::new();
        let (root, extension) = (members(&root), members(&[11]));
        let p_end = full.p_pass(&inference, &root, &[]);
        let p = full.p_pass(&inference, &extension, &root);
        let r = full
            .resolution(&inference, &extension, &root, &none)
            .unwrap();
        let value = p + (1.0 - p) * r;
        assert!(p_end < 0.6 && value > 0.6, "{p_end} {value}");
        let score = full
            .score(&inference, &extension, &root, p_end, &none)
            .unwrap();
        assert!((score - (p_end * value - 0.6)).abs() < 1e-12);
        assert!(score < 0.0);
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

        // The same with a probe pending and a lane to spare: nothing.
        let mut bayes = Bayes::new(BayesConfig {
            speculate_above: 1e9,
            lanes: 3,
            ..params()
        });
        let failed = testing(1, trunk(), &[1, 2], ci_failed());
        step(&mut bayes, &observation(ready_all(&[1, 2]), vec![failed]));
        let probe = testing(12, trunk(), &[], CiStatus::Pending);
        let root = testing(10, trunk(), &[1], CiStatus::Pending);
        let plan = step(
            &mut bayes,
            &observation(ready_all(&[1, 2]), vec![root, probe]),
        );
        assert_eq!(plan, Plan::default());
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
        assert_eq!(belief(&bayes, bad(3)), passed_once(&bayes));
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
        bayes.model.sync(&prs, &BTreeSet::new(), 0, 0);
        // The forge counts 6's landing in every staleness it reports.
        let prs = prs
            .into_iter()
            .map(|pr| {
                let behind = pr.behind + 1;
                stale(pr, behind)
            })
            .collect();
        let mut input = observation(prs, batches);
        input.trunk_position = 1;
        input.departed = vec![landed(6, 2)];
        let plan = step(&mut bayes, &input);
        assert_eq!(
            plan.eject,
            vec![
                ejection(3, conflicts_with_trunk()),
                ejection(2, conflicts_with(1)),
            ]
        );
        assert_eq!(plan.retire, ids(&[1, 2, 3]));
        // The landing of 6 regrew every queued belief, once.
        let regrown = unpassed(&bayes, i64::from(BEHIND) + 1);
        assert_eq!(belief(&bayes, bad(1)), regrown);
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
        both_running(&mut bayes);
        // The chain's root landed: trunk moved under the finder.
        let root = batch(2, trunk(), &[1], BatchState::Landed { tip: tip(2) });
        let finder = testing(3, trunk(), &[2], CiStatus::Pending);
        let mut input = observation(ready_all(&[2, 4]), vec![root, finder]);
        input.trunk = tip(2);
        input.trunk_position = 1;
        input.departed = vec![landed(1, 2)];
        let plan = step(&mut bayes, &input);
        assert_eq!(plan.retire, ids(&[2]));
        // One lane is the finder's; 4 takes the other.
        assert_eq!(plan.start, vec![start(4, tip(2), &[4])]);

        let finder = testing(3, trunk(), &[2], ci_failed());
        let mut input = observation(ready_all(&[2, 4]), vec![finder]);
        input.trunk = tip(2);
        input.trunk_position = 1;
        let plan = step(&mut bayes, &input);
        assert_eq!(plan.retire, ids(&[3]));
        assert_eq!(plan.eject, vec![ejection(2, failed_by(3))]);
    }

    #[test]
    fn a_finder_passing_after_trunk_moved_under_it_leaves_its_suspect_regrown() {
        let mut bayes = bayes();
        with_finder(&mut bayes);
        both_running(&mut bayes);
        // The chain's root landed while the finder ran on the old trunk.
        let root = batch(2, trunk(), &[1], BatchState::Landed { tip: tip(2) });
        let finder = testing(3, trunk(), &[2], CiStatus::Pending);
        let mut input = observation(ready_all(&[2, 4]), vec![root, finder]);
        input.trunk = tip(2);
        input.trunk_position = 1;
        input.departed = vec![landed(1, 2)];
        step(&mut bayes, &input);

        let finder = testing(3, trunk(), &[2], CiStatus::Passed);
        let mut input = observation(ready_all(&[2, 4]), vec![finder]);
        input.trunk = tip(2);
        input.trunk_position = 1;
        step(&mut bayes, &input);
        // Its own CI's alibi at the trunk it reports, and the finder's at
        // the old trunk, not at 1's landing.
        assert_eq!(bayes.model.alibis(PrNumber(2)), Some(&[-24, 0][..]));
        let windows = bayes.model.windows(PrNumber(2), []).unwrap();
        assert_eq!(windows.last(), Some(&(0, 1, bayes.model.regrown(1))));
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
        assert_eq!(belief(&bayes, bad(2)), passed_once(&bayes));
        // 2, discounted, does not extend the pending {1}: the root, which
        // failed with 2, stands at about 0.31 and {2} on it passes at about
        // 0.94 (as the next test pins), so the extension scores about
        // 0.31 · 0.94 − 0.5 < 0. Its pass covered the run that blamed it,
        // so {2} is no suspect there; and with 1 in flight no finder can run.
        assert_eq!(plan.start, vec![]);
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
        // {1, 2} failed and 2's finder passed: Bad(2) is discounted and the
        // pair keeps most of the failure's share beside 1.
        let two = passed_once(&bayes);
        let given = 1.0 - 0.98 * (1.0 - prior()) * 0.999;
        let fail = 1.0 - 0.98 * (1.0 - prior()) * (1.0 - two) * 0.998 * 0.999;
        // The current stretch, opened by the finder's pass, at the floor.
        let expected = 0.98 * (1.0 - two) * 0.998 * given / fail * 0.999;
        let priced = bayes.p_pass(&inference, &members(&[2]), &members(&[1]));
        assert!(
            (priced - expected).abs() < 1e-9,
            "{priced} against {expected}"
        );
        assert!((priced - 0.938).abs() < 5e-4, "{priced}");
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
            input.trunk_position = 1;
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
            input.trunk_position = 1;
            assert!(step(&mut bayes, &input).land.is_empty());
        }
    }

    #[test]
    fn a_fresh_scheduler_starts_above_every_tracked_id() {
        for speculation in [Speculation::Chain, Speculation::Tree] {
            let mut bayes = Bayes::new(BayesConfig {
                speculation,
                ..params()
            });
            let landed = batch(7, trunk(), &[9], BatchState::Landed { tip: tip(7) });
            let plan = step(&mut bayes, &observation(ready_all(&[1, 2]), vec![landed]));
            assert_eq!(
                plan.start,
                vec![start(8, trunk(), &[1, 2])],
                "{speculation:?}"
            );
        }
    }

    #[test]
    fn a_component_too_improbable_to_resolve_is_read_over_fewer_runs_and_the_step_plans() {
        // Eleven chained runs whose causes all sit near the floor, one
        // landing above it: inference cannot resolve all of them, reads
        // fewer, and the step plans.
        let mut bayes = Bayes::new(params());
        let prs = (1..=12).map(|n| stale(ready(n), 1)).collect();
        let input = observation(prs, vec![]);
        bayes.model.sync(&input.prs, &BTreeSet::new(), 0, 0);
        for r in 1..=11 {
            failed_alone(&mut bayes.model, 100 + r, &members(&[r, r + 1]));
        }
        let plan = bayes.step(&input).unwrap();
        assert!(!plan.start.is_empty(), "{plan:?}");
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
        for speculation in [Speculation::Chain, Speculation::Tree] {
            let make = || {
                Bayes::new(BayesConfig {
                    speculation,
                    ..params()
                })
            };
            let (mut a, mut b) = (make(), make());
            for input in &inputs {
                assert_eq!(a.step(input).unwrap(), b.step(input).unwrap());
            }
            assert_eq!(a, b);
        }
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
        bayes.model.sync(&prs, &BTreeSet::new(), 0, 0);
        let inference = Inference::of(&bayes.model).unwrap();
        let p = |m: &[u64], b: &[u64]| bayes.p_pass(&inference, &members(m), &members(b));
        // Neither a flake nor a broken trunk.
        let (good, flake) = (1.0 - prior(), 0.98 * 0.999);
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
        let sets = bayes.suspects(
            &inference,
            &stacks,
            &members(&[1, 2, 3]),
            &BTreeSet::new(),
            false,
        );
        assert_eq!(sets, vec![members(&[1]), members(&[1, 2]), members(&[3])]);
        informative(model, &inference, &sets);
        // With 1 in flight, neither finder holding it is generated.
        let sets = bayes.suspects(
            &inference,
            &stacks,
            &members(&[2, 3]),
            &BTreeSet::new(),
            false,
        );
        assert_eq!(sets, vec![members(&[3])]);
        informative(model, &inference, &sets);
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
        assert_eq!(belief(&bayes, bad(1)), passed_once(&bayes));
        assert_eq!(belief(&bayes, bad(2)), passed_once(&bayes));
        assert_eq!(belief(&bayes, bad(3)), prior());
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

    #[test]
    fn a_unit_failed_twice_is_not_rerun_and_a_prefix_finder_starts() {
        // An `Atomic` stack of seven, each a landing behind: after one
        // failure a rerun is still the likeliest pass; after a second the
        // unit almost surely holds a bad member or trunk is broken, and its
        // prefixes are tried.
        for speculation in [Speculation::Tree, Speculation::Chain] {
            let mut bayes = Bayes::new(BayesConfig {
                max_batch: 16,
                p_interact: 0.001,
                w_age: 0.02,
                speculation,
                ..params()
            });
            let unit: Vec<u64> = (25..=31).collect();
            let mut prs = vec![stale(ready(25), 1)];
            prs.extend((26..=31).map(|n| stale(atomic_stacked(n, n - 1), 1)));
            let plan = step(&mut bayes, &observation(prs.clone(), vec![]));
            assert_eq!(
                plan.start,
                vec![start(1, trunk(), &unit)],
                "{speculation:?}"
            );
            let failed = |id| vec![testing(id, trunk(), &unit, ci_failed())];
            let plan = step(&mut bayes, &observation(prs.clone(), failed(1)));
            assert_eq!(
                plan.start,
                vec![start(2, trunk(), &unit)],
                "{speculation:?}"
            );
            let plan = step(&mut bayes, &observation(prs.clone(), failed(2)));
            assert_eq!(plan.retire, ids(&[2]), "{speculation:?}");
            assert_eq!(
                plan.start,
                vec![start(3, trunk(), &[25, 26, 27])],
                "{speculation:?}"
            );

            let inference = Inference::of(&bayes.model).unwrap();
            let unit = members(&unit);
            let p = bayes.p_pass(&inference, &unit, &[]);
            assert!(p < 0.06, "{speculation:?}: {p}");
            let score = bayes
                .score(&inference, &unit, &[], 1.0, &BTreeSet::new())
                .unwrap();
            assert!(
                score < bayes.params.speculate_above,
                "{speculation:?}: {score}"
            );
        }
    }

    #[test]
    fn p_pass_after_a_failure_is_the_joint_posterior() {
        let (mut bayes, _) = aged(0.0, &ready_all(&[1, 2]), &[]);
        bayes.model.failed(BatchId(9), &members(&[1, 2]), 0);
        let inference = Inference::of(&bayes.model).unwrap();
        let (p_flake, p) = (bayes.params.p_flake, prior());
        let none = (1.0 - p) * (1.0 - p) * (1.0 - 0.002);
        let fail = 1.0 - (1.0 - p_flake) * none * 0.999;
        // One member: it and the run's trunk, jointly; the run's others
        // explain it.
        let alone = bayes.p_pass(&inference, &members(&[1]), &[]);
        let others = 1.0 - (1.0 - p_flake) * (1.0 - p) * (1.0 - 0.002);
        let exact = (1.0 - p_flake) * (1.0 - p) * 0.999 * others / fail;
        assert!((alone - exact).abs() < 1e-12, "{alone} {exact}");
        // Both, their materialised pair and the trunk: the run's flake share.
        let both = bayes.p_pass(&inference, &members(&[1, 2]), &[]);
        let joint = (1.0 - p_flake) * p_flake * none * 0.999 / fail;
        assert!((both - joint).abs() < 1e-12, "{both} {joint}");
        let product = (1.0 - p_flake)
            * [bad(1), bad(2), pair(1, 2), Cause::Trunk(None)]
                .iter()
                .map(|&c| 1.0 - inference.posterior(&bayes.model, c))
                .product::<f64>();
        assert!(both < 0.6 * product, "{both} {product}");
    }

    #[test]
    fn units_order_by_the_joint_risk_of_their_members() {
        // The unit {1, 2} failed once: the joint chance it fails again is
        // about 0.83, the product of its marginals 0.68; 3 alone is at 0.75.
        let prs = vec![ready(1), atomic_stacked(2, 1), stale(ready(3), 692)];
        let (mut bayes, input) = aged(0.0, &prs, &[]);
        bayes.model.failed(BatchId(9), &members(&[1, 2]), 0);
        let inference = Inference::of(&bayes.model).unwrap();
        let (p_flake, p) = (bayes.params.p_flake, prior());
        let none = (1.0 - p) * (1.0 - p);
        let joint = 1.0 - inference.p_none(&bayes.model, &BTreeSet::from([bad(1), bad(2)]));
        let other = 1.0 - (1.0 - p_flake) * 0.999;
        let exact = 1.0 - other * none / (1.0 - (1.0 - p_flake) * none * 0.999);
        assert!((joint - exact).abs() < 1e-12, "{joint} {exact}");
        let three = belief(&bayes, bad(3));
        assert!(joint > three && three > 0.7, "{joint} {three}");
        let heads: BTreeMap<PrNumber, &Sha> =
            input.prs.iter().map(|p| (p.number, &p.head)).collect();
        let units = bayes.units(&inference, &Stacks::new(&input), &heads, &BTreeSet::new());
        assert_eq!(units, vec![members(&[3]), members(&[1, 2])]);
    }

    /// `n` pull requests merged by another hand, numbered from `from`.
    fn merges(from: u64, n: u64) -> Vec<Departure> {
        (from..from + n)
            .map(|k| Departure::new(PrNumber(k), sha(k), Departed::Merged))
            .collect()
    }

    /// An observation of `prs` and `batches` on trunk `tip(1000 + at)`, the
    /// trunk at position `at`, bringing `departed`.
    fn on(
        at: u64,
        prs: Vec<PrSnapshot>,
        batches: Vec<BatchStatus>,
        departed: Vec<Departure>,
    ) -> Observation {
        let mut input = observation(prs, batches);
        input.trunk = tip(1000 + at);
        input.trunk_position = at;
        input.departed = departed;
        input
    }

    #[test]
    fn the_trunk_position_is_the_observation_s_and_a_run_is_read_where_it_started() {
        let mut bayes = bayes();
        let prs = |at: u32| vec![stale(ready(1), at), stale(ready(2), BEHIND + at)];
        let mut departed = merges(900, 1);
        departed.push(landed(9, 7));
        departed.push(Departure::new(PrNumber(8), sha(8), Departed::Ejected));
        bayes.step(&on(2, prs(2), vec![], departed)).unwrap();
        assert_eq!(bayes.model.position(), 2);
        let run = |ci| testing(5, tip(1002), &[2], ci);
        bayes
            .step(&on(2, prs(2), vec![run(CiStatus::Pending)], vec![]))
            .unwrap();
        // Three merges while 5 ran: its pass covers the trunk it started on.
        bayes
            .step(&on(5, prs(5), vec![run(CiStatus::Passed)], merges(910, 3)))
            .unwrap();
        assert_eq!(bayes.model.position(), 5);
        assert_eq!(bayes.tracked[&BatchId(5)].covered, 2);
        assert_eq!(bayes.model.alibis(PrNumber(2)), Some(&[2][..]));
        assert_eq!(bayes.model.origin(PrNumber(1)), Some(0));
        assert_eq!(bayes.model.alibis(PrNumber(1)), Some(&[][..]));
    }

    #[test]
    fn a_trunk_change_the_queue_did_not_make_regrows_every_queued_pull_request_and_keeps_its_failed_runs_until_it_is_ejected()
     {
        let mut bayes = bayes();
        step(&mut bayes, &on(0, vec![fresh(1), fresh(2)], vec![], vec![]));
        let content = bayes.model.discounted(params().p_interact, 1);
        assert!((belief(&bayes, bad(1)) - content).abs() < 1e-15);
        // Another hand moved trunk by 3: no departure, every `behind` rises.
        let prs = || vec![stale(ready(1), 3), stale(ready(2), 3)];
        step(&mut bayes, &on(3, prs(), vec![], vec![]));
        assert_eq!(bayes.model.origin(PrNumber(1)), Some(0));
        let regrown = bayes.model.regrown(3);
        assert_eq!(
            belief(&bayes, bad(1)),
            1.0 - (1.0 - content) * (1.0 - regrown)
        );

        // The three changes raise the stretch's trunk with the floor.
        let trunk_now = belief(&bayes, Cause::Trunk(None));
        assert!((trunk_now - (1.0 - 0.999f64.powi(4))).abs() < 1e-15);

        let failed = |id| testing(id, tip(1003), &[1], ci_failed());
        let plan = step(&mut bayes, &on(3, prs(), vec![failed(50)], vec![]));
        assert!(plan.eject.is_empty(), "{plan:?}");
        let runs: Vec<(BatchId, &BTreeSet<Cause>)> = bayes
            .model
            .evidence()
            .iter()
            .map(|run| (run.batch, &run.exercised))
            .collect();
        assert_eq!(runs, vec![(BatchId(50), &BTreeSet::from([bad(1)]))]);

        // A second failure in the stretch the changes raised leaves 1 under
        // its trunk; one after a pass on trunk singles it out.
        let mut same = bayes.clone();
        let plan = step(&mut same, &on(3, prs(), vec![failed(52)], vec![]));
        assert!(plan.eject.is_empty(), "{plan:?}");
        bayes.model.passed(BatchId(51), &[], 3, true);
        let plan = step(&mut bayes, &on(3, prs(), vec![failed(52)], vec![]));
        assert_eq!(plan.eject, vec![ejection(1, failed_by(52))]);
    }

    #[test]
    fn a_foreign_change_raises_the_current_stretch() {
        let mut bayes = bayes();
        let trunk_now = |bayes: &Bayes| belief(bayes, Cause::Trunk(bayes.model.current()));
        let floor = |changes: i32| 1.0 - (1.0 - 0.001f64).powi(1 + changes);
        // A first observation compares with nothing, whatever its position.
        step(&mut bayes, &on(7, ready_all(&[1]), vec![], vec![]));
        assert!((trunk_now(&bayes) - floor(0)).abs() < 1e-15);
        // Two changes by another hand, with no departure.
        step(&mut bayes, &on(9, ready_after(&[1], 2), vec![], vec![]));
        assert!((trunk_now(&bayes) - floor(2)).abs() < 1e-15);
        // A landing by the queue is none; a merge by another hand is one.
        let departed = vec![landed(5, 4), merges(6, 1).remove(0)];
        step(&mut bayes, &on(11, ready_after(&[1], 4), vec![], departed));
        assert!((trunk_now(&bayes) - floor(3)).abs() < 1e-15);
    }

    #[test]
    fn a_landing_raises_the_stretch_current_when_it_is_read() {
        // 9 failed before anything here started, which keeps every stretch
        // since in the evidence's span. {1}, batch 1, passes on trunk and
        // lands; with `finder`, 2's finder, batch 2, started on trunk after
        // it and without it, passes beside it.
        let begin = |finder: bool| {
            let mut bayes = bayes();
            bayes
                .model
                .sync(&ready_all(&[1, 2, 3, 9]), &BTreeSet::new(), 0, 0);
            bayes.model.failed(BatchId(0), &members(&[9]), 0);
            let mut batches = vec![testing(1, trunk(), &[1], CiStatus::Passed)];
            if finder {
                bayes.finders.insert(BatchId(2));
                batches.push(testing(2, trunk(), &[2], CiStatus::Passed));
            }
            let plan = step(&mut bayes, &observation(ready_all(&[1, 2, 3, 9]), batches));
            assert_eq!(plan.land, ids(&[1]));
            // The chance 1 was bad, given its own pass.
            let beta = passed_once(&bayes);
            (bayes, beta)
        };
        let landed_batch = || batch(1, trunk(), &[1], BatchState::Landed { tip: tip(1) });
        let after = |batches: Vec<BatchStatus>, departed: Vec<Departure>| {
            let mut input = observation(ready_after(&[2, 3, 9], 1), batches);
            input.trunk = tip(1);
            input.trunk_position = 1;
            input.departed = departed;
            input
        };
        let stretch = |bayes: &Bayes, id: u64| belief(bayes, Cause::Trunk(Some(BatchId(id))));
        let close = |a: f64, b: f64| assert!((a - b).abs() < 1e-15, "{a} against {b}");
        let floor = 1.0 - (1.0 - 0.001);

        // Alone: its own pass's stretch carries it once the landing is read.
        let (mut bayes, beta) = begin(false);
        close(stretch(&bayes, 1), floor);
        step(&mut bayes, &after(vec![landed_batch()], vec![landed(1, 1)]));
        let raised = 1.0 - (1.0 - 0.001) * (1.0 - beta);
        close(stretch(&bayes, 1), raised);
        assert!(bayes.landing_breaks.is_empty());

        // Beside the finder's pass: the finder's stretch, the current one,
        // where every run started after the landing falls.
        let (mut bayes, beta) = begin(true);
        step(&mut bayes, &after(vec![landed_batch()], vec![landed(1, 1)]));
        assert_eq!(bayes.model.current(), Some(BatchId(2)));
        close(stretch(&bayes, 2), 1.0 - (1.0 - 0.001) * (1.0 - beta));
        close(stretch(&bayes, 1), floor);
        step(
            &mut bayes,
            &after(vec![testing(4, tip(1), &[3], ci_failed())], vec![]),
        );
        let run = bayes.model.evidence().back().unwrap();
        assert_eq!(run.batch, BatchId(4));
        assert!(
            bayes
                .model
                .effective(run)
                .any(|cause| cause == Cause::Trunk(Some(BatchId(2))))
        );

        // A batch on its tip passes as the landing is read: the landing is
        // credited first, and the new pass opens a stretch at the floor.
        let (mut bayes, beta) = begin(true);
        let on_tip = testing(3, tip(1), &[3], CiStatus::Passed);
        step(
            &mut bayes,
            &after(vec![landed_batch(), on_tip], vec![landed(1, 1)]),
        );
        assert_eq!(bayes.model.current(), Some(BatchId(3)));
        close(stretch(&bayes, 2), 1.0 - (1.0 - 0.001) * (1.0 - beta));
        close(stretch(&bayes, 3), floor);
    }

    #[test]
    fn a_pass_on_a_dead_base_opens_no_stretch_and_one_on_a_tip_on_trunk_does() {
        let mut bayes = bayes();
        let batches = vec![
            testing(1, moved(), &[1], CiStatus::Passed),
            testing(2, trunk(), &[2], CiStatus::Pending),
            testing(3, tip(2), &[3], CiStatus::Passed),
        ];
        step(&mut bayes, &observation(ready_all(&[1, 2, 3]), batches));
        assert_eq!(bayes.model.alibis(PrNumber(1)), Some(&[0][..]));
        assert_eq!(bayes.model.current(), Some(BatchId(3)));
        assert_eq!(bayes.model.stretch(BatchId(3)), None);
    }

    #[test]
    fn a_batch_inherited_on_a_base_the_scheduler_never_saw_is_covered_at_the_position_now_and_retired()
     {
        for speculation in [Speculation::Chain, Speculation::Tree] {
            let mut bayes = Bayes::new(BayesConfig {
                speculation,
                ..params()
            });
            let inherited = testing(1, moved(), &[1], CiStatus::Pending);
            let plan = step(
                &mut bayes,
                &observation(ready_all(&[1, 2]), vec![inherited]),
            );
            assert_eq!(bayes.tracked[&BatchId(1)].covered, 0, "{speculation:?}");
            assert_eq!(plan.retire, ids(&[1]), "{speculation:?}");
        }
    }

    #[test]
    fn a_batch_first_seen_after_another_hand_moved_trunk_is_covered_at_the_trunk_it_started_on() {
        let mut bayes = bayes();
        step(&mut bayes, &on(0, vec![fresh(1), fresh(2)], vec![], vec![]));
        // A batch started on that trunk passed, and another hand moved trunk
        // by 3, before the next read.
        let passed = testing(5, tip(1000), &[1], CiStatus::Passed);
        let prs = vec![stale(ready(1), 3), stale(ready(2), 3)];
        step(&mut bayes, &on(3, prs, vec![passed], vec![]));
        assert_eq!(bayes.tracked[&BatchId(5)].covered, 0);
        assert_eq!(bayes.model.alibis(PrNumber(1)), Some(&[0][..]));
        let windows = bayes.model.windows(PrNumber(1), []).unwrap();
        assert_eq!(windows.last(), Some(&(0, 3, bayes.model.regrown(3))));
    }

    #[test]
    fn a_batch_on_a_landed_tip_is_covered_at_that_tip_s_position() {
        let mut bayes = bayes();
        let passed = testing(2, trunk(), &[1], CiStatus::Passed);
        let plan = step(&mut bayes, &observation(ready_all(&[1, 3]), vec![passed]));
        assert_eq!(plan.land, ids(&[2]));
        // 2 landed and batch 3 started on its tip; another hand then pushed
        // two changes on top before the read.
        let landed_batch = batch(2, trunk(), &[1], BatchState::Landed { tip: tip(2) });
        let child = testing(3, tip(2), &[3], CiStatus::Pending);
        let input = on(
            3,
            ready_all(&[3]),
            vec![landed_batch, child],
            vec![landed(1, 2)],
        );
        step(&mut bayes, &input);
        assert_eq!(bayes.tracked[&BatchId(2)].covered, 1);
        assert_eq!(bayes.tracked[&BatchId(3)].covered, 1);
    }

    #[test]
    fn a_run_read_at_its_own_trunk_does_not_single_out_what_later_landings_may_have_broken() {
        let mut bayes = bayes();
        let prs = |at: u32| vec![stale(ready(1), 1 + at), stale(ready(2), 1 + at)];
        bayes.model.sync(&prs(0), &BTreeSet::new(), 0, 0);
        bayes.model.failed(BatchId(1), &members(&[1, 2]), 0);
        bayes.model.sync(&prs(2000), &BTreeSet::new(), 2000, 0);
        bayes.model.failed(BatchId(2), &members(&[1]), 2000);
        let inference = Inference::of(&bayes.model).unwrap();
        let then = inference.run_posteriors(0);
        assert_eq!(then[0].0, bad(1));
        assert!(then[0].1 < 0.1, "{then:?}");
        assert!(inference.posterior(&bayes.model, bad(1)) > 0.9);
        let runs = (0..2).map(|r| (Some(BatchId(r as u64 + 1)), inference.run_posteriors(r)));
        assert_eq!(
            singled_out(runs, 0.9, 0.1),
            BTreeMap::from([(bad(1), Some(BatchId(2)))])
        );
    }

    /// The chain at `sim.toml`'s settings, with the test priors.
    fn sim_chain() -> Bayes {
        Bayes::new(BayesConfig {
            max_batch: 16,
            w_age: 0.02,
            ..params()
        })
    }

    #[test]
    fn a_run_is_not_eroded_by_the_landings_during_it() {
        // 446 on 445, `Atomic` 447 on 446. 445 and 446 have their origin at
        // 0, their own content alone; 447 is 178 landings stale,
        // about 0.3. {445, 446, 447} starts three landings later and fails
        // after twenty more, each landed by the queue.
        let prs = |at: u32| {
            vec![
                stale(ready(445), at),
                stale(stacked(446, 445), at),
                stale(atomic_stacked(447, 446), 178 + at),
            ]
        };
        let mut bayes = sim_chain();
        let run = |ci| testing(2, tip(1003), &[445, 446, 447], ci);
        let landings = |from: u64, n: u64| (from..from + n).map(|k| landed(k, 1)).collect();
        bayes
            .step(&on(
                3,
                prs(3),
                vec![run(CiStatus::Pending)],
                landings(900, 3),
            ))
            .unwrap();
        let plan = bayes
            .step(&on(23, prs(23), vec![run(ci_failed())], landings(910, 20)))
            .unwrap();
        assert_eq!(plan.eject, vec![ejection(447, failed_by(2))]);
    }

    #[test]
    fn a_culprit_beneath_every_other_suspect_gets_its_ancestors_tried_alone() {
        // The stack 445–449, 446 onward `Atomic`. 445 and 446 have their
        // origin at 0; 447 is 178 landings stale, 448 and 449 twenty. The whole stack
        // starts ten landings later and fails: 447 is about 0.73, 448 and
        // 449 about 0.14, 445 and 446 about 0.05.
        let mut prs = vec![stale(ready(445), 10), stale(atomic_stacked(446, 445), 10)];
        prs.push(stale(atomic_stacked(447, 446), 188));
        prs.push(stale(atomic_stacked(448, 447), 30));
        prs.push(stale(atomic_stacked(449, 448), 30));
        let stack = [445, 446, 447, 448, 449];
        let failed = testing(1, tip(1010), &stack, ci_failed());
        let input = on(10, prs.clone(), vec![failed], merges(900, 10));
        let mut bayes = sim_chain();
        let plan = bayes.step(&input).unwrap();

        let inference = Inference::of(&bayes.model).unwrap();
        let posterior = |n| inference.posterior(&bayes.model, bad(n));
        assert!(posterior(447) < 0.9 && posterior(447) > 0.7);
        assert!(posterior(449) > posterior(445));
        // Every set the top three and the two `Bad`s give holds 447; only
        // the ancestors finder leaves it out.
        let stacks = Stacks::new(&observation(prs, vec![]));
        let suspects = |pool: &[u64], beneath: &[u64]| {
            let beneath: BTreeSet<PrNumber> = numbers(beneath).into_iter().collect();
            bayes.suspects(&inference, &stacks, &members(pool), &beneath, false)
        };
        assert_eq!(
            suspects(&stack, &[]),
            vec![
                members(&[445, 446, 447]),
                members(&[445, 446]),
                members(&[445, 446, 447, 448]),
                members(&stack),
            ]
        );
        // Never a member beneath the node; a lone suspect has no ancestors.
        assert_eq!(
            suspects(&stack[1..], &[445]),
            vec![
                members(&[446, 447]),
                members(&[446]),
                members(&[446, 447, 448]),
                members(&[446, 447, 448, 449]),
            ]
        );
        assert_eq!(
            suspects(&stack[2..], &[445, 446]),
            vec![
                members(&[447]),
                members(&[447, 448]),
                members(&[447, 448, 449]),
            ]
        );
        // The ancestors' pass is the likeliest and the best use of the lane.
        assert_eq!(plan.start, vec![start(2, tip(1010), &[445, 446])]);
    }

    /// The stack #235–#238 and the `Atomic` unit #843–#849, every member a
    /// landing behind, after three failures at position 0, each in a stretch
    /// of its own: the stack alone, both together, and the unit alone.
    /// Returns the pull requests.
    fn two_stacks_failed(bayes: &mut Bayes) -> Vec<PrSnapshot> {
        let mut prs = vec![stale(ready(235), 1)];
        prs.extend((236..=238).map(|n| stale(stacked(n, n - 1), 1)));
        prs.push(stale(ready(843), 1));
        prs.extend((844..=849).map(|n| stale(atomic_stacked(n, n - 1), 1)));
        bayes.model.sync(&prs, &BTreeSet::new(), 0, 0);
        let stack: Vec<u64> = (235..=238).collect();
        let unit: Vec<u64> = (843..=849).collect();
        let both: Vec<u64> = stack.iter().chain(&unit).copied().collect();
        failed_alone(&mut bayes.model, 1, &members(&stack));
        failed_alone(&mut bayes.model, 2, &members(&both));
        failed_alone(&mut bayes.model, 3, &members(&unit));
        prs
    }

    #[test]
    fn a_suspect_in_flight_does_not_leave_its_component_without_a_finder() {
        let mut bayes = sim_chain();
        let prs = two_stacks_failed(&mut bayes);
        let inference = Inference::of(&bayes.model).unwrap();
        assert_eq!(inference.components().len(), 1);
        let posterior = |n| inference.posterior(&bayes.model, bad(n));
        assert!(posterior(235) >= posterior(236) && posterior(238) > posterior(843));
        let stacks = Stacks::new(&observation(prs.clone(), vec![]));
        let suspects = |pool: &[u64]| {
            let sets = bayes.suspects(&inference, &stacks, &members(pool), &BTreeSet::new(), false);
            informative(&bayes.model, &inference, &sets);
            sets
        };
        let everything: Vec<u64> = (235..=238).chain(843..=849).collect();
        // Nothing in flight: #235's stack holds the three most probable.
        assert_eq!(
            suspects(&everything),
            vec![
                members(&[235]),
                members(&[235, 236]),
                members(&[235, 236, 237]),
            ]
        );
        // #235 in flight: every closure of its stack is passed over, and the
        // unit's are counted.
        assert_eq!(
            suspects(&everything[1..]),
            vec![
                members(&[843]),
                members(&[843, 844]),
                members(&[843, 844, 845]),
            ]
        );

        // The step that reads the unit's second failure with #235's finder
        // pending starts a finder of #843, not the unit again.
        let unit: Vec<u64> = (843..=849).collect();
        let batches = vec![
            testing(8, trunk(), &[235], CiStatus::Pending),
            testing(9, trunk(), &unit, ci_failed()),
        ];
        bayes.finders.insert(BatchId(8));
        let plan = step(&mut bayes, &observation(prs, batches));
        assert!(
            plan.start.iter().all(|s| s.prs != numbers(&unit)),
            "{plan:?}"
        );
        assert!(
            plan.start.iter().any(|s| {
                s.prs.contains(&PrNumber(843)) && s.prs.iter().all(|pr| unit.contains(&pr.0))
            }),
            "{plan:?}"
        );
    }

    #[test]
    fn the_two_bad_set_is_of_the_two_most_probable_suspects_whose_closures_can_run() {
        let prs = vec![
            stale(ready(1), 40),
            stale(ready(2), 35),
            stale(ready(3), 30),
            ready(4),
        ];
        let mut bayes = bayes();
        bayes.model.sync(&prs, &BTreeSet::new(), 0, 0);
        bayes.model.failed(BatchId(1), &members(&[1, 2, 3, 4]), 0);
        let inference = Inference::of(&bayes.model).unwrap();
        let stacks = Stacks::new(&observation(prs, vec![]));
        let suspects = |pool: &[u64]| {
            let sets = bayes.suspects(&inference, &stacks, &members(pool), &BTreeSet::new(), false);
            informative(&bayes.model, &inference, &sets);
            sets
        };
        assert_eq!(
            suspects(&[1, 2, 3, 4]),
            vec![
                members(&[1]),
                members(&[2]),
                members(&[3]),
                members(&[1, 2])
            ]
        );
        assert_eq!(
            suspects(&[2, 3, 4]),
            vec![
                members(&[2]),
                members(&[3]),
                members(&[4]),
                members(&[2, 3])
            ]
        );
    }

    #[test]
    fn ancestors_are_proposed_alone_only_while_a_run_blames_them_beside_their_suspect() {
        // The ancestors' pass covers the failed run's trunk: the run blames
        // #445 and #446 beside #447 only as discounted, tested causes.
        let at = |landings: u32| {
            let mut prs = vec![
                stale(ready(445), landings),
                stale(atomic_stacked(446, 445), landings),
            ];
            prs.push(stale(atomic_stacked(447, 446), 178 + landings));
            prs.push(stale(atomic_stacked(448, 447), 20 + landings));
            prs.push(stale(atomic_stacked(449, 448), 20 + landings));
            prs
        };
        let stack = [445, 446, 447, 448, 449];
        let mut bayes = sim_chain();
        let failed = testing(1, tip(1010), &stack, ci_failed());
        bayes
            .step(&on(10, at(10), vec![failed], merges(900, 10)))
            .unwrap();
        bayes.model.passed_after(&members(&[445, 446]), 10);
        let stacks = Stacks::new(&observation(at(10), vec![]));
        let suspects = |bayes: &Bayes| {
            let inference = Inference::of(&bayes.model).unwrap();
            let sets = bayes.suspects(
                &inference,
                &stacks,
                &members(&stack),
                &BTreeSet::new(),
                false,
            );
            informative(&bayes.model, &inference, &sets);
            sets
        };
        let sets = suspects(&bayes);
        assert!(sets.contains(&members(&[445, 446, 447])), "{sets:?}");
        assert!(!sets.contains(&members(&[445, 446])), "{sets:?}");
        // Three landings later they could have broken since, but the run at
        // 10 is still covered by their pass.
        bayes.model.sync(&at(13), &BTreeSet::new(), 13, 0);
        assert!(
            bayes
                .model
                .would_blame(&members(&[445]))
                .contains(&bad(445))
        );
        let sets = suspects(&bayes);
        assert!(sets.contains(&members(&[445, 446, 447])), "{sets:?}");
        assert!(!sets.contains(&members(&[445, 446])), "{sets:?}");
        // A failure of the stack at 13, after their pass, blames them
        // uncovered beside #447, and they are proposed again.
        bayes.model.failed(BatchId(1_000_001), &members(&stack), 13);
        let sets = suspects(&bayes);
        assert!(sets.contains(&members(&[445, 446])), "{sets:?}");
    }

    #[test]
    fn a_passed_base_is_not_proposed_as_its_suspects_ancestors() {
        // The stack #355–#363 failed whole; #355–#359 then passed against
        // that run's trunk, which their pass covers. The run blames
        // #360–#363 at about 0.24 each, and #355–#359, discounted, at about
        // 0.005.
        let mut prs = vec![ready(355)];
        prs.extend((356..=363).map(|n| stacked(n, n - 1)));
        let mut bayes = bayes();
        bayes.model.sync(&prs, &BTreeSet::new(), 0, 0);
        let stack: Vec<u64> = (355..=363).collect();
        bayes.model.failed(BatchId(1), &members(&stack), 0);
        bayes.model.passed_after(&members(&stack[..5]), 0);
        let inference = Inference::of(&bayes.model).unwrap();
        let stacks = Stacks::new(&observation(prs, vec![]));
        let sets = bayes.suspects(
            &inference,
            &stacks,
            &members(&stack),
            &BTreeSet::new(),
            false,
        );
        informative(&bayes.model, &inference, &sets);
        assert_eq!(sets[0], members(&stack[..6]));
        assert!(!sets.contains(&members(&stack[..5])), "{sets:?}");
    }

    #[test]
    fn a_passed_pair_s_closure_is_proposed_once_per_trunk_position() {
        let prs: Vec<PrSnapshot> = (1..=4).map(fresh).collect();
        let mut bayes = bayes();
        bayes.model.sync(&prs, &BTreeSet::new(), 0, 0);
        let all = [1, 2, 3, 4];
        bayes.model.failed(BatchId(1), &members(&all), 0);
        // Each passes alone at 0: every `Bad` is covered in the run, and
        // only its pairs can be walked.
        for n in all {
            bayes.model.passed_after(&members(&[n]), 0);
        }
        let stacks = Stacks::new(&observation(prs.clone(), vec![]));
        let suspects = |bayes: &Bayes| {
            let inference = Inference::of(&bayes.model).unwrap();
            let sets = bayes.suspects(&inference, &stacks, &members(&all), &BTreeSet::new(), false);
            informative(&bayes.model, &inference, &sets);
            sets
        };
        let pass_each = |bayes: &mut Bayes, sets: &[Vec<Member>], at: i64| {
            for set in sets {
                bayes.model.passed_after(set, at);
            }
        };
        // The six pairs tie at the prior: cause order takes the first three.
        let sets = suspects(&bayes);
        assert_eq!(
            sets,
            vec![members(&[1, 2]), members(&[1, 3]), members(&[1, 4])]
        );
        pass_each(&mut bayes, &sets, 0);
        // On rank alone: the three passed pairs sit at d(0.002, 1), the
        // other three at the prior, which fill the slots with or without
        // the coverage rule.
        let sets = suspects(&bayes);
        assert_eq!(
            sets,
            vec![members(&[2, 3]), members(&[2, 4]), members(&[3, 4])]
        );
        pass_each(&mut bayes, &sets, 0);
        // Every pair, and every `Bad`, is covered at 0: nothing is
        // proposed. This is the one round the pair's coverage decides;
        // without it the first three pairs, still above the floor, would be
        // proposed again at the position their passes covered.
        assert_eq!(suspects(&bayes), Vec::<Vec<Member>>::new());
        // Trunk moves to 1 and the four fail together there. The sync reads each pull
        // request's own CI at 1, an alibi covering its `Bad` there; every
        // pair blames the run uncovered, all six tie at d(0.002, 1), and
        // cause order picks the first three again.
        bayes.model.sync(&prs, &BTreeSet::new(), 1, 0);
        bayes.model.failed(BatchId(2_000_000), &members(&all), 1);
        assert_eq!(
            suspects(&bayes),
            vec![members(&[1, 2]), members(&[1, 3]), members(&[1, 4])]
        );
    }

    /// The score of a batch of `members` on trunk after `bayes`'s last step,
    /// at the beliefs that step planned with.
    fn score_now(bayes: &Bayes, members: &[Member]) -> f64 {
        let inference = Inference::of(&bayes.model).unwrap();
        bayes
            .score(&inference, members, &[], 1.0, &BTreeSet::new())
            .unwrap()
    }

    fn trunk_posterior(bayes: &Bayes, stretch: Option<BatchId>) -> f64 {
        let inference = Inference::of(&bayes.model).unwrap();
        inference.posterior(&bayes.model, Cause::Trunk(stretch))
    }

    fn bad_posterior(bayes: &Bayes, pr: u64) -> f64 {
        let inference = Inference::of(&bayes.model).unwrap();
        inference.posterior(&bayes.model, bad(pr))
    }

    /// `batch` with its members at their original heads, as a run started
    /// on trunk with verdict `ci`.
    fn run_of(start: &BatchStart, ci: CiStatus) -> BatchStatus {
        let members: Vec<u64> = start.prs.iter().map(|pr| pr.0).collect();
        testing(start.id.0, start.base.clone(), &members, ci)
    }

    /// The parameters of the shipped GitHub example, `speculation` aside.
    fn shipped(speculation: Speculation) -> BayesConfig {
        BayesConfig {
            lanes: 2,
            max_batch: 8,
            p_interact: 0.001,
            p_flake: 0.03,
            p_miss: 0.02,
            p_trunk: 0.001,
            run_cost: 0.5,
            w_reject: 1.0,
            w_age: 0.0,
            eject_above: 0.9,
            others_below: 0.1,
            speculate_above: 0.0,
            speculation,
        }
    }

    /// Pull requests 1 to 4, fresh, after `{1, 2}`, `{3}` and `{4}` failed
    /// on trunk with no pass between: the plan of that step.
    fn three_failed(bayes: &mut Bayes) -> Plan {
        let batches = vec![
            testing(1, trunk(), &[1, 2], ci_failed()),
            testing(2, trunk(), &[3], ci_failed()),
            testing(3, trunk(), &[4], ci_failed()),
        ];
        step(bayes, &observation(four_fresh(), batches))
    }

    fn four_fresh() -> Vec<PrSnapshot> {
        (1..=4).map(fresh).collect()
    }

    #[test]
    fn failures_between_two_passes_make_trunk_the_suspect_and_a_probe_the_candidate() {
        let mut bayes = bayes();
        let plan = three_failed(&mut bayes);
        assert_eq!(plan.start, vec![start(4, trunk(), &[])]);
        assert!(plan.eject.is_empty(), "{plan:?}");
        assert_eq!(plan.retire, ids(&[1, 2, 3]));
        assert!(trunk_posterior(&bayes, None) > params().eject_above);
        assert!(score_now(&bayes, &[]) > params().speculate_above);

        // With the probe pending, nothing else is worth a lane, and liveness
        // waits on it.
        let probe = testing(4, trunk(), &[], CiStatus::Pending);
        let plan = step(&mut bayes, &observation(four_fresh(), vec![probe]));
        assert_eq!(plan, Plan::default());
        for set in [&[1, 2, 3, 4][..], &[1, 2], &[1], &[2], &[3], &[4]] {
            let score = score_now(&bayes, &members(set));
            assert!(score < params().speculate_above, "{set:?}: {score}");
        }
    }

    #[test]
    fn a_failed_probe_keeps_trunk_the_suspect_and_a_passed_one_resumes_the_queue() {
        let mut bayes = bayes();
        three_failed(&mut bayes);
        let probe = |id: u64, ci: CiStatus| testing(id, trunk(), &[], ci);

        let plan = step(
            &mut bayes,
            &observation(four_fresh(), vec![probe(4, ci_failed())]),
        );
        assert_eq!(plan.retire, ids(&[4]));
        assert_eq!(plan.start, vec![start(5, trunk(), &[])]);
        assert!(plan.eject.is_empty(), "{plan:?}");

        let plan = step(
            &mut bayes,
            &observation(four_fresh(), vec![probe(5, CiStatus::Passed)]),
        );
        assert_eq!(plan.retire, ids(&[5]));
        assert!(plan.land.is_empty(), "{plan:?}");
        assert!(plan.eject.is_empty(), "{plan:?}");
        assert_eq!(bayes.model.current(), Some(BatchId(5)));
        assert_eq!(plan.start[0], start(6, trunk(), &[1, 2, 3, 4]));
        let inference = Inference::of(&bayes.model).unwrap();
        let p = bayes.p_pass(&inference, &members(&[1, 2, 3, 4]), &[]);
        assert!(p > 0.95 && p < 1.0 - params().p_flake, "{p}");
        // The break's runs over pull requests stay, explained by their
        // stretch's trunk; the failed probe's, which held none, leaves.
        let runs: Vec<BatchId> = bayes.model.evidence().iter().map(|r| r.batch).collect();
        assert_eq!(runs, ids(&[1, 2, 3]));
        assert!(trunk_posterior(&bayes, None) > params().eject_above);
    }

    #[test]
    fn a_probe_is_never_landed_and_never_built_on() {
        for speculation in [Speculation::Chain, Speculation::Tree] {
            let mut bayes = Bayes::new(BayesConfig {
                speculation,
                ..params()
            });
            bayes.finders.insert(BatchId(1));
            let batches = vec![
                testing(1, trunk(), &[], CiStatus::Passed),
                testing(2, trunk(), &[1], CiStatus::Passed),
            ];
            let plan = step(&mut bayes, &observation(ready_all(&[1, 2]), batches));
            assert_eq!(plan.land, ids(&[2]), "{speculation:?}");
            assert_eq!(plan.retire, ids(&[1]), "{speculation:?}");
        }
        // Under the tree a pending probe is no node: a passed sibling on
        // trunk lands past it, and nothing is proposed on its tip.
        let mut bayes = Bayes::new(BayesConfig {
            speculation: Speculation::Tree,
            lanes: 3,
            ..params()
        });
        let batches = vec![
            testing(1, trunk(), &[], CiStatus::Pending),
            testing(2, trunk(), &[1], CiStatus::Passed),
        ];
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2]), batches));
        assert_eq!(plan.land, ids(&[2]));
        assert!(plan.retire.is_empty(), "{plan:?}");
        assert!(plan.start.iter().all(|s| s.base == tip(2)), "{plan:?}");
        // Once that landing is read, the probe's base is no longer trunk and
        // its verdict could not say whether trunk is broken: it is retired.
        let batches = vec![
            testing(1, trunk(), &[], CiStatus::Pending),
            batch(2, trunk(), &[1], BatchState::Landed { tip: tip(2) }),
        ];
        let mut input = observation(ready_all(&[2]), batches);
        input.trunk = tip(2);
        input.trunk_position = 1;
        input.departed = vec![landed(1, 2)];
        let plan = step(&mut bayes, &input);
        assert_eq!(plan.retire, ids(&[1, 2]));
    }

    #[test]
    fn a_probe_on_a_base_trunk_has_moved_past_is_retired_and_another_proposed() {
        for speculation in [Speculation::Chain, Speculation::Tree] {
            let mut bayes = Bayes::new(BayesConfig {
                speculation,
                ..params()
            });
            let plan = three_failed(&mut bayes);
            assert_eq!(plan.start, vec![start(4, trunk(), &[])], "{speculation:?}");
            // A change by another hand moves trunk under the pending probe.
            let mut input = observation(
                four_fresh(),
                vec![testing(4, trunk(), &[], CiStatus::Pending)],
            );
            input.trunk = moved();
            input.trunk_position = 1;
            assert!(!probe_pending(&input), "{speculation:?}");
            let plan = step(&mut bayes, &input);
            assert_eq!(plan.retire, ids(&[4]), "{speculation:?}");
            assert_eq!(plan.start, vec![start(5, moved(), &[])], "{speculation:?}");
        }
    }

    #[test]
    fn an_inherited_probe_is_retired_on_its_verdict() {
        for speculation in [Speculation::Chain, Speculation::Tree] {
            let mut bayes = Bayes::new(BayesConfig {
                speculation,
                lanes: 1,
                ..params()
            });
            let pending = testing(7, trunk(), &[], CiStatus::Pending);
            let plan = step(&mut bayes, &observation(ready_all(&[1]), vec![pending]));
            assert_eq!(plan, Plan::default(), "{speculation:?}");
            let passed = testing(7, trunk(), &[], CiStatus::Passed);
            let plan = step(&mut bayes, &observation(ready_all(&[1]), vec![passed]));
            assert_eq!(plan.retire, ids(&[7]), "{speculation:?}");
            assert!(plan.land.is_empty(), "{speculation:?}");
            assert_eq!(bayes.model.current(), Some(BatchId(7)), "{speculation:?}");
        }
    }

    /// Runs pull request 1 alone under `config`, failing every run that
    /// holds it and passing every probe, until a plan ejects it: each plan's
    /// start, and the plan that ejects.
    fn lone_until_ejected(config: BayesConfig, pr: PrSnapshot) -> (Bayes, Vec<BatchStart>, Plan) {
        let mut bayes = Bayes::new(config);
        let mut starts = Vec::new();
        let mut batches = Vec::new();
        loop {
            let plan = step(&mut bayes, &observation(vec![pr.clone()], batches));
            if !plan.eject.is_empty() {
                return (bayes, starts, plan);
            }
            assert_eq!(plan.start.len(), 1, "{plan:?}");
            let start = plan.start[0].clone();
            let ci = if start.prs.is_empty() {
                CiStatus::Passed
            } else {
                ci_failed()
            };
            batches = vec![run_of(&start, ci)];
            starts.push(start);
            assert!(starts.len() < 12, "never ejected: {starts:?}");
        }
    }

    #[test]
    fn a_lone_pull_request_failing_alone_is_found_across_a_probe() {
        let (_, starts, plan) = lone_until_ejected(params(), fresh(1));
        assert_eq!(
            starts,
            vec![
                start(1, trunk(), &[1]),
                start(2, trunk(), &[1]),
                start(3, trunk(), &[]),
                start(4, trunk(), &[1]),
                start(5, trunk(), &[1]),
            ]
        );
        assert_eq!(plan.eject, vec![ejection(1, failed_by(5))]);

        // The posteriors along the way: after the second failure trunk is
        // the likelier cause and the probe beats the re-run; after the
        // probe's pass and one more failure, 1 is likelier but not singled
        // out.
        let mut bayes = bayes();
        let mut walk = |batches: Vec<BatchStatus>| {
            step(&mut bayes, &observation(vec![fresh(1)], batches));
            let current = bayes.model.current();
            (
                trunk_posterior(&bayes, current),
                bad_posterior(&bayes, 1),
                score_now(&bayes, &[]),
                score_now(&bayes, &members(&[1])),
            )
        };
        walk(vec![]);
        walk(vec![testing(1, trunk(), &[1], ci_failed())]);
        let (broken, _, probe, rerun) = walk(vec![testing(2, trunk(), &[1], ci_failed())]);
        assert!((0.6..0.8).contains(&broken), "{broken}");
        assert!(probe > 0.0 && rerun < 0.0, "{probe} {rerun}");
        walk(vec![testing(3, trunk(), &[], CiStatus::Passed)]);
        let (_, bad1, _, rerun) = walk(vec![testing(4, trunk(), &[1], ci_failed())]);
        assert!((0.5..params().eject_above).contains(&bad1), "{bad1}");
        assert!(rerun > 0.0, "{rerun}");
    }

    #[test]
    fn a_stale_pull_request_failing_alone_is_found_across_a_probe_at_shipped_priors() {
        for speculation in [Speculation::Chain, Speculation::Tree] {
            let config = shipped(speculation);
            let (bayes, starts, plan) = lone_until_ejected(config, stale(ready(1), 3));
            assert_eq!(
                starts,
                vec![
                    start(1, trunk(), &[1]),
                    start(2, trunk(), &[1]),
                    start(3, trunk(), &[1]),
                    start(4, trunk(), &[]),
                    start(5, trunk(), &[1]),
                ],
                "{speculation:?}"
            );
            assert_eq!(
                plan.eject,
                vec![ejection(1, failed_by(5))],
                "{speculation:?}"
            );
            // Against 1, trunk broken in the first stretch and a flake in
            // the second: about `0.001 · 0.03` to 1's `0.003`.
            let ejected = bad_posterior(&bayes, 1);
            assert!(ejected > 0.98, "{speculation:?}: {ejected}");

            // After the third failure 1 is capped below ejection, and the
            // probe starts by liveness: nothing scores above the bar.
            let mut bayes = Bayes::new(config);
            let pr = || vec![stale(ready(1), 3)];
            step(&mut bayes, &observation(pr(), vec![]));
            for id in 1..=3 {
                step(
                    &mut bayes,
                    &observation(pr(), vec![testing(id, trunk(), &[1], ci_failed())]),
                );
            }
            let bad1 = bad_posterior(&bayes, 1);
            assert!(bad1 > 0.5 && bad1 < config.eject_above, "{bad1}");
            let probe = score_now(&bayes, &[]);
            let rerun = score_now(&bayes, &members(&[1]));
            assert!(
                rerun < probe && probe < config.speculate_above,
                "{probe} {rerun}"
            );
        }
    }

    #[test]
    fn the_tree_proposes_a_probe_only_on_trunk_and_one_at_a_time() {
        let mut bayes = Bayes::new(BayesConfig {
            speculation: Speculation::Tree,
            lanes: 3,
            ..params()
        });
        // A pending batch on trunk is a node beside trunk; three failures
        // make trunk the suspect. One probe starts, on trunk.
        let mut batches = vec![testing(9, trunk(), &[5], CiStatus::Pending)];
        batches.extend([
            testing(1, trunk(), &[1, 2], ci_failed()),
            testing(2, trunk(), &[3], ci_failed()),
            testing(3, trunk(), &[4], ci_failed()),
        ]);
        batches.sort_by_key(|b| b.id);
        let mut prs = four_fresh();
        prs.push(fresh(5));
        let plan = step(&mut bayes, &observation(prs.clone(), batches));
        let probes: Vec<&BatchStart> = plan.start.iter().filter(|s| s.prs.is_empty()).collect();
        assert_eq!(probes.len(), 1, "{plan:?}");
        assert_eq!(probes[0].base, trunk());

        // While it pends, no other probe is proposed.
        let probe = probes[0].id;
        let batches = vec![
            testing(9, trunk(), &[5], CiStatus::Pending),
            testing(probe.0, trunk(), &[], CiStatus::Pending),
        ];
        let plan = step(&mut bayes, &observation(prs, batches));
        assert!(plan.start.iter().all(|s| !s.prs.is_empty()), "{plan:?}");
    }

    #[test]
    fn a_passed_ancestors_set_is_started_once_and_the_suspect_is_found_across_its_pass() {
        // #32, fresh, under the `Atomic` stack #33 ← #34 ← #35; #33 and #34
        // ten landings past their origin, #35 fresh. The stack and its
        // closure of #34 have failed in one stretch.
        let config = shipped(Speculation::Chain);
        let prs = vec![
            fresh(32),
            stale(atomic_stacked(33, 32), 10),
            stale(atomic_stacked(34, 33), 10),
            stale(atomic_stacked(35, 34), 0),
        ];
        let stack = [32, 33, 34, 35];
        let mut bayes = Bayes::new(config);
        let mut starts: Vec<BatchStart> = Vec::new();
        let failed = vec![
            testing(1, trunk(), &stack, ci_failed()),
            testing(2, trunk(), &[32, 33, 34], ci_failed()),
        ];
        let plan = step(&mut bayes, &observation(prs.clone(), failed));
        starts.extend(plan.start.iter().cloned());
        let ancestors = start(3, trunk(), &[32, 33]);
        assert!(plan.start.contains(&ancestors), "{plan:?}");
        assert!(
            plan.start
                .iter()
                .filter(|s| **s != ancestors)
                .all(|s| !s.prs.iter().any(|pr| (32..=34).contains(&pr.0))),
            "{plan:?}"
        );

        // The ancestors pass on trunk and are retired: the unit cannot land
        // without #34. Their pass covers both failed runs and opens a
        // stretch.
        let mut batches: Vec<BatchStatus> = plan
            .start
            .iter()
            .map(|s| {
                let ci = if *s == ancestors {
                    CiStatus::Passed
                } else {
                    CiStatus::Pending
                };
                run_of(s, ci)
            })
            .collect();
        let plan = step(&mut bayes, &observation(prs.clone(), batches.clone()));
        starts.extend(plan.start.iter().cloned());
        assert_eq!(plan.retire, ids(&[3]));
        assert!(plan.land.is_empty() && plan.eject.is_empty(), "{plan:?}");
        // #34 is capped by the stretch its two failures share: about 0.83.
        let bad34 = bad_posterior(&bayes, 34);
        assert!(bad34 < config.eject_above, "{bad34}");
        // #33 outranks #32, about 1.7e-2 against 3e-5, so `closure(33)`,
        // `[32, 33]`, is the set the coverage gate keeps out.
        let (bad32, bad33) = (bad_posterior(&bayes, 32), bad_posterior(&bayes, 33));
        assert!(bad33 > bad32, "{bad33} {bad32}");
        let inference = Inference::of(&bayes.model).unwrap();
        let stacks = Stacks::new(&observation(prs.clone(), vec![]));
        let sets = bayes.suspects(
            &inference,
            &stacks,
            &members(&stack),
            &BTreeSet::new(),
            false,
        );
        assert!(sets.contains(&members(&[32, 33, 34])), "{sets:?}");
        assert!(!sets.contains(&members(&[32, 33])), "{sets:?}");
        // The extension starts: about 0.95, against 0.80 for the closure and
        // 1.41 for `[32, 33]` were it proposed.
        let extension = start(4, trunk(), &stack);
        assert_eq!(plan.start, vec![extension.clone()]);

        // The extension fails in the stretch the pass opened, and #34 is
        // singled out across the boundary: about 0.975. The ejecting plan
        // starts a probe alone, by liveness: its hypothetical failure leaves
        // trunk at about 0.04, so it scores `-run_cost`, and nothing else can
        // run.
        batches.retain(|b| b.id != BatchId(3));
        batches.push(run_of(&extension, ci_failed()));
        let plan = step(&mut bayes, &observation(prs, batches));
        starts.extend(plan.start.iter().cloned());
        assert_eq!(plan.eject, vec![ejection(34, failed_by(4))]);
        let bad34 = bad_posterior(&bayes, 34);
        assert!(bad34 > config.eject_above, "{bad34}");
        assert_eq!(plan.start, vec![start(5, trunk(), &[])]);
        let inference = Inference::of(&bayes.model).unwrap();
        let ejected = BTreeSet::from([PrNumber(34)]);
        let probe = bayes.score(&inference, &[], &[], 1.0, &ejected).unwrap();
        assert!(probe < config.speculate_above, "{probe}");

        let ancestors_started = starts
            .iter()
            .filter(|s| s.prs == numbers(&[32, 33]))
            .count();
        assert_eq!(ancestors_started, 1, "{starts:?}");
    }
}
