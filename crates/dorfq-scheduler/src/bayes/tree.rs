//! `speculation = "tree"`: every standing batch's tip is a node, and so is
//! trunk once the plan's landings run. A batch on a node lands when the node
//! becomes trunk and every older batch standing on the node fails; each
//! candidate is scored at that chance, and a free lane takes the best
//! candidate on any node. A batch whose node dies, or whose chance of
//! landing falls to zero, is cancelled.

use std::collections::{BTreeMap, BTreeSet};

use dorfq_core::{
    BatchStart, BatchState, CiStatus, Observation, Plan, PrNumber, SchedulerError, Sha,
};

use super::Bayes;
use super::infer::Inference;
use super::model::Cause;
use super::policy::{Fate, doomed, landing_members};
use crate::stacks::Stacks;
use crate::{Member, is_pending};

/// A hypothetical trunk: trunk once the plan's landings run, or the tip of
/// a standing batch.
#[derive(Debug, Clone, PartialEq)]
struct Node {
    /// The chance the node becomes trunk.
    p: f64,
    /// The members beneath the node beyond the observation's trunk, less
    /// those this plan lands.
    tree: Vec<Member>,
    /// For each standing batch on the node, oldest first, the chance it
    /// lands once the node is trunk: its `p_pass` while it pends, 1 once it
    /// passed.
    siblings: Vec<f64>,
}

impl Node {
    fn root() -> Node {
        Node {
            p: 1.0,
            tree: Vec::new(),
            siblings: Vec::new(),
        }
    }

    /// The chance a batch started on the node now lands: the node becomes
    /// trunk and every batch already on it fails.
    // TODO(bayes-joint-path-pass): the node's `p` and its siblings' chances
    // multiply per-batch pass chances as if independent; batches whose trees
    // share a failed run are not.
    fn weight(&self) -> f64 {
        self.p * self.siblings.iter().map(|q| 1.0 - q).product::<f64>()
    }
}

/// A batch the step could start.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    base: Sha,
    members: Vec<Member>,
    score: f64,
}

impl Candidate {
    /// Whether `self` is chosen over `other`: the higher score, ties by
    /// `(base, members)`.
    fn beats(&self, other: &Candidate) -> bool {
        match self.score.total_cmp(&other.score) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => {
                (&self.base, &self.members) < (&other.base, &other.members)
            }
        }
    }
}

/// What a step's candidates are drawn from, the same for every pick.
struct Ground<'a> {
    inference: &'a Inference,
    stacks: &'a Stacks,
    heads: &'a BTreeMap<PrNumber, &'a Sha>,
    /// The members of the batches the plan lands.
    landed: &'a BTreeSet<PrNumber>,
    /// The pull requests the plan ejects.
    ejected: &'a BTreeSet<PrNumber>,
    /// Each evidence component's suspects.
    components: &'a [BTreeSet<PrNumber>],
}

/// The best of `candidates`, by [`Candidate::beats`].
fn best<'a>(candidates: impl IntoIterator<Item = &'a Candidate>) -> Option<&'a Candidate> {
    candidates
        .into_iter()
        .reduce(|a, b| if b.beats(a) { b } else { a })
}

