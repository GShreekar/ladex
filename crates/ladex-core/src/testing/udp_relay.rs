//! A UDP relay in front of a node's QUIC endpoint that can go silent, as a link does when it drops every packet.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

const MAX_DATAGRAM_BYTES: usize = 65_535;

pub struct UdpRelay {
    addr: SocketAddr,
    is_silent: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl UdpRelay {
    /// Passes datagrams sent to [`UdpRelay::addr`] on to `upstream`, and its replies back to the sender.
    pub async fn start(upstream: SocketAddr) -> Self {
        let near = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind the relay's near side");
        let far = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.expect("bind the relay's far side");
        let addr = near.local_addr().expect("the relay has an address");
        let is_silent = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(relay(near, far, upstream, is_silent.clone()));
        Self { addr, is_silent, task }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Drops every packet, both ways, until [`UdpRelay::resume`].
    pub fn go_silent(&self) {
        self.is_silent.store(true, Ordering::Relaxed);
    }

    pub fn resume(&self) {
        self.is_silent.store(false, Ordering::Relaxed);
    }
}

impl Drop for UdpRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn relay(near: UdpSocket, far: UdpSocket, upstream: SocketAddr, is_silent: Arc<AtomicBool>) {
    let mut sender = None;
    let mut outbound = vec![0u8; MAX_DATAGRAM_BYTES];
    let mut inbound = vec![0u8; MAX_DATAGRAM_BYTES];
    loop {
        tokio::select! {
            Ok((len, from)) = near.recv_from(&mut outbound) => {
                sender = Some(from);
                if !is_silent.load(Ordering::Relaxed) {
                    let _ = far.send_to(&outbound[..len], upstream).await;
                }
            }
            Ok((len, _)) = far.recv_from(&mut inbound) => {
                if let (Some(sender), false) = (sender, is_silent.load(Ordering::Relaxed)) {
                    let _ = near.send_to(&inbound[..len], sender).await;
                }
            }
        }
    }
}
