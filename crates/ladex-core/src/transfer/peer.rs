//! What the scheduler knows about one node it fetches a file from: how fast it is and how far to trust it.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use tokio::task::AbortHandle;

// Throughput is measured over windows this long, so a burst of chunks does not read as a fast link.
const SAMPLE_WINDOW: Duration = Duration::from_millis(200);
// How much a new measurement moves the speed estimate.
const SPEED_WEIGHT: f64 = 0.3;
const BAN_AFTER_BAD_CHUNKS: u32 = 2;
const BAN_AFTER_FAILURES: u32 = 3;

/// Why a request's chunks came back unanswered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The node said it does not hold them.
    Missing,
    /// The stream ended, failed or stalled before they arrived.
    Failed,
    /// The node had no upload slot to spare; not its fault.
    Busy,
}

pub struct Peer {
    pub in_flight: HashSet<u32>,
    pub speed_bps: f64,
    // Requests for a single chunk, which can be called off when another node delivers it first.
    pub cancellable: HashMap<u32, AbortHandle>,
    window_start: Instant,
    window_bytes: usize,
    bad_chunks: u32,
    failures: u32,
    banned: bool,
}

impl Peer {
    pub fn new(now: Instant) -> Self {
        Self {
            in_flight: HashSet::new(),
            speed_bps: 0.0,
            cancellable: HashMap::new(),
            window_start: now,
            window_bytes: 0,
            bad_chunks: 0,
            failures: 0,
            banned: false,
        }
    }

    pub fn is_banned(&self) -> bool {
        self.banned
    }

    pub fn ban(&mut self) {
        self.banned = true;
    }

    /// Call before adding to `in_flight`, so idle time is not counted as slowness.
    pub fn request_started(&mut self, now: Instant) {
        if self.in_flight.is_empty() {
            self.window_start = now;
            self.window_bytes = 0;
        }
    }

    /// A chunk that passed verification arrived.
    pub fn delivered(&mut self, now: Instant, bytes: usize) {
        self.failures = 0;
        self.window_bytes += bytes;
        let elapsed = now.saturating_duration_since(self.window_start);
        if elapsed < SAMPLE_WINDOW {
            return;
        }
        let sample = self.window_bytes as f64 / elapsed.as_secs_f64();
        self.speed_bps = if self.speed_bps == 0.0 { sample } else { (1.0 - SPEED_WEIGHT) * self.speed_bps + SPEED_WEIGHT * sample };
        self.window_start = now;
        self.window_bytes = 0;
    }

    /// A chunk failed verification; the node is dropped for this file after repeated offences.
    pub fn sent_bad_chunk(&mut self) {
        self.bad_chunks += 1;
        if self.bad_chunks >= BAN_AFTER_BAD_CHUNKS {
            self.banned = true;
        }
    }

    /// A request ended without all its chunks; slows the node and drops it for this file after repeated failures.
    pub fn released(&mut self, fault: Fault) {
        if fault == Fault::Busy {
            return;
        }
        self.speed_bps *= 0.5;
        self.failures += 1;
        if self.failures >= BAN_AFTER_FAILURES {
            self.banned = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: usize = 1_000_000;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    #[test]
    fn speed_is_the_bytes_delivered_over_the_time_they_took() {
        let start = Instant::now();
        let mut peer = Peer::new(start);

        peer.delivered(at(start, 500), MB);

        assert_eq!(peer.speed_bps, 2.0 * MB as f64);
    }

    #[test]
    fn chunks_arriving_in_a_burst_do_not_inflate_the_speed() {
        let start = Instant::now();
        let mut peer = Peer::new(start);

        peer.delivered(at(start, 1), MB);
        peer.delivered(at(start, 2), MB);
        peer.delivered(at(start, 1000), MB);

        assert_eq!(peer.speed_bps, 3.0 * MB as f64);
    }

    #[test]
    fn new_measurements_move_the_estimate_gradually() {
        let start = Instant::now();
        let mut peer = Peer::new(start);
        peer.delivered(at(start, 1000), 10 * MB);

        peer.delivered(at(start, 2000), 20 * MB);

        assert_eq!(peer.speed_bps, 0.7 * 10.0 * MB as f64 + 0.3 * 20.0 * MB as f64);
    }

    #[test]
    fn time_spent_idle_before_a_request_is_not_counted_against_the_node() {
        let start = Instant::now();
        let mut peer = Peer::new(start);

        peer.request_started(at(start, 60_000));
        peer.delivered(at(start, 61_000), MB);

        assert_eq!(peer.speed_bps, MB as f64);
    }

    #[test]
    fn a_failed_request_halves_the_speed() {
        let mut peer = Peer::new(Instant::now());
        peer.speed_bps = 8.0;

        peer.released(Fault::Failed);

        assert_eq!(peer.speed_bps, 4.0);
    }

    #[test]
    fn a_busy_node_is_not_penalized() {
        let mut peer = Peer::new(Instant::now());
        peer.speed_bps = 8.0;

        for _ in 0..10 {
            peer.released(Fault::Busy);
        }

        assert_eq!((peer.speed_bps, peer.is_banned()), (8.0, false));
    }

    #[test]
    fn repeated_failures_exclude_the_node_but_a_delivery_forgives_them() {
        let start = Instant::now();
        let mut peer = Peer::new(start);
        peer.released(Fault::Failed);
        peer.released(Fault::Missing);
        peer.delivered(at(start, 300), MB);
        peer.released(Fault::Failed);
        peer.released(Fault::Failed);
        assert!(!peer.is_banned());

        peer.released(Fault::Missing);

        assert!(peer.is_banned());
    }

    #[test]
    fn a_second_corrupted_chunk_excludes_the_node() {
        let mut peer = Peer::new(Instant::now());

        peer.sent_bad_chunk();
        assert!(!peer.is_banned());
        peer.sent_bad_chunk();

        assert!(peer.is_banned());
    }
}