impl Bayes {
    /// The plan under `speculation = "tree"`, once the observation is
    /// ingested and inferred over.
    pub(super) fn tree_plan(
        &mut self,
        observation: &Observation,
        heads: &BTreeMap<PrNumber, &Sha>,
        inference: &Inference,
    ) -> Result<Plan, SchedulerError> {
        let trunk = &observation.trunk;
        let mut plan = Plan::default();

        // Fates, in id order, so every base is reached before what stands
        // on it. `live` is trunk once the landings so far run and the tip
        // of every batch left waiting; a landing removes the base it lands
        // past. A passed batch lands only with no older pending batch on its
        // base: a hedge never pre-empts the batch it hedges.
        let mut live: BTreeSet<&Sha> = BTreeSet::from([trunk]);
        let mut pending_on: BTreeSet<&Sha> = BTreeSet::new();
        let mut after = trunk;
        let mut fates = Vec::with_capacity(observation.batches.len());
        for batch in &observation.batches {
            let base_live = live.contains(&batch.base);
            let fate = match &batch.state {
                BatchState::Testing { .. } if doomed(heads, batch) => Fate::Retire,
                BatchState::Testing {
                    tip,
                    ci: CiStatus::Pending,
                } if base_live => {
                    live.insert(tip);
                    pending_on.insert(&batch.base);
                    Fate::Wait
                }
                BatchState::Testing {
                    tip,
                    ci: CiStatus::Passed,
                } if base_live => {
                    let on_trunk = batch.base == *after;
                    if on_trunk && observation.split_atomic(batch).is_some() {
                        // An `Atomic` pull request above a member arrived
                        // after the batch started: the members go back to
                        // the pool.
                        Fate::Retire
                    } else if on_trunk && !pending_on.contains(&batch.base) {
                        live.remove(after);
                        live.insert(tip);
                        after = tip;
                        plan.land.push(batch.id);
                        Fate::Land
                    } else {
                        live.insert(tip);
                        Fate::Wait
                    }
                }
                BatchState::Conflict { pr, with } => Fate::Conflict(*pr, *with),
                // A batch on a dead base, pending or passed, is cancelled.
                BatchState::Testing { .. }
                | BatchState::Landed { .. }
                | BatchState::LandRefused { .. } => Fate::Retire,
            };
            fates.push(fate);
        }

        let ejected = self.eject(observation, inference, &mut fates, &mut plan);

        // The nodes, from the fates the ejections leave: a waiting batch
        // whose base is no node, because the batch beneath it was just
        // retired, is retired too, and so is one whose chance of landing
        // is 0.
        let mut nodes: BTreeMap<Sha, Node> = BTreeMap::from([(trunk.clone(), Node::root())]);
        for (batch, fate) in observation.batches.iter().zip(fates.iter_mut()) {
            let BatchState::Testing { tip, .. } = &batch.state else {
                continue;
            };
            match fate {
                Fate::Land => {
                    nodes.remove(&batch.base);
                    nodes.insert(tip.clone(), Node::root());
                }
                Fate::Wait => {
                    let Some(node) = nodes.get_mut(&batch.base) else {
                        *fate = Fate::Retire;
                        continue;
                    };
                    let members: Vec<Member> = batch
                        .members
                        .iter()
                        .map(|m| (m.pr, m.head.clone()))
                        .collect();
                    let weight = node.weight();
                    if weight == 0.0 {
                        // An older passed batch on the node lands first
                        // on every path: this one cannot land.
                        *fate = Fate::Retire;
                        continue;
                    }
                    let lands = if is_pending(&batch.state) {
                        self.p_pass(inference, &members, &node.tree)
                    } else {
                        1.0
                    };
                    node.siblings.push(lands);
                    let mut tree = node.tree.clone();
                    tree.extend(members);
                    nodes.insert(
                        tip.clone(),
                        Node {
                            p: weight * lands,
                            tree,
                            siblings: Vec::new(),
                        },
                    );
                }
                Fate::Conflict(..) | Fate::Retire => {}
            }
        }

        let (in_flight, pending) = self.settle(observation, &fates, &mut plan);
        if pending >= self.params.lanes {
            return Ok(plan);
        }
        let mut free = self.params.lanes - pending;
        let mut taken: BTreeSet<PrNumber> = in_flight.union(&ejected).copied().collect();
        let landed: BTreeSet<PrNumber> = landing_members(observation, &fates).collect();
        let stacks = Stacks::new(observation);

        // Each evidence component's suspects: every pull request one of its
        // causes names.
        let components: Vec<BTreeSet<PrNumber>> = inference
            .components()
            .iter()
            .map(|runs| {
                inference
                    .component_causes(runs)
                    .iter()
                    .flat_map(Cause::prs)
                    .collect()
            })
            .collect();

        let ground = Ground {
            inference,
            stacks: &stacks,
            heads,
            landed: &landed,
            ejected: &ejected,
            components: &components,
        };
        while free > 0 {
            let candidates = self.candidates(&ground, &nodes, &taken)?;
            let Some(mut chosen) = best(&candidates) else {
                break;
            };
            if chosen.score <= self.params.speculate_above {
                // Liveness: an idle queue with ready work runs the best
                // candidate on trunk even when none is worth its run. With
                // a batch in flight, waiting for its verdict is the choice.
                let idle = pending == 0 && plan.start.is_empty();
                match best(candidates.iter().filter(|c| c.base == *after)) {
                    Some(candidate) if idle => chosen = candidate,
                    _ => break,
                }
            }
            let node = nodes
                .get_mut(&chosen.base)
                .expect("a candidate is built on a node");
            node.siblings
                .push(self.p_pass(inference, &chosen.members, &node.tree));
            taken.extend(chosen.members.iter().map(|m| m.0));
            let prs = stacks.ordered(chosen.members.iter().map(|m| m.0));
            plan.start
                .push(BatchStart::new(self.ids.mint(), chosen.base.clone(), prs));
            free -= 1;
            if chosen.score <= self.params.speculate_above {
                break;
            }
        }
        Ok(plan)
    }

