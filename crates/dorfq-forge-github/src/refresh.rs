//! The refresh schedule: when each entity the forge caches is next read.
//!
//! Every entity — the listing, trunk, each batch, each pull request — has a
//! deadline on the forge's [`Timestamp`] clock; an entity is due once its
//! deadline has passed. A refresh moves the deadline one interval on, and a
//! class of entities refreshed together is spread across the interval that
//! follows, so later refreshes come due one at a time. A hint moves a
//! deadline earlier, never later, and no sooner than [`HINT_FLOOR`] after
//! the entity's last refresh. The queue's own writes make an entity due at
//! once, floor or not. A rate-limit pause holds the next deadline back.
//!
//! Pure: nothing here reads a clock or GitHub.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use dorfq_core::{BatchId, PrNumber, Timestamp};

/// The least time between two refreshes of one entity that a hint can
/// cause, and how soon an entity marked [`Schedule::soon`] is due.
pub const HINT_FLOOR: u64 = 5;

/// How often each class of entity is read unprompted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refresh {
    /// The labelled set and the batch records.
    pub listing: Duration,
    /// Each queued pull request.
    pub pr: Duration,
    /// Each testing batch's checks, and trunk.
    pub batch: Duration,
}

impl Refresh {
    /// Rejects an interval under one second or not a whole number of
    /// seconds. The message begins with the interval's name
    /// (`listing_secs is 0 s`).
    pub fn validate(&self) -> Result<(), String> {
        for (name, interval) in [
            ("listing_secs", self.listing),
            ("pr_secs", self.pr),
            ("batch_secs", self.batch),
        ] {
            if interval.as_secs() < 1 || interval.subsec_nanos() != 0 {
                return Err(format!(
                    "{name} is {interval:?}; it must be a whole number of seconds, at least 1"
                ));
            }
        }
        Ok(())
    }
}

/// Something the forge caches and refreshes as one unit. Ordered as
/// [`Schedule::due`] reports: listing, trunk, batches by id, pull requests
/// by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Entity {
    Listing,
    Trunk,
    Batch(BatchId),
    Pr(PrNumber),
}

impl fmt::Display for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Entity::Listing => f.write_str("the listing"),
            Entity::Trunk => f.write_str("trunk"),
            Entity::Batch(id) => write!(f, "batch {}", id.0),
            Entity::Pr(number) => write!(f, "pull request #{}", number.0),
        }
    }
}

/// When each tracked entity is next due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    /// [`Refresh::listing`], in whole seconds.
    listing: u64,
    /// [`Refresh::pr`], in whole seconds.
    pr: u64,
    /// [`Refresh::batch`], in whole seconds: batches and trunk.
    batch: u64,
    deadlines: BTreeMap<Entity, Timestamp>,
    last_refreshed: BTreeMap<Entity, Timestamp>,
    paused_until: Option<Timestamp>,
}

impl Schedule {
    /// A schedule tracking the listing and trunk, both due at once, and
    /// nothing else, over intervals [`Refresh::validate`] accepts.
    pub fn new(refresh: &Refresh) -> Schedule {
        let at_once = Timestamp(0);
        Schedule {
            listing: refresh.listing.as_secs(),
            pr: refresh.pr.as_secs(),
            batch: refresh.batch.as_secs(),
            deadlines: BTreeMap::from([(Entity::Listing, at_once), (Entity::Trunk, at_once)]),
            last_refreshed: BTreeMap::new(),
            paused_until: None,
        }
    }

    fn interval(&self, entity: Entity) -> u64 {
        match entity {
            Entity::Listing => self.listing,
            Entity::Pr(_) => self.pr,
            Entity::Trunk | Entity::Batch(_) => self.batch,
        }
    }

    /// Whether `entity` is tracked.
    pub fn tracks(&self, entity: Entity) -> bool {
        self.deadlines.contains_key(&entity)
    }

    /// Every tracked entity whose deadline is at or before `now`, in
    /// [`Entity`] order. A pause does not hold an entity back here; it
    /// holds back [`Schedule::next_deadline`].
    pub fn due(&self, now: Timestamp) -> Vec<Entity> {
        self.deadlines
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(entity, _)| *entity)
            .collect()
    }

    /// Records a refresh of `entity` at `now`, the `index`th of `count`
    /// entities of its class refreshed in the same pass: the next deadline
    /// is `now + interval + index·interval/count`, so a class refreshed
    /// together comes due spread over `[interval, 2·interval)`. Tracks
    /// `entity` if it was not tracked.
    ///
    /// # Panics
    ///
    /// When `index` is not below `count`.
    pub fn refreshed(&mut self, entity: Entity, now: Timestamp, count: usize, index: usize) {
        assert!(
            index < count,
            "refresh index {index} is not below the class count {count}"
        );
        let interval = self.interval(entity);
        // Neither factor is anywhere near overflowing: an interval is
        // configured seconds, `index` a count of cached entities.
        let spread = interval * index as u64 / count as u64;
        self.deadlines
            .insert(entity, Timestamp(now.0 + interval + spread));
        self.last_refreshed.insert(entity, now);
    }

    /// Records a refresh of `entity` at `now` whose result is due to settle
    /// shortly: it is due again [`HINT_FLOOR`] seconds later. Tracks
    /// `entity` if it was not tracked.
    pub fn soon(&mut self, entity: Entity, now: Timestamp) {
        self.deadlines.insert(entity, Timestamp(now.0 + HINT_FLOOR));
        self.last_refreshed.insert(entity, now);
    }

    /// Makes `entity` due at `now`, whenever it was last refreshed; tracks
    /// it if it was not tracked. Returns the deadline set, `now`.
    pub fn due_now(&mut self, entity: Entity, now: Timestamp) -> Timestamp {
        self.deadlines.insert(entity, now);
        now
    }

    /// Moves `entity`'s deadline to `now`, or to [`HINT_FLOOR`] seconds
    /// after its last refresh if that is later, unless its deadline is
    /// already earlier. Returns the deadline `entity` holds after the call;
    /// `None` for an entity not tracked, which stays untracked.
    pub fn hint(&mut self, entity: Entity, now: Timestamp) -> Option<Timestamp> {
        let floor = self
            .last_refreshed
            .get(&entity)
            .map_or(now, |last| now.max(Timestamp(last.0 + HINT_FLOOR)));
        let deadline = self.deadlines.get_mut(&entity)?;
        *deadline = (*deadline).min(floor);
        Some(*deadline)
    }

    /// Stops tracking `entity`: it left the cache.
    pub fn forget(&mut self, entity: Entity) {
        self.deadlines.remove(&entity);
        self.last_refreshed.remove(&entity);
    }

    /// Holds [`Schedule::next_deadline`] back until `until`; a later pause
    /// already in force stands.
    pub fn pause(&mut self, until: Timestamp) {
        self.paused_until = Some(self.paused_until.map_or(until, |held| held.max(until)));
    }

    /// The end of the rate-limit pause in force, if one was ever set.
    pub fn paused_until(&self) -> Option<Timestamp> {
        self.paused_until
    }

    /// The earliest deadline, or the pause's end if that is later; `None`
    /// when nothing is tracked.
    pub fn next_deadline(&self) -> Option<Timestamp> {
        let earliest = self.deadlines.values().min().copied()?;
        Some(
            self.paused_until
                .map_or(earliest, |until| earliest.max(until)),
        )
    }
}

#[cfg(test)]
mod tests;
