//! Who dials whom, and how often: of two nodes only the lower node id dials, and it keeps retrying with jittered backoff.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{is_revoked, quic, AuthFailure};
use crate::types::NodeId;
use crate::NodeState;

const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
// A wrong passphrase won't fix itself soon, so those retries back off much further.
const AUTH_RETRY_BASE: Duration = Duration::from_secs(15);
const AUTH_RETRY_MAX: Duration = Duration::from_secs(600);
// Dialing stops once nobody has asked for the peer in this long, as when discovery stops hearing it.
const GIVE_UP: Duration = Duration::from_secs(600);

/// Peers with a dial loop running, and where to reach them.
pub type DialTargets = Arc<Mutex<HashMap<NodeId, DialTarget>>>;

#[derive(Debug, Clone, Copy)]
pub struct DialTarget {
    addr: SocketAddr,
    wanted_at: Instant,
}

/// Whether this node is the one that dials `peer_node_id`, so two nodes never dial each other at once.
pub fn is_dialer(our_node_id: &str, peer_node_id: &str) -> bool {
    our_node_id < peer_node_id
}

/// Dials the peer until connected, unless the other node is the dialer; a loop already running just learns the new address.
pub fn dial(state: &NodeState, peer_node_id: &NodeId, addr: SocketAddr) {
    if !is_dialer(&state.node_id, peer_node_id) {
        return;
    }
    let target = DialTarget { addr, wanted_at: Instant::now() };
    if state.dial_targets.lock().unwrap().insert(peer_node_id.clone(), target).is_some() {
        return;
    }
    tokio::spawn(dial_until_connected(state.clone(), peer_node_id.clone()));
}

async fn dial_until_connected(state: NodeState, peer_node_id: NodeId) {
    let mut failures = 0;
    let mut auth_failed = false;
    loop {
        if failures > 0 {
            tokio::time::sleep(retry_delay(failures, auth_failed, rand::random())).await;
        }
        let Some(addr) = next_address(&state, &peer_node_id).await else { return };
        match quic::connect(&state, addr, &peer_node_id).await {
            Ok(()) => break,
            Err(e) => {
                failures += 1;
                auth_failed = e.downcast_ref::<AuthFailure>().is_some();
                tracing::warn!("Mesh: dial {failures} to {peer_node_id} at {addr} failed: {e:#}");
            }
        }
    }
    state.dial_targets.lock().unwrap().remove(&peer_node_id);
}

/// Where to dial the peer next; None, and the loop is over, once it is connected, revoked or no longer wanted.
async fn next_address(state: &NodeState, peer_node_id: &NodeId) -> Option<SocketAddr> {
    let connected = state.mesh_peers.read().await.contains_key(peer_node_id);
    let mut targets = state.dial_targets.lock().unwrap();
    let target = targets.get(peer_node_id).copied()?;
    if connected || is_revoked(state, peer_node_id) || target.wanted_at.elapsed() > GIVE_UP {
        targets.remove(peer_node_id);
        return None;
    }
    Some(target.addr)
}

/// How long to wait after `failures` failed dials in a row; `jitter` in [0, 1) spreads the retries of different nodes apart.
pub fn retry_delay(failures: u32, auth_failed: bool, jitter: f64) -> Duration {
    let (base, max) = if auth_failed { (AUTH_RETRY_BASE, AUTH_RETRY_MAX) } else { (RETRY_BASE, RETRY_MAX) };
    let doublings = failures.saturating_sub(1).min(16);
    let full = base.saturating_mul(1 << doublings).min(max);
    full.mul_f64(0.5 + jitter.clamp(0.0, 1.0) / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lower_node_id_dials() {
        assert!(is_dialer("node_a", "node_b"));
    }

    #[test]
    fn the_higher_node_id_waits_to_be_dialed() {
        assert!(!is_dialer("node_b", "node_a"));
    }

    #[test]
    fn exactly_one_of_two_nodes_dials() {
        assert_ne!(is_dialer("node_x", "node_y"), is_dialer("node_y", "node_x"));
    }

    #[test]
    fn the_first_retry_comes_within_a_second() {
        assert_eq!(retry_delay(1, false, 0.0), Duration::from_millis(500));
        assert_eq!(retry_delay(1, false, 1.0), Duration::from_secs(1));
    }

    #[test]
    fn each_failure_doubles_the_wait() {
        assert_eq!(retry_delay(4, false, 1.0), Duration::from_secs(8));
    }

    #[test]
    fn the_wait_is_capped_at_thirty_seconds() {
        assert_eq!(retry_delay(50, false, 1.0), RETRY_MAX);
    }

    #[test]
    fn jitter_keeps_at_least_half_the_wait() {
        assert_eq!(retry_delay(50, false, 0.0), RETRY_MAX / 2);
    }

    #[test]
    fn jitter_spreads_retries_between_half_and_all_of_the_wait() {
        assert_eq!(retry_delay(3, false, 0.5), Duration::from_secs(3));
    }

    #[test]
    fn an_authentication_failure_waits_much_longer() {
        assert_eq!(retry_delay(1, true, 1.0), AUTH_RETRY_BASE);
        assert_eq!(retry_delay(50, true, 1.0), AUTH_RETRY_MAX);
    }
}
