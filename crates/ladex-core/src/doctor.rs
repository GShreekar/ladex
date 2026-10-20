//! The network doctor: checks that a node is running, can be reached from the network and has room to work.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::control::Status;
use crate::{discovery, tls};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const MULTICAST_TIMEOUT: Duration = Duration::from_secs(1);
const LOW_DISK_BYTES: u64 = 1 << 30;
const NEARLY_FULL_PERCENT: u64 = 90;
const SLOW_NODE_MS: u32 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Ok,
    Warning,
    Problem,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub verdict: Verdict,
    pub detail: String,
}

pub fn check(name: &str, verdict: Verdict, detail: impl Into<String>) -> Check {
    Check { name: name.to_string(), verdict, detail: detail.into() }
}

/// What probing the machine found out, which a node's status cannot say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probes {
    /// Whether the node's own page answered at its network address; None when there was no address to try.
    pub is_page_reachable: Option<bool>,
    /// Whether a multicast packet sent from this machine was heard by it; None when discovery is off.
    pub is_multicast_heard: Option<bool>,
    pub interfaces: Vec<IpAddr>,
}

/// Probes this machine and runs every check on the node described by `status`.
pub async fn examine(status: &Status) -> Vec<Check> {
    let is_page_reachable = match status.local_ip {
        Some(ip) => Some(is_reachable(SocketAddr::new(ip, status.http_port)).await),
        None => None,
    };
    let is_multicast_heard = match status.discovery_port {
        Some(port) => Some(is_multicast_heard(port).await),
        None => None,
    };
    checks_of(status, &Probes { is_page_reachable, is_multicast_heard, interfaces: tls::local_ipv4_addresses() })
}

async fn is_reachable(address: SocketAddr) -> bool {
    matches!(tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(address)).await, Ok(Ok(_)))
}

