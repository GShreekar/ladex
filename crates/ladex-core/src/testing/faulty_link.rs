// A bad network between two in-process nodes. The link relays the mesh
// WebSocket one message at a time, so each message can be lost, held back or
// overtaken, the way packets are on a real network. Nodes must run without TLS.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use rand::Rng;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use super::{eventually, fully_connected, spawn_node, NodeConfig, NodeHandle};

// Large enough for a full catalog or a chunk frame.
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// What happens to each message crossing the link, in either direction.
#[derive(Clone, Copy, Debug, Default)]
pub struct Faults {
    /// Chance of a message being lost, from 0.0 to 1.0.
    pub drop: f64,
    pub delay: Duration,
    /// A random extra delay of up to this much on top of `delay`.
    pub jitter: Duration,
    /// Chance of a message skipping the delay and overtaking earlier ones, as in netem.
    pub reorder: f64,
}

impl Faults {
    /// How long to hold a message back, or None to lose it.
    fn roll(&self) -> Option<Duration> {
        let mut rng = rand::thread_rng();
        if rng.gen_bool(self.drop) {
            return None;
        }
        if rng.gen_bool(self.reorder) {
            return Some(Duration::ZERO);
        }
        Some(self.delay + self.jitter.mul_f64(rng.gen::<f64>()))
    }
}

struct Shared {
    faults: Mutex<Faults>,
    partitioned: watch::Sender<bool>,
}

pub struct FaultyLink {
    shared: Arc<Shared>,
    relays: [JoinHandle<()>; 2],
}

impl Drop for FaultyLink {
    fn drop(&mut self) {
        self.relays.iter().for_each(JoinHandle::abort);
    }
}

impl FaultyLink {
    /// Connects `b` to `a` through a new link; any reconnection between them goes through it too.
    pub async fn join(a: &NodeHandle, b: &NodeHandle) -> FaultyLink {
        let listen = || TcpListener::bind((Ipv4Addr::LOCALHOST, 0));
        let (to_a, to_b) = (listen().await.expect("bind the link"), listen().await.expect("bind the link"));
        let (to_a_addr, to_b_addr) = (to_a.local_addr().unwrap(), to_b.local_addr().unwrap());
        let shared = Arc::new(Shared { faults: Mutex::new(Faults::default()), partitioned: watch::channel(false).0 });
        let link = FaultyLink {
            relays: [
                tokio::spawn(accept(to_a, a.addr, to_b_addr, shared.clone())),
                tokio::spawn(accept(to_b, b.addr, to_a_addr, shared.clone())),
            ],
            shared,
        };
        b.connect(to_a_addr).await.expect("b joins a through the link");
        link
    }

    pub fn set_faults(&self, faults: Faults) {
        *self.shared.faults.lock().unwrap() = faults;
    }

    /// Cuts the connection and refuses new ones until `heal`.
    pub fn partition(&self) {
        self.shared.partitioned.send_replace(true);
    }

    pub fn heal(&self) {
        self.shared.partitioned.send_replace(false);
    }
}

// `return_addr` is where the far node should redial whoever connects here: the link's other end.
async fn accept(listener: TcpListener, target: SocketAddr, return_addr: SocketAddr, shared: Arc<Shared>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        // Dropping the socket refuses the connection.
        if !*shared.partitioned.borrow() {
            tokio::spawn(relay(stream, target, return_addr, shared.clone()));
        }
    }
}

async fn relay(stream: TcpStream, target: SocketAddr, return_addr: SocketAddr, shared: Arc<Shared>) {
    let config = WebSocketConfig {
        max_message_size: Some(MAX_MESSAGE_BYTES),
        max_frame_size: Some(MAX_MESSAGE_BYTES),
        ..Default::default()
    };
    let Ok(dialer) = tokio_tungstenite::accept_async_with_config(stream, Some(config)).await else { return };
    let url = format!("ws://{target}/mesh");
    let Ok((target, _)) = tokio_tungstenite::connect_async_with_config(&url, Some(config), false).await else { return };
    let (dialer_tx, dialer_rx) = dialer.split();
    let (target_tx, target_rx) = target.split();
    let mut partitioned = shared.partitioned.subscribe();
    // Whichever finishes first ends the connection; dropping both sockets is the cut.
    tokio::select! {
        _ = forward(dialer_rx, target_tx, Some(return_addr), &shared) => {}
        _ = forward(target_rx, dialer_tx, None, &shared) => {}
        _ = partitioned.wait_for(|cut| *cut) => {}
    }
}

async fn forward(
    mut from: impl Stream<Item = Result<Message, WsError>> + Unpin,
    mut to: impl Sink<Message> + Unpin,
    redirect_hello_to: Option<SocketAddr>,
    shared: &Shared,
) {
    // Messages due at the same moment keep their order.
    let mut queue: BTreeMap<(Instant, u64), Message> = BTreeMap::new();
    let mut sequence = 0u64;
    loop {
        let next_due = queue.keys().next().map(|(due, _)| *due);
        tokio::select! {
            incoming = from.next() => {
                let mut message = match incoming {
                    Some(Ok(message)) if message.is_text() || message.is_binary() => message,
                    Some(Ok(message)) if !message.is_close() => continue,
                    _ => return,
                };
                if let Some(addr) = redirect_hello_to {
                    message = redirect_hello(message, addr);
                }
                let roll = shared.faults.lock().unwrap().roll();
                if let Some(wait) = roll {
                    queue.insert((Instant::now() + wait, sequence), message);
                    sequence += 1;
                }
            }
            _ = tokio::time::sleep_until(next_due.unwrap_or_else(Instant::now)), if next_due.is_some() => {
                let (_, message) = queue.pop_first().expect("a message is due");
                if to.send(message).await.is_err() {
                    return;
                }
            }
        }
    }
}

// A Hello names the address to redial its sender at; point that at the link as well.
fn redirect_hello(message: Message, addr: SocketAddr) -> Message {
    let Message::Text(text) = &message else { return message };
    let Ok(mut hello) = serde_json::from_str::<serde_json::Value>(text) else { return message };
    if hello["type"] != "hello" {
        return message;
    }
    hello["ip"] = addr.ip().to_string().into();
    hello["http_port"] = addr.port().into();
    Message::Text(hello.to_string())
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

    /// Cuts node `i` off from all the others.
    pub fn isolate(&self, i: usize) {
        self.links.iter().filter(|((a, b), _)| *a == i || *b == i).for_each(|(_, link)| link.partition());
    }

    pub fn heal_everywhere(&self) {
        self.links.values().for_each(FaultyLink::heal);
    }
}
