//! The stack view of an observation: which pull requests may join a batch,
//! in what order, and which of them must land together.

use std::collections::{BTreeMap, BTreeSet};

use dorfq_core::{Observation, PrBase, PrNumber, StackPolicy};

use crate::{ancestors, bases, batchable};

/// The batchable pull requests of one observation, grouped into landing
/// units.
///
/// A **unit** is the smallest set of batchable pull requests that can land
/// together without splitting an `Atomic` stack: an `Atomic` pull request
/// and every ancestor of it are one unit, two `Atomic` pull requests over a
/// shared ancestor are one unit with it, and every other batchable pull
/// request is a unit by itself. A unit of more than one member is closed
/// under `base`, so the only dependency between units is a single-member
/// unit whose base lies in another; and units ordered by their first member
/// put every such base's unit first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stacks {
    /// The batchable pull requests, bases before children, ties by number.
    order: Vec<PrNumber>,
    /// Each batchable pull request's index into `order`.
    position: BTreeMap<PrNumber, usize>,
    /// Every enqueued pull request's base.
    base: BTreeMap<PrNumber, PrBase>,
    /// The units, each in batchable order, ordered by first member.
    units: Vec<Vec<PrNumber>>,
    /// Each batchable pull request's index into `units`.
    unit_of: BTreeMap<PrNumber, usize>,
}

impl Stacks {
    /// The stack view of `observation`: `batchable(observation)`, split
    /// into units.
    pub(crate) fn new(observation: &Observation) -> Stacks {
        let order = batchable(observation);
        let base = bases(&observation.prs);
        let position: BTreeMap<PrNumber, usize> =
            order.iter().enumerate().map(|(i, &n)| (n, i)).collect();

        // Union every `Atomic` pull request with its ancestors. `batchable`
        // keeps an `Atomic` pull request only with all its ancestors, so
        // every ancestor walked here has a position.
        let mut root: Vec<usize> = (0..order.len()).collect();
        fn find(root: &mut [usize], mut i: usize) -> usize {
            while root[i] != i {
                root[i] = root[root[i]];
                i = root[i];
            }
            i
        }
        for p in &observation.prs {
            let Some(&tip) = position.get(&p.number) else {
                continue;
            };
            if p.stack != StackPolicy::Atomic {
                continue;
            }
            for n in ancestors(&base, p.base) {
                let (a, b) = (find(&mut root, tip), find(&mut root, position[&n]));
                // The earlier position is the root, so a unit's root is its
                // first member.
                root[a.max(b)] = a.min(b);
            }
        }

        let mut units: Vec<Vec<PrNumber>> = Vec::new();
        let mut unit_at_root: BTreeMap<usize, usize> = BTreeMap::new();
        let mut unit_of = BTreeMap::new();
        for (i, &pr) in order.iter().enumerate() {
            let r = find(&mut root, i);
            let unit = *unit_at_root.entry(r).or_insert_with(|| {
                units.push(Vec::new());
                units.len() - 1
            });
            units[unit].push(pr);
            unit_of.insert(pr, unit);
        }
        Stacks {
            order,
            position,
            base,
            units,
            unit_of,
        }
    }

    /// Whether `pr` is batchable.
    pub(crate) fn contains(&self, pr: PrNumber) -> bool {
        self.position.contains_key(&pr)
    }

    /// The batchable pull requests, bases before children, ties by number.
    pub(crate) fn order(&self) -> &[PrNumber] {
        &self.order
    }

    /// `members` in batchable order.
    ///
    /// # Panics
    ///
    /// When a member is not batchable: the caller chose it from something
    /// other than this view.
    pub(crate) fn ordered(&self, members: impl IntoIterator<Item = PrNumber>) -> Vec<PrNumber> {
        let mut out: Vec<PrNumber> = members.into_iter().collect();
        out.sort_by_key(|pr| {
            *self
                .position
                .get(pr)
                .unwrap_or_else(|| panic!("{pr:?} is not batchable"))
        });
        out
    }

    /// `pr` and every enqueued ancestor of it, bases first; `None` when `pr`
    /// or one of its ancestors is not batchable.
    pub(crate) fn closure(&self, pr: PrNumber) -> Option<Vec<PrNumber>> {
        if !self.contains(pr) {
            return None;
        }
        let mut out: Vec<PrNumber> = std::iter::once(pr)
            .chain(ancestors(&self.base, self.base[&pr]))
            .collect();
        if !out.iter().all(|&n| self.contains(n)) {
            return None;
        }
        out.reverse();
        Some(out)
    }

    /// The landing units, in order of each unit's first member.
    pub(crate) fn units(&self) -> &[Vec<PrNumber>] {
        &self.units
    }

    /// The unit `pr` belongs to, `None` when it is not batchable.
    pub(crate) fn unit(&self, pr: PrNumber) -> Option<&[PrNumber]> {
        self.unit_of.get(&pr).map(|&u| self.units[u].as_slice())
    }

