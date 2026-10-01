// Hybrid logical clock for ordering catalog and peer updates across nodes.
//
// Ordering by raw wall-clock time lets a node with a fast clock win every
// conflict, and lets a node with a slow clock lose even its own deletions. A
// hybrid logical clock stamps every local event with `(wall, counter, node)`
// where `wall` never runs behind anything this node has seen. So an update made
// after observing another is always ordered after it, whatever the wall clocks
// say. Concurrent updates are ordered deterministically by `node`.
//
// A remote stamp more than `MAX_DRIFT_MS` ahead of our own wall clock is
// refused instead of adopted; otherwise one bad clock would drag every node's
// clock forward with it.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::types::NodeId;

pub const MAX_DRIFT_MS: u64 = 60 * 60 * 1000;

// Field order is the comparison order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
pub struct Stamp {
    pub wall: u64,
    pub counter: u32,
    pub node: NodeId,
}

impl Stamp {
    // A default stamp means the sender never set one.
    pub fn is_set(&self) -> bool {
        self.wall > 0
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ClockError {
    Unset,
    Ahead { by_ms: u64 },
}

impl std::fmt::Display for ClockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClockError::Unset => f.write_str("missing timestamp"),
            ClockError::Ahead { by_ms } => write!(f, "timestamp is {} ahead of this device's clock", describe_ms(*by_ms)),
        }
    }
}

pub fn describe_ms(ms: u64) -> String {
    match ms {
        0..=119_999 => format!("{}s", ms / 1000),
        120_000..=7_199_999 => format!("{} min", ms / 60_000),
        _ => format!("{:.1} h", ms as f64 / 3_600_000.0),
    }
}

pub fn wall_clock_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

pub struct Clock {
    node: NodeId,
    // (wall, counter) of the latest stamp issued or observed.
    latest: Mutex<(u64, u32)>,
}

impl Clock {
    pub fn new(node: NodeId) -> Self {
        Self { node, latest: Mutex::new((0, 0)) }
    }

    // Stamp for an event happening on this node now.
    pub fn now(&self) -> Stamp {
        self.now_at(wall_clock_ms())
    }

    // Take a stamp from a remote update into account, so everything this node
    // stamps from now on is ordered after it.
    pub fn observe(&self, remote: &Stamp) -> Result<(), ClockError> {
        self.observe_at(remote, wall_clock_ms())
    }

    fn now_at(&self, physical: u64) -> Stamp {
        let mut latest = self.latest.lock().unwrap();
        let (wall, counter) = *latest;
        *latest = if physical > wall {
            (physical, 0)
        } else {
            match counter.checked_add(1) {
                Some(counter) => (wall, counter),
                None => (wall + 1, 0),
            }
        };
        Stamp { wall: latest.0, counter: latest.1, node: self.node.clone() }
    }

    fn observe_at(&self, remote: &Stamp, physical: u64) -> Result<(), ClockError> {
        if !remote.is_set() {
            return Err(ClockError::Unset);
        }
        if remote.wall > physical.saturating_add(MAX_DRIFT_MS) {
            return Err(ClockError::Ahead { by_ms: remote.wall - physical });
        }
        let mut latest = self.latest.lock().unwrap();
        let (wall, counter) = *latest;
        let merged_wall = wall.max(remote.wall).max(physical);
        let merged_counter = match (merged_wall == wall, merged_wall == remote.wall) {
            (true, true) => counter.max(remote.counter).saturating_add(1),
            (true, false) => counter.saturating_add(1),
            (false, true) => remote.counter.saturating_add(1),
            (false, false) => 0,
        };
        *latest = (merged_wall, merged_counter);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: u64 = 1_700_000_000_000;

    fn clock(node: &str) -> Clock {
        Clock::new(node.to_string())
    }

    #[test]
    fn local_stamps_strictly_increase_even_with_a_frozen_or_backwards_clock() {
        let c = clock("a");
        let first = c.now_at(T);
        let same_ms = c.now_at(T);
        let backwards = c.now_at(T - 5_000);
        assert!(first < same_ms && same_ms < backwards);
        assert_eq!(backwards.wall, T);
    }

    #[test]
    fn a_node_with_a_slow_clock_still_orders_after_what_it_observed() {
        let fast = clock("fast");
        let slow = clock("slow");
        let created = fast.now_at(T + 30 * 60_000);
        // The slow node's wall clock is 30 minutes behind, but it has seen the
        // creation, so its deletion must still win.
        slow.observe_at(&created, T).unwrap();
        let deleted = slow.now_at(T);
        assert!(deleted > created);
    }

    #[test]
    fn a_stamp_far_in_the_future_is_refused_and_does_not_move_the_clock() {
        let c = clock("a");
        let bogus = Stamp { wall: T + MAX_DRIFT_MS + 1, counter: 0, node: "evil".into() };
        assert_eq!(c.observe_at(&bogus, T), Err(ClockError::Ahead { by_ms: MAX_DRIFT_MS + 1 }));
        assert_eq!(c.now_at(T).wall, T);
    }

    #[test]
    fn a_stamp_within_the_allowed_drift_is_adopted() {
        let c = clock("a");
        let ahead = Stamp { wall: T + MAX_DRIFT_MS, counter: 3, node: "b".into() };
        c.observe_at(&ahead, T).unwrap();
        let next = c.now_at(T);
        assert!(next > ahead);
    }

    #[test]
    fn unset_stamps_are_refused() {
        assert_eq!(clock("a").observe_at(&Stamp::default(), T), Err(ClockError::Unset));
        assert!(!Stamp::default().is_set());
    }

    #[test]
    fn concurrent_stamps_are_ordered_deterministically_by_node() {
        let a = clock("node_a").now_at(T);
        let b = clock("node_b").now_at(T);
        assert_eq!((a.wall, a.counter), (b.wall, b.counter));
        assert!(a < b);
    }

    #[test]
    fn counter_overflow_carries_into_the_wall_component() {
        let c = clock("a");
        *c.latest.lock().unwrap() = (T, u32::MAX);
        let s = c.now_at(T);
        assert_eq!((s.wall, s.counter), (T + 1, 0));
    }

    #[test]
    fn describe_ms_picks_a_readable_unit() {
        assert_eq!(describe_ms(5_000), "5s");
        assert_eq!(describe_ms(10 * 60_000), "10 min");
        assert_eq!(describe_ms(3 * 3_600_000), "3.0 h");
    }
}
