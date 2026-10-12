//! A UDP relay in front of a node's QUIC endpoint that can lose, delay and reorder datagrams, or go silent.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::Rng;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio::time::Instant;

const MAX_DATAGRAM_BYTES: usize = 65_535;
// What a throttle holds back before it starts losing datagrams, as a router's queue does, so senders slow down instead of queueing.
const MAX_BACKLOG: Duration = Duration::from_millis(100);

/// What happens to each datagram crossing a relay, in either direction.
#[derive(Clone, Copy, Debug, Default)]
pub struct Faults {
    /// Chance of a datagram being lost, from 0.0 to 1.0.
    pub drop: f64,
    pub delay: Duration,
    /// A random extra delay of up to this much on top of `delay`.
    pub jitter: Duration,
    /// Chance of a datagram skipping the delay and overtaking earlier ones, as in netem.
    pub reorder: f64,
}

impl Faults {
    /// How long to hold a datagram back, or None to lose it.
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

/// One direction of a node's network card: datagrams cross it one after another at a fixed speed.
pub struct Throttle {
    seconds_per_byte: f64,
    free_at: Mutex<Instant>,
}

impl Throttle {
    pub fn new(bytes_per_second: u64) -> Arc<Self> {
        Arc::new(Self { seconds_per_byte: 1.0 / bytes_per_second as f64, free_at: Mutex::new(Instant::now()) })
    }

    /// When a datagram of `len` bytes that is ready at `ready` has fully crossed, or None if the queue ahead of it is full.
    fn finish(&self, ready: Instant, len: usize) -> Option<Instant> {
        let mut free_at = self.free_at.lock().unwrap();
        let start = (*free_at).max(ready);
        if start - ready > MAX_BACKLOG {
            return None;
        }
        let done = start + Duration::from_secs_f64(self.seconds_per_byte * len as f64);
        *free_at = done;
        Some(done)
    }
}

/// The cards a relay's datagrams cross: toward the node behind it (`upstream`) and back to the sender (`back`).
#[derive(Default)]
struct Cards {
    upstream: Vec<Arc<Throttle>>,
    back: Vec<Arc<Throttle>>,
}

struct Link {
    faults: Mutex<Faults>,
    cards: Mutex<Cards>,
    is_silent: AtomicBool,
}

pub struct UdpRelay {
    addr: SocketAddr,
    link: Arc<Link>,
    task: JoinHandle<()>,
}

impl UdpRelay {
    /// Passes datagrams sent to [`UdpRelay::addr`] on to `upstream`, and its replies back to the sender.
    pub async fn start(upstream: SocketAddr) -> Self {
        let near = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind the relay's near side");
        let far = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind the relay's far side");
        let addr = near.local_addr().expect("the relay has an address");
        let link = Arc::new(Link { faults: Mutex::new(Faults::default()), cards: Mutex::default(), is_silent: AtomicBool::new(false) });
        let task = tokio::spawn(relay(near, far, upstream, link.clone()));
        Self { addr, link, task }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn set_faults(&self, faults: Faults) {
        *self.link.faults.lock().unwrap() = faults;
    }

    /// Makes datagrams cross the given throttles: `upstream` ones heading to the node behind the relay, `back` ones coming from it.
    pub fn set_throttles(&self, upstream: Vec<Arc<Throttle>>, back: Vec<Arc<Throttle>>) {
        *self.link.cards.lock().unwrap() = Cards { upstream, back };
    }

    /// Drops every packet, both ways, until [`UdpRelay::resume`].
    pub fn go_silent(&self) {
        self.link.is_silent.store(true, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.link.is_silent.store(false, Ordering::Relaxed);
    }
}

impl Drop for UdpRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone, Copy)]
enum Way {
    Upstream,
    Back,
}

async fn relay(near: UdpSocket, far: UdpSocket, upstream: SocketAddr, link: Arc<Link>) {
    let mut sender = None;
    let mut outbound = vec![0u8; MAX_DATAGRAM_BYTES];
    let mut inbound = vec![0u8; MAX_DATAGRAM_BYTES];
    // Datagrams due at the same moment keep their order.
    let mut queue: BTreeMap<(Instant, u64), (Way, Vec<u8>)> = BTreeMap::new();
    let mut sequence = 0u64;
    loop {
        let next_due = queue.keys().next().map(|(due, _)| *due);
        let arrived = tokio::select! {
            Ok((len, from)) = near.recv_from(&mut outbound) => {
                sender = Some(from);
                Some((Way::Upstream, outbound[..len].to_vec()))
            }
            Ok((len, _)) = far.recv_from(&mut inbound) => Some((Way::Back, inbound[..len].to_vec())),
            _ = tokio::time::sleep_until(next_due.unwrap_or_else(Instant::now)), if next_due.is_some() => None,
        };
        match arrived {
            Some((way, datagram)) => {
                let roll = link.faults.lock().unwrap().roll();
                if let (Some(wait), false) = (roll, link.is_silent.load(Ordering::Relaxed)) {
                    let due = {
                        let cards = link.cards.lock().unwrap();
                        let crossed = match way {
                            Way::Upstream => &cards.upstream,
                            Way::Back => &cards.back,
                        };
                        crossed.iter().try_fold(Instant::now() + wait, |due, card| card.finish(due, datagram.len()))
                    };
                    if let Some(due) = due {
                        queue.insert((due, sequence), (way, datagram));
                        sequence += 1;
                    }
                }
            }
            None => {
                let (_, (way, datagram)) = queue.pop_first().expect("a datagram is due");
                if link.is_silent.load(Ordering::Relaxed) {
                    continue;
                }
                match (way, sender) {
                    (Way::Upstream, _) => {
                        let _ = far.send_to(&datagram, upstream).await;
                    }
                    (Way::Back, Some(sender)) => {
                        let _ = near.send_to(&datagram, sender).await;
                    }
                    (Way::Back, None) => {}
                }
            }
        }
    }
}