    /// The view from a node whose tree holds `beneath` beyond the
    /// observation's trunk.
    pub(crate) fn on<'a>(&'a self, beneath: &'a BTreeSet<PrNumber>) -> Footing<'a> {
        Footing {
            stacks: self,
            beneath,
        }
    }

    /// `members` grouped by unit, each group in `members` order, groups in
    /// order of their first member's appearance in `members`; a member that
    /// is not batchable is a group by itself.
    pub(crate) fn partition(&self, members: &[PrNumber]) -> Vec<Vec<PrNumber>> {
        let mut groups: Vec<Vec<PrNumber>> = Vec::new();
        let mut group_of_unit: BTreeMap<usize, usize> = BTreeMap::new();
        for &pr in members {
            match self.unit_of.get(&pr) {
                Some(&unit) => match group_of_unit.get(&unit) {
                    Some(&g) => groups[g].push(pr),
                    None => {
                        group_of_unit.insert(unit, groups.len());
                        groups.push(vec![pr]);
                    }
                },
                None => groups.push(vec![pr]),
            }
        }
        groups
    }
}

/// The stack view from one node: what a batch built on it may stand on.
pub(crate) struct Footing<'a> {
    stacks: &'a Stacks,
    /// The members of the node's tree beyond the observation's trunk.
    beneath: &'a BTreeSet<PrNumber>,
}

