//! A lossy, delaying, reordering link between two in-process nodes, made of a UDP relay in front of each.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock, Mutex};

use super::udp_relay::{Faults, Throttle, UdpRelay};
use super::{eventually, fully_connected, spawn_node, NodeConfig, NodeHandle};
use crate::types::NodeId;

// Which link a node must go through to reach an address; nodes in every test share the process, so it is keyed by dialer.
static DETOURS: LazyLock<Mutex<HashMap<(NodeId, SocketAddr), SocketAddr>>> = LazyLock::new(Default::default);

/// Where `dialer` should dial to reach `target`: the `FaultyLink` between them, if there is one.
pub(crate) fn detour(dialer: &str, target: SocketAddr) -> SocketAddr {
    DETOURS.lock().unwrap().get(&(dialer.to_string(), target)).copied().unwrap_or(target)
}

pub struct FaultyLink {
    relays: [UdpRelay; 2],
    detours: [(NodeId, SocketAddr); 2],
}

impl Drop for FaultyLink {
    fn drop(&mut self) {
        let mut detours = DETOURS.lock().unwrap();
        self.detours.iter().for_each(|key| {
            detours.remove(key);
        });
    }
}

impl FaultyLink {
    /// Connects `b` to `a` through a new link; any redial between them goes through it too.
    pub async fn join(a: &NodeHandle, b: &NodeHandle) -> FaultyLink {
        let (to_a, to_b) = (UdpRelay::start(a.addr).await, UdpRelay::start(b.addr).await);
        let detours = [(a.state.node_id.clone(), b.addr), (b.state.node_id.clone(), a.addr)];
        {
            let mut registered = DETOURS.lock().unwrap();
            registered.insert(detours[0].clone(), to_b.addr());
            registered.insert(detours[1].clone(), to_a.addr());
        }
        let link = FaultyLink { relays: [to_a, to_b], detours };
        b.connect(link.relays[0].addr()).await.expect("b joins a through the link");
        link
    }

    pub fn set_faults(&self, faults: Faults) {
        self.relays.iter().for_each(|relay| relay.set_faults(faults));
    }

    /// Makes the link cross the cards of its two ends: a datagram leaves through its sender's `up` and arrives through its receiver's `down`.
    fn set_cards(&self, a: &Cards, b: &Cards) {
        // The relay in front of a node sees what goes to it as upstream and what it sends as back.
        self.relays[0].set_throttles(vec![b.up.clone(), a.down.clone()], vec![a.up.clone(), b.down.clone()]);
        self.relays[1].set_throttles(vec![a.up.clone(), b.down.clone()], vec![b.up.clone(), a.down.clone()]);
    }

    /// Drops every datagram until `heal`; a connection across it ends once QUIC's idle timeout passes.
    pub fn partition(&self) {
        self.relays.iter().for_each(UdpRelay::go_silent);
    }

    pub fn heal(&self) {
        self.relays.iter().for_each(UdpRelay::resume);
    }
}

/// One node's network card: its own speed in each direction, shared by all its links.
struct Cards {
    up: Arc<Throttle>,
    down: Arc<Throttle>,
}

/// Nodes where every pair talks through its own `FaultyLink`.
pub struct LinkedMesh {
    pub nodes: Vec<NodeHandle>,
    links: BTreeMap<(usize, usize), FaultyLink>,
}

impl LinkedMesh {
    /// `n` open nodes, each linked to every other one. Returns once they all see each other.
    pub async fn spawn(n: usize) -> LinkedMesh {
        let mut nodes = Vec::new();
        for i in 0..n {
            nodes.push(spawn_node(NodeConfig::named(&format!("node_{i}"))).await);
        }
        let mut links = BTreeMap::new();
        for i in 0..n {
            for j in i + 1..n {
                links.insert((i, j), FaultyLink::join(&nodes[i], &nodes[j]).await);
            }
        }
        assert!(eventually(async || fully_connected(&nodes).await).await, "the {n} nodes never all connected");
        LinkedMesh { nodes, links }
    }

    pub fn link(&self, i: usize, j: usize) -> &FaultyLink {
        &self.links[&(i.min(j), i.max(j))]
    }

    pub fn set_faults_everywhere(&self, faults: Faults) {
        self.links.values().for_each(|link| link.set_faults(faults));
    }

    /// Gives every node a card that moves `bytes_per_second` in each direction, however many peers it talks to.
    pub fn limit_bandwidth(&self, bytes_per_second: u64) {
        let cards: Vec<Cards> =
            self.nodes.iter().map(|_| Cards { up: Throttle::new(bytes_per_second), down: Throttle::new(bytes_per_second) }).collect();
        for (&(i, j), link) in &self.links {
            link.set_cards(&cards[i], &cards[j]);
        }
    }

    /// Cuts node `i` off from all the others.
    pub fn isolate(&self, i: usize) {
        self.links.iter().filter(|((a, b), _)| *a == i || *b == i).for_each(|(_, link)| link.partition());
    }

    pub fn heal_everywhere(&self) {
        self.links.values().for_each(FaultyLink::heal);
    }
}
