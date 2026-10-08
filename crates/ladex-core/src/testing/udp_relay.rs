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

struct Link {
    faults: Mutex<Faults>,
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
        let link = Arc::new(Link { faults: Mutex::new(Faults::default()), is_silent: AtomicBool::new(false) });
        let task = tokio::spawn(relay(near, far, upstream, link.clone()));
        Self { addr, link, task }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn set_faults(&self, faults: Faults) {
        *self.link.faults.lock().unwrap() = faults;
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
                    queue.insert((Instant::now() + wait, sequence), (way, datagram));
                    sequence += 1;
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