impl Footing<'_> {
    /// `chosen` with `seq` added, when each member of `seq` in turn has its
    /// base on trunk, in `chosen`, earlier in `seq` or beneath the node;
    /// `None` otherwise, and for a pull request that is not enqueued.
    pub(crate) fn extend(
        &self,
        chosen: &BTreeSet<PrNumber>,
        seq: impl IntoIterator<Item = PrNumber>,
    ) -> Option<BTreeSet<PrNumber>> {
        let mut with = chosen.clone();
        for pr in seq {
            let based = match self.stacks.base.get(&pr)? {
                PrBase::Trunk => true,
                PrBase::Pr(b) => with.contains(b) || self.beneath.contains(b),
            };
            if !based {
                return None;
            }
            with.insert(pr);
        }
        Some(with)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use dorfq_core::{Hold, PrSnapshot};

    fn stacks(prs: Vec<PrSnapshot>) -> Stacks {
        Stacks::new(&observation(prs, vec![]))
    }

    fn units(s: &Stacks) -> Vec<Vec<u64>> {
        s.units()
            .iter()
            .map(|u| u.iter().map(|n| n.0).collect())
            .collect()
    }

    fn set(ns: &[u64]) -> BTreeSet<PrNumber> {
        numbers(ns).into_iter().collect()
    }

    /// Every test shape below, for the properties that hold of all of them.
    fn shapes() -> Vec<Vec<PrSnapshot>> {
        vec![
            vec![ready(1), stacked(2, 1), stacked(3, 2)],
            vec![ready(1), stacked(2, 1), atomic_stacked(3, 2), ready(4)],
            vec![ready(1), atomic_stacked(2, 1), atomic_stacked(3, 1)],
            vec![ready(1), atomic_stacked(2, 1), stacked(3, 1), stacked(4, 3)],
            vec![
                ready(1),
                stacked(2, 1),
                atomic_stacked(3, 2),
                stacked(4, 2),
                atomic_stacked(5, 4),
                ready(6),
                stacked(7, 6),
            ],
        ]
    }

    #[test]
    fn a_splittable_chain_is_one_unit_per_pull_request() {
        let s = stacks(vec![ready(1), stacked(2, 1), stacked(3, 2)]);
        assert_eq!(units(&s), [vec![1], vec![2], vec![3]]);
    }

    #[test]
    fn an_atomic_tip_and_its_ancestors_are_one_unit() {
        let s = stacks(vec![
            ready(1),
            stacked(2, 1),
            atomic_stacked(3, 2),
            ready(4),
        ]);
        assert_eq!(units(&s), [vec![1, 2, 3], vec![4]]);
        assert_eq!(s.unit(PrNumber(2)), Some(numbers(&[1, 2, 3]).as_slice()));
    }

    #[test]
    fn two_atomic_tips_over_a_shared_ancestor_are_one_unit() {
        let s = stacks(vec![ready(1), atomic_stacked(2, 1), atomic_stacked(3, 1)]);
        assert_eq!(units(&s), [vec![1, 2, 3]]);
    }

    #[test]
    fn a_splittable_child_beside_an_atomic_unit_is_its_own_unit_after_it() {
        let s = stacks(vec![
            ready(1),
            atomic_stacked(2, 1),
            stacked(3, 1),
            stacked(4, 3),
        ]);
        assert_eq!(units(&s), [vec![1, 2], vec![3], vec![4]]);
    }

    #[test]
    fn atomic_units_chained_through_a_shared_ancestor_merge() {
        // 3 pulls in {1, 2}; 5 pulls in {1, 2, 4}: one unit.
        let s = stacks(shapes().remove(4));
        assert_eq!(units(&s), [vec![1, 2, 3, 4, 5], vec![6], vec![7]]);
    }

    #[test]
    fn pull_requests_that_are_not_batchable_have_no_unit() {
        let mut tip = atomic(held(3, Hold::Review));
        tip.base = PrBase::Pr(PrNumber(2));
        let s = stacks(vec![ready(1), stacked(2, 1), tip, ready(4)]);
        assert_eq!(units(&s), [vec![4]]);
        assert!(!s.contains(PrNumber(1)));
        assert!(s.unit(PrNumber(3)).is_none());
        assert!(s.contains(PrNumber(4)));
    }

    #[test]
    fn extend_accepts_bases_on_trunk_chosen_earlier_or_beneath() {
        let s = stacks(vec![ready(1), stacked(2, 1), stacked(3, 2)]);
        let none = BTreeSet::new();
        let from = |beneath: &BTreeSet<PrNumber>, chosen: &[u64], seq: &[u64]| {
            s.on(beneath).extend(&set(chosen), numbers(seq))
        };
        assert_eq!(from(&none, &[], &[1]), Some(set(&[1])));
        assert_eq!(from(&none, &[], &[2]), None);
        assert_eq!(from(&none, &[1], &[2]), Some(set(&[1, 2])));
        assert_eq!(from(&set(&[1]), &[], &[2]), Some(set(&[2])));
        assert_eq!(from(&none, &[], &[1, 2, 3]), Some(set(&[1, 2, 3])));
        assert_eq!(from(&none, &[], &[2, 1]), None);
        assert_eq!(from(&none, &[1], &[3]), None);
        assert_eq!(from(&none, &[], &[9]), None);
    }

    #[test]
    fn partition_keeps_units_together_in_order_of_appearance() {
        let s = stacks(vec![
            ready(1),
            atomic_stacked(2, 1),
            stacked(3, 1),
            ready(4),
            held(5, Hold::Review),
        ]);
        let groups = s.partition(&numbers(&[1, 3, 2, 5, 4]));
        assert_eq!(
            groups,
            [
                numbers(&[1, 2]),
                numbers(&[3]),
                numbers(&[5]),
                numbers(&[4])
            ]
        );
    }

    #[test]
    fn order_puts_bases_first_and_ordered_follows_it() {
        let s = stacks(vec![stacked(2, 3), ready(3), ready(1), stacked(4, 2)]);
        assert_eq!(s.order(), numbers(&[1, 3, 2, 4]).as_slice());
        assert_eq!(s.ordered(numbers(&[4, 1, 2])), numbers(&[1, 2, 4]));
    }

    #[test]
    #[should_panic(expected = "is not batchable")]
    fn ordered_refuses_a_member_that_is_not_batchable() {
        stacks(vec![ready(1)]).ordered(numbers(&[1, 2]));
    }

    #[test]
    fn closure_is_the_pull_request_and_its_ancestors_bases_first() {
        let s = stacks(shapes().remove(4));
        assert_eq!(s.closure(PrNumber(1)), Some(numbers(&[1])));
        assert_eq!(s.closure(PrNumber(5)), Some(numbers(&[1, 2, 4, 5])));
        assert_eq!(s.closure(PrNumber(7)), Some(numbers(&[6, 7])));
        assert_eq!(s.closure(PrNumber(9)), None);

        // An ancestor held back under an un-`Ready` `Atomic` tip, and a
        // pull request whose base is not `Ready`.
        let mut tip = atomic(held(3, Hold::Review));
        tip.base = PrBase::Pr(PrNumber(2));
        let s = stacks(vec![
            ready(1),
            stacked(2, 1),
            tip,
            held(4, Hold::Review),
            stacked(5, 4),
        ]);
        assert_eq!(s.closure(PrNumber(2)), None);
        assert_eq!(s.closure(PrNumber(5)), None);
    }

    #[test]
    fn every_unit_of_more_than_one_member_is_base_closed_and_units_are_topological() {
        for prs in shapes() {
            let s = stacks(prs.clone());
            let base: BTreeMap<PrNumber, PrBase> = prs.iter().map(|p| (p.number, p.base)).collect();
            let mut earlier: BTreeSet<PrNumber> = BTreeSet::new();
            for unit in s.units() {
                let members: BTreeSet<PrNumber> = unit.iter().copied().collect();
                for pr in unit {
                    if let PrBase::Pr(b) = base[pr] {
                        if unit.len() > 1 {
                            assert!(members.contains(&b), "{unit:?} lacks {b:?}");
                        }
                        assert!(
                            members.contains(&b) || earlier.contains(&b),
                            "{pr:?}'s base {b:?} comes after it"
                        );
                    }
                }
                earlier.extend(unit.iter().copied());
            }
        }
    }
}