// A second socket on the discovery port sends to the group and listens for it, so a firewall or VPN that eats multicast shows up.
async fn is_multicast_heard(port: u16) -> bool {
    let Ok(listener) = discovery::bind_probe_socket(port) else { return false };
    let Ok(sender) = tokio::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await else { return false };
    if sender.set_multicast_loop_v4(true).is_err() {
        return false;
    }
    let probe = format!("ladex-multicast-test {}", rand::random::<u64>());
    let target = SocketAddrV4::new(discovery::MULTICAST_ADDR, port);
    if sender.send_to(probe.as_bytes(), target).await.is_err() {
        return false;
    }
    let mut buffer = [0u8; 128];
    let heard = async {
        loop {
            match listener.recv_from(&mut buffer).await {
                Ok((length, _)) if &buffer[..length] == probe.as_bytes() => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    };
    tokio::time::timeout(MULTICAST_TIMEOUT, heard).await.unwrap_or(false)
}

fn check_address(status: &Status, interfaces: &[IpAddr]) -> Check {
    let Some(ip) = status.local_ip else {
        return check("Network address", Verdict::Problem, "this machine has no address on a local network; connect to Wi-Fi or Ethernet");
    };
    let others: Vec<String> = interfaces.iter().filter(|other| **other != ip).map(ToString::to_string).collect();
    if others.is_empty() {
        return check("Network address", Verdict::Ok, ip.to_string());
    }
    check("Network address", Verdict::Ok, format!("{ip} (also on {})", others.join(", ")))
}

fn check_page(status: &Status, is_reachable: Option<bool>) -> Check {
    let port = status.http_port;
    match is_reachable {
        Some(true) => check("Page for other devices", Verdict::Ok, format!("answering on TCP port {port}")),
        Some(false) => check(
            "Page for other devices",
            Verdict::Problem,
            format!("nothing answers on TCP port {port} at this machine's network address; a firewall may be blocking it"),
        ),
        None => check("Page for other devices", Verdict::Warning, "not checked without a network address"),
    }
}

fn check_encryption(status: &Status) -> Check {
    if status.is_encrypted {
        return check("Encryption", Verdict::Ok, "browsers connect over TLS");
    }
    check("Encryption", Verdict::Warning, "browsers connect without TLS, so their traffic can be read on the network")
}

fn check_passphrase(status: &Status) -> Check {
    if status.has_passphrase {
        return check("Passphrase", Verdict::Ok, "needed to sign in and to join the mesh");
    }
    check("Passphrase", Verdict::Warning, "none: anyone on this network can open the page and join the mesh")
}

fn check_discovery(status: &Status, is_multicast_heard: Option<bool>) -> Check {
    let Some(port) = status.discovery_port else {
        return check(
            "Discovery",
            Verdict::Warning,
            "switched off; add other nodes by address (Network page, or ladex settings --add-peer)",
        );
    };
    let announcing = format!("announcing on UDP port {port}; nodes connect on UDP port {}", status.quic_port);
    match is_multicast_heard {
        Some(false) => check(
            "Discovery",
            Verdict::Warning,
            format!("{announcing}, but this machine did not hear its own test packet: a firewall or VPN may be blocking multicast"),
        ),
        _ => check("Discovery", Verdict::Ok, announcing),
    }
}

fn check_nodes(status: &Status, is_multicast_heard: Option<bool>) -> Check {
    if status.nodes.is_empty() {
        let hint = if is_multicast_heard == Some(false) {
            "none connected, and multicast does not work on this machine (see Discovery)"
        } else {
            "none connected. If other nodes are running: are they on the same network, and is client isolation off on the router? \
             If it cannot be turned off, let one device share a hotspot and join the others to it"
        };
        return check("Other nodes", Verdict::Warning, hint);
    }
    let slowest = status.nodes.iter().filter_map(|node| node.rtt_ms).max();
    let detail = match slowest {
        Some(ms) => format!("{} connected, the slowest {ms} ms away", status.nodes.len()),
        None => format!("{} connected", status.nodes.len()),
    };
    let verdict = if slowest.is_some_and(|ms| ms > SLOW_NODE_MS) { Verdict::Warning } else { Verdict::Ok };
    check("Other nodes", verdict, detail)
}

fn check_storage(status: &Status) -> Check {
    let (used, quota) = (status.store_used_bytes, status.store_quota_bytes);
    let detail = format!("{} of {} used in {}", format_bytes(used), format_bytes(quota), status.data_dir.display());
    let is_nearly_full = used.saturating_mul(100).checked_div(quota).is_some_and(|percent| percent >= NEARLY_FULL_PERCENT);
    check("Storage", if is_nearly_full { Verdict::Warning } else { Verdict::Ok }, detail)
}

fn check_disk(status: &Status) -> Check {
    match status.disk_free_bytes {
        Some(free) if free < LOW_DISK_BYTES => {
            check("Disk", Verdict::Problem, format!("only {} free; new files may be refused", format_bytes(free)))
        }
        Some(free) => check("Disk", Verdict::Ok, format!("{} free", format_bytes(free))),
        None => check("Disk", Verdict::Warning, "could not tell how much space is free"),
    }
}

/// The checks that follow from a node's status and what probing found.
pub fn checks_of(status: &Status, probes: &Probes) -> Vec<Check> {
    let running = format!("{} (version {}, id {})", status.node_name, status.version, status.node_id);
    vec![
        check("Node", Verdict::Ok, running),
        check_address(status, &probes.interfaces),
        check_page(status, probes.is_page_reachable),
        check_encryption(status),
        check_passphrase(status),
        check_discovery(status, probes.is_multicast_heard),
        check_nodes(status, probes.is_multicast_heard),
        check_storage(status),
        check_disk(status),
    ]
}

/// What can be said about a machine when no node is running on it.
pub fn checks_without_a_node(reason: &str) -> Vec<Check> {
    let address = match tls::local_ipv4_addresses().first() {
        Some(ip) => check("Network address", Verdict::Ok, ip.to_string()),
        None => check("Network address", Verdict::Problem, "this machine has no address on a local network; connect to Wi-Fi or Ethernet"),
    };
    vec![check("Node", Verdict::Problem, reason), address]
}

pub fn has_problem(checks: &[Check]) -> bool {
    checks.iter().any(|check| check.verdict == Verdict::Problem)
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} B");
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Node;

    fn healthy() -> Status {
        Status {
            version: "3.0.0".into(),
            node_id: "node_1".into(),
            node_name: "laptop".into(),
            local_ip: Some("192.168.1.20".parse().unwrap()),
            http_port: 8080,
            local_http_port: Some(8081),
            quic_port: 8080,
            discovery_port: Some(7878),
            is_encrypted: true,
            has_passphrase: true,
            nodes: vec![Node { id: "node_2".into(), name: "desk".into(), address: "192.168.1.21:8080".parse().unwrap(), rtt_ms: Some(3) }],
            devices: 2,
            shared_files: 4,
            downloads_running: 0,
            data_dir: "/data".into(),
            store_used_bytes: 1 << 30,
            store_quota_bytes: 20 << 30,
            disk_free_bytes: Some(50 << 30),
        }
    }

    fn working() -> Probes {
        Probes { is_page_reachable: Some(true), is_multicast_heard: Some(true), interfaces: vec!["192.168.1.20".parse().unwrap()] }
    }

    fn verdict_of(checks: &[Check], name: &str) -> Verdict {
        checks.iter().find(|check| check.name == name).expect("the check exists").verdict
    }

    #[test]
    fn a_healthy_node_passes_every_check() {
        let checks = checks_of(&healthy(), &working());
        assert!(checks.iter().all(|check| check.verdict == Verdict::Ok), "{checks:?}");
    }

    #[test]
    fn a_page_nothing_answers_on_is_a_problem() {
        let probes = Probes { is_page_reachable: Some(false), ..working() };
        assert_eq!(verdict_of(&checks_of(&healthy(), &probes), "Page for other devices"), Verdict::Problem);
    }

    #[test]
    fn a_machine_without_a_network_address_is_a_problem() {
        let status = Status { local_ip: None, ..healthy() };
        assert_eq!(verdict_of(&checks_of(&status, &Probes::default()), "Network address"), Verdict::Problem);
    }

    #[test]
    fn every_network_address_is_listed_with_the_one_in_use_first() {
        let probes = Probes { interfaces: vec!["192.168.1.20".parse().unwrap(), "10.0.0.4".parse().unwrap()], ..working() };
        let checks = checks_of(&healthy(), &probes);
        let detail = &checks.iter().find(|check| check.name == "Network address").unwrap().detail;
        assert_eq!(detail, "192.168.1.20 (also on 10.0.0.4)");
    }

    #[test]
    fn an_open_unencrypted_node_only_gets_warnings() {
        let status = Status { is_encrypted: false, has_passphrase: false, ..healthy() };
        let checks = checks_of(&status, &working());
        assert_eq!(verdict_of(&checks, "Encryption"), Verdict::Warning);
        assert_eq!(verdict_of(&checks, "Passphrase"), Verdict::Warning);
    }

    #[test]
    fn a_node_alone_on_the_network_is_warned_about() {
        let status = Status { nodes: Vec::new(), ..healthy() };
        assert_eq!(verdict_of(&checks_of(&status, &working()), "Other nodes"), Verdict::Warning);
    }

    #[test]
    fn a_node_alone_with_working_multicast_is_pointed_at_client_isolation() {
        let status = Status { nodes: Vec::new(), ..healthy() };
        let checks = checks_of(&status, &working());
        let detail = &checks.iter().find(|check| check.name == "Other nodes").unwrap().detail;
        assert!(detail.contains("client isolation") && detail.contains("hotspot"), "{detail}");
    }

    #[test]
    fn multicast_that_is_not_heard_is_warned_about_where_discovery_is_checked() {
        let probes = Probes { is_multicast_heard: Some(false), ..working() };
        assert_eq!(verdict_of(&checks_of(&healthy(), &probes), "Discovery"), Verdict::Warning);
    }

    #[test]
    fn a_nearly_full_disk_is_a_problem() {
        let status = Status { disk_free_bytes: Some(100 << 20), ..healthy() };
        assert_eq!(verdict_of(&checks_of(&status, &working()), "Disk"), Verdict::Problem);
    }

    #[test]
    fn a_store_close_to_its_limit_is_warned_about() {
        let status = Status { store_used_bytes: 19 << 30, ..healthy() };
        assert_eq!(verdict_of(&checks_of(&status, &working()), "Storage"), Verdict::Warning);
    }

    #[test]
    fn a_problem_is_found_among_the_checks() {
        assert!(has_problem(&[check("Disk", Verdict::Problem, "full")]));
        assert!(!has_problem(&[check("Disk", Verdict::Warning, "low")]));
    }
}