    /// Every candidate on every node a batch started now could land from,
    /// over the pull requests not `taken`: the extension grown from every
    /// unit, each suspect batch, and each component's extension grown from
    /// the units holding its suspects, each scored with the node's tree as
    /// `below` at the node's weight.
    fn candidates(
        &self,
        ground: &Ground,
        nodes: &BTreeMap<Sha, Node>,
        taken: &BTreeSet<PrNumber>,
    ) -> Result<Vec<Candidate>, SchedulerError> {
        let Ground {
            inference,
            stacks,
            heads,
            landed,
            ejected,
            components,
        } = *ground;
        let pool = self.pool(stacks, heads, taken);
        if pool.is_empty() {
            return Ok(Vec::new());
        }
        let units = self.units(inference, stacks, heads, taken);
        let mut out: Vec<Candidate> = Vec::new();
        for (base, node) in nodes {
            let weight = node.weight();
            if weight == 0.0 {
                continue;
            }
            let beneath: BTreeSet<PrNumber> = node
                .tree
                .iter()
                .map(|m| m.0)
                .chain(landed.iter().copied())
                .collect();
            let footing = stacks.on(&beneath);
            let mut grown = |units: &[Vec<Member>]| -> Result<(), SchedulerError> {
                if let Some((members, score)) = self
                    .grow(inference, &footing, units, &node.tree, weight, ejected)
                    .map_err(SchedulerError::new)?
                {
                    out.push(Candidate {
                        base: base.clone(),
                        members,
                        score,
                    });
                }
                Ok(())
            };
            grown(&units)?;
            for suspects in components {
                let theirs: Vec<Vec<Member>> = units
                    .iter()
                    .filter(|unit| unit.iter().any(|m| suspects.contains(&m.0)))
                    .cloned()
                    .collect();
                if !theirs.is_empty() {
                    grown(&theirs)?;
                }
            }
            // TODO(tree-starves-resolution): a suspect batch's score rarely beats a
            // fresh extension's, so suspects seldom start while fresh work is ready.
            for members in self.suspects(inference, stacks, &pool, &beneath) {
                let score = self
                    .score(inference, &members, &node.tree, weight, ejected)
                    .map_err(SchedulerError::new)?;
                out.push(Candidate {
                    base: base.clone(),
                    members,
                    score,
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{informative, members, params};
    use super::*;
    use crate::test_support::*;
    use crate::{BayesConfig, Charge, Speculation};
    use dorfq_core::{BatchId, Hold, Scheduler};

    /// A tree scheduler at [`params`] with `edit` applied.
    fn tree(edit: impl FnOnce(&mut BayesConfig)) -> Bayes {
        let mut params = BayesConfig {
            speculation: Speculation::Tree,
            ..params()
        };
        edit(&mut params);
        Bayes::new(params)
    }

    fn ids(ns: &[u64]) -> Vec<BatchId> {
        ns.iter().copied().map(BatchId).collect()
    }

    fn started(plan: &Plan) -> BTreeSet<PrNumber> {
        plan.start.iter().flat_map(|s| s.prs.clone()).collect()
    }

    #[test]
    fn a_pending_batch_gets_an_extension_on_its_tip_then_a_hedge_beside_it() {
        let prs: Vec<u64> = (1..=12).collect();
        let mut bayes = tree(|p| {
            p.lanes = 1;
            p.max_batch = 4;
            p.charge = Charge::Full;
        });
        let plan = step(&mut bayes, &observation(ready_all(&prs), vec![]));
        assert_eq!(plan.start, vec![start(1, trunk(), &[1, 2, 3, 4])]);

        bayes.params.lanes = 3;
        let root = testing(1, trunk(), &[1, 2, 3, 4], CiStatus::Pending);
        let plan = step(&mut bayes, &observation(ready_all(&prs), vec![root]));
        assert_eq!(
            plan.start,
            vec![
                start(2, tip(1), &[5, 6, 7, 8]),
                start(3, trunk(), &[9, 10, 11, 12])
            ]
        );

        // The hedge lands only if the root fails: `(1 − p_pass(B1))·V − c`.
        let inference = Inference::of(&bayes.model).unwrap();
        let none = BTreeSet::new();
        let (root, extension, hedge) = (
            members(&[1, 2, 3, 4]),
            members(&[5, 6, 7, 8]),
            members(&[9, 10, 11, 12]),
        );
        let p_root = bayes.p_pass(&inference, &root, &[]);
        let p = bayes.p_pass(&inference, &hedge, &[]);
        let r = bayes.resolution(&inference, &hedge, &[], &none).unwrap();
        let expected = (1.0 - p_root) * (p * 4.0 + (1.0 - p) * r) - 0.5;
        let hedged = bayes
            .score(&inference, &hedge, &[], 1.0 - p_root, &none)
            .unwrap();
        assert!((hedged - expected).abs() < 1e-12, "{hedged} {expected}");
        assert!(hedged > 0.0, "{hedged}");
        let extended = bayes
            .score(&inference, &extension, &root, p_root, &none)
            .unwrap();
        assert!(extended > hedged, "{extended} {hedged}");
    }

    #[test]
    fn a_passed_hedge_waits_for_its_older_sibling_and_follows_its_verdict() {
        let prs = ready_all(&[1, 2]);
        let hedge = || testing(2, trunk(), &[2], CiStatus::Passed);
        let primary = |ci| testing(1, trunk(), &[1], ci);

        let plan = step(
            &mut tree(|_| {}),
            &observation(prs.clone(), vec![primary(CiStatus::Pending), hedge()]),
        );
        assert!(plan.land.is_empty() && plan.retire.is_empty(), "{plan:?}");

        let plan = step(
            &mut tree(|_| {}),
            &observation(prs.clone(), vec![primary(ci_failed()), hedge()]),
        );
        assert_eq!(plan.land, ids(&[2]));
        assert_eq!(plan.retire, ids(&[1]));

        let plan = step(
            &mut tree(|_| {}),
            &observation(prs, vec![primary(CiStatus::Passed), hedge()]),
        );
        assert_eq!(plan.land, ids(&[1]));
        assert_eq!(plan.retire, ids(&[2]));
    }

    #[test]
    fn nothing_starts_beside_a_passed_batch_but_its_tip_is_built_on() {
        let mut bayes = tree(|p| {
            p.lanes = 4;
            p.max_batch = 1;
        });
        let batches = vec![
            testing(1, trunk(), &[1], CiStatus::Pending),
            testing(2, tip(1), &[2], CiStatus::Passed),
        ];
        let plan = step(
            &mut bayes,
            &observation(ready_all(&[1, 2, 3, 4, 5]), batches),
        );
        assert!(plan.land.is_empty() && plan.retire.is_empty(), "{plan:?}");
        assert_eq!(plan.start[0], start(3, tip(2), &[3]));
        assert!(plan.start.iter().all(|s| s.base != tip(1)), "{plan:?}");
    }

    #[test]
    fn a_waiting_batch_with_no_chance_of_landing_is_retired() {
        let mut bayes = tree(|p| {
            p.lanes = 4;
            p.max_batch = 1;
        });
        // Batch 3 stands beside an older passed batch: it is cancelled, and
        // so is batch 4 on its tip, freeing both lanes.
        let batches = vec![
            testing(1, trunk(), &[1], CiStatus::Pending),
            testing(2, trunk(), &[2], CiStatus::Passed),
            testing(3, trunk(), &[3], CiStatus::Pending),
            testing(4, tip(3), &[4], CiStatus::Pending),
        ];
        let plan = step(
            &mut bayes,
            &observation(ready_all(&[1, 2, 3, 4, 5]), batches),
        );
        assert!(plan.land.is_empty(), "{plan:?}");
        assert_eq!(plan.retire, ids(&[3, 4]));
        assert!(!plan.start.is_empty(), "{plan:?}");
        assert!(
            plan.start
                .iter()
                .all(|s| s.base == tip(1) || s.base == tip(2)),
            "{plan:?}"
        );
        assert!(started(&plan).is_subset(&BTreeSet::from([3, 4, 5].map(PrNumber))));

        // A passed newer sibling is retired the same way.
        let batches = vec![
            testing(1, trunk(), &[1], CiStatus::Pending),
            testing(2, trunk(), &[2], CiStatus::Passed),
            testing(3, trunk(), &[3], CiStatus::Passed),
        ];
        let plan = step(
            &mut tree(|p| p.max_batch = 1),
            &observation(ready_all(&[1, 2, 3, 4, 5]), batches),
        );
        assert!(plan.land.is_empty(), "{plan:?}");
        assert_eq!(plan.retire, ids(&[3]));
    }

    #[test]
    fn batches_on_a_dead_node_are_cancelled() {
        // A failed root takes its extensions with it.
        let batches = vec![
            testing(1, trunk(), &[1], ci_failed()),
            testing(2, tip(1), &[2], CiStatus::Pending),
            testing(3, tip(2), &[3], CiStatus::Pending),
        ];
        let plan = step(
            &mut tree(|_| {}),
            &observation(ready_all(&[1, 2, 3]), batches),
        );
        assert_eq!(plan.retire, ids(&[1, 2, 3]));
        assert!(plan.start.iter().all(|s| s.base == trunk()), "{plan:?}");

        // A landing takes the hedge on the trunk it lands past.
        let batches = vec![
            testing(1, trunk(), &[1], CiStatus::Passed),
            testing(2, trunk(), &[2], CiStatus::Pending),
        ];
        let plan = step(&mut tree(|_| {}), &observation(ready_all(&[1, 2]), batches));
        assert_eq!(plan.land, ids(&[1]));
        assert_eq!(plan.retire, ids(&[2]));
        assert_eq!(plan.start, vec![start(3, tip(1), &[2])]);

        // Landing two deep, nothing starts on the intermediate tip.
        let batches = vec![
            testing(1, trunk(), &[1], CiStatus::Passed),
            testing(2, tip(1), &[2], CiStatus::Passed),
        ];
        let plan = step(
            &mut tree(|p| p.max_batch = 1),
            &observation(ready_all(&[1, 2, 3, 4]), batches),
        );
        assert_eq!(plan.land, ids(&[1, 2]));
        assert!(!plan.start.is_empty());
        assert!(plan.start.iter().all(|s| s.base == tip(2)), "{plan:?}");
    }

    #[test]
    fn nothing_worth_its_run_starts_only_on_an_idle_queue() {
        let lazy = || tree(|p| p.speculate_above = 1e9);
        let plan = step(&mut lazy(), &observation(ready_all(&[1, 2]), vec![]));
        assert_eq!(plan.start, vec![start(1, trunk(), &[1, 2])]);

        // With a batch in flight the queue waits for its verdict, where the
        // chain would run a candidate on trunk.
        let root = testing(1, trunk(), &[1], CiStatus::Pending);
        let plan = step(&mut lazy(), &observation(ready_all(&[1, 2]), vec![root]));
        assert_eq!(plan, Plan::default());
    }

    #[test]
    fn after_a_batch_of_four_fails_the_next_start_bisects_it() {
        let mut bayes = tree(|p| p.lanes = 1);
        let failed = testing(1, trunk(), &[1, 2, 3, 4], ci_failed());
        let plan = step(
            &mut bayes,
            &observation(ready_all(&[1, 2, 3, 4]), vec![failed]),
        );
        assert_eq!(plan.retire, ids(&[1]));
        assert_eq!(plan.start.len(), 1, "{plan:?}");
        let members = started(&plan);
        assert!((2..=3).contains(&members.len()), "{plan:?}");
        assert!(members.iter().all(|pr| (1..=4).contains(&pr.0)), "{plan:?}");
    }

    #[test]
    fn a_suspect_batch_runs_on_a_tip_with_the_tips_tree_beneath() {
        // {1, 2} failed; 3 pends alone on trunk, and one lane is free.
        let mut bayes = tree(|_| {});
        let failed = testing(1, trunk(), &[1, 2], ci_failed());
        step(
            &mut bayes,
            &observation(ready_all(&[1, 2, 3]), vec![failed]),
        );
        let root = testing(2, trunk(), &[3], CiStatus::Pending);
        let plan = step(&mut bayes, &observation(ready_all(&[1, 2, 3]), vec![root]));
        // The suspects' batch goes on the root's tip, which it lands from
        // far more often than from trunk beside the root.
        assert_eq!(plan.start.len(), 1, "{plan:?}");
        assert_eq!(plan.start[0].base, tip(2));
        assert!(
            started(&plan).is_subset(&BTreeSet::from([PrNumber(1), PrNumber(2)])),
            "{plan:?}"
        );

        // A stacked suspect's batch on a node holding its base leaves the
        // base out.
        let prs = vec![ready(1), stacked(2, 1), ready(3)];
        let mut bayes = tree(|_| {});
        let failed = testing(1, trunk(), &[1, 2, 3], ci_failed());
        step(&mut bayes, &observation(prs.clone(), vec![failed]));
        let inference = Inference::of(&bayes.model).unwrap();
        let stacks = Stacks::new(&observation(prs, vec![]));
        // 1's closure cannot run here, so it is passed over and 2 and 3 are
        // the two most probable `Bad` suspects whose closures can.
        let sets = bayes.suspects(
            &inference,
            &stacks,
            &members(&[2, 3]),
            &BTreeSet::from([PrNumber(1)]),
        );
        assert_eq!(sets, vec![members(&[2]), members(&[3]), members(&[2, 3])]);
        informative(&bayes.model, &inference, &sets);
    }

    #[test]
    fn a_child_extends_the_node_holding_its_base_and_an_atomic_unit_runs_alone() {
        let prs = vec![ready(1), stacked(2, 1)];
        let pending = testing(1, trunk(), &[1], CiStatus::Pending);
        let plan = step(&mut tree(|_| {}), &observation(prs, vec![pending]));
        assert_eq!(plan.start, vec![start(2, tip(1), &[2])]);

        let prs = vec![ready(1), stacked(2, 1), atomic_stacked(3, 2)];
        let plan = step(&mut tree(|p| p.max_batch = 2), &observation(prs, vec![]));
        assert_eq!(plan.start, vec![start(1, trunk(), &[1, 2, 3])]);
    }

    /// A scheduler of `speculation` at beliefs one landing above the floor,
    /// whose evidence is two
    /// chains of `n` failed pairs, over 1..=n+1 and 101..=n+101, each held
    /// for review, with `n + 1` and 101 ready: each chain resolves alone,
    /// and a failure of {n + 1, 101} would join them into one that does not.
    fn two_clusters(speculation: Speculation, n: u64) -> (Bayes, Observation) {
        let mut bayes = Bayes::new(BayesConfig {
            speculation,
            ..params()
        });
        let mut prs: Vec<_> = (1..=n)
            .chain(102..=n + 101)
            .map(|k| stale(held(k, Hold::Review), 1))
            .collect();
        prs.extend([stale(ready(n + 1), 1), stale(ready(101), 1)]);
        let input = observation(prs, vec![]);
        bayes.model.sync(&input.prs, &BTreeSet::new(), 0);
        bayes.age(&input);
        for r in 1..=n {
            bayes.model.failed(BatchId(r), &members(&[r, r + 1]), 0);
            bayes
                .model
                .failed(BatchId(100 + r), &members(&[100 + r, 101 + r]), 0);
        }
        (bayes, input)
    }

    #[test]
    fn an_unresolvable_hypothetical_scores_no_resolution_under_the_tree_only() {
        const N: u64 = 5;
        let none = BTreeSet::new();
        let both = members(&[N + 1, 101]);

        let (mut chain, input) = two_clusters(Speculation::Chain, N);
        let inference = Inference::of(&chain.model).unwrap();
        assert!(chain.score(&inference, &both, &[], 1.0, &none).is_err());
        let stacks = Stacks::new(&input);
        let units = vec![members(&[N + 1]), members(&[101])];
        let beneath = BTreeSet::new();
        let footing = stacks.on(&beneath);
        assert!(
            chain
                .grow(&inference, &footing, &units, &[], 1.0, &none)
                .is_err()
        );
        let error = chain.step(&input).unwrap_err();
        assert!(error.to_string().contains("cannot be resolved"), "{error}");

        let (mut tree, input) = two_clusters(Speculation::Tree, N);
        let inference = Inference::of(&tree.model).unwrap();
        let p = tree.p_pass(&inference, &both, &[]);
        let score = tree.score(&inference, &both, &[], 1.0, &none).unwrap();
        assert!((score - (p * 2.0 - 0.5)).abs() < 1e-12, "{score} {p}");
        assert!(
            tree.grow(&inference, &footing, &units, &[], 1.0, &none)
                .unwrap()
                .is_some()
        );
        let plan = tree.step(&input).unwrap();
        assert!(!plan.start.is_empty(), "{plan:?}");
        assert!(plan.start.iter().all(|s| s.base == trunk()), "{plan:?}");
    }

    #[test]
    fn evidence_too_improbable_to_resolve_is_a_scheduler_error_under_the_tree_too() {
        let mut bayes = tree(|_| {});
        let prs = (1..=12).map(|n| stale(ready(n), 1)).collect();
        let input = observation(prs, vec![]);
        bayes.model.sync(&input.prs, &BTreeSet::new(), 0);
        for r in 1..=11 {
            bayes
                .model
                .failed(BatchId(100 + r), &members(&[r, r + 1]), 0);
        }
        let error = bayes.step(&input).unwrap_err();
        assert!(error.to_string().contains("cannot be resolved"), "{error}");
    }
}
