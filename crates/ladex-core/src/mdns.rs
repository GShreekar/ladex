//! Advertises this node over mDNS as ladex.local, alongside the IP-based URLs.

use mdns_sd::{DaemonEvent, ServiceDaemon, ServiceInfo};
use std::net::IpAddr;
use std::time::Duration;

const HOST_NAME: &str = "ladex.local.";
const SERVICE_TYPE: &str = "_ladex._tcp.local.";

pub struct MdnsHandle {
    daemon: ServiceDaemon,
    fullname: String,
}

/// Starts the mDNS daemon and registers the host name and service; never fatal, returns None on failure.
pub fn advertise(local_ips: &[IpAddr], port: u16, tls: bool) -> Option<MdnsHandle> {
    if local_ips.is_empty() {
        return None;
    }

    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("mDNS: could not start daemon: {e}");
            return None;
        }
    };

    let scheme = if tls { "https" } else { "http" };
    let instance_name = crate::types::hostname();
    let props: [(&str, &str); 2] = [("scheme", scheme), ("path", "/")];

    let service = match ServiceInfo::new(SERVICE_TYPE, &instance_name, HOST_NAME, local_ips, port, &props[..]) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("mDNS: could not build service info: {e}");
            return None;
        }
    };
    let fullname = service.get_fullname().to_string();

    if let Err(e) = daemon.register(service) {
        tracing::warn!("mDNS: could not register service: {e}");
        return None;
    }

    let host = HOST_NAME.trim_end_matches('.');
    println!("Access via mDNS: {scheme}://{host}:{port} (most phones/computers; Windows may need Bonjour installed)");

    // Another node may already hold ladex.local and get this one renamed; watch for that to report the real name.
    if let Ok(monitor) = daemon.monitor() {
        tokio::spawn(async move {
            while let Ok(event) = monitor.recv_async().await {
                if let DaemonEvent::NameChange(change) = event {
                    if change.original == HOST_NAME {
                        let new_host = change.new_name.trim_end_matches('.');
                        tracing::warn!("mDNS: {host} was already in use — renamed to {new_host}");
                        println!("Note: {host} was already taken on this network — this node is reachable at {scheme}://{new_host}:{port} instead");
                    }
                }
            }
        });
    }

    Some(MdnsHandle { daemon, fullname })
}

/// Sends mDNS goodbye records so other devices drop this node; best-effort.
pub async fn shutdown(handle: MdnsHandle) {
    if let Ok(recv) = handle.daemon.unregister(&handle.fullname) {
        let _ = tokio::time::timeout(Duration::from_millis(500), recv.recv_async()).await;
    }
}
