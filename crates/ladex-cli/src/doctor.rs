//! `ladex doctor`: checks that the node is running, can be reached from the network and has room to work.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use ladex_core::control::{Reply, Request, Status};
use ladex_core::tls;
use serde::Serialize;

use crate::commands::{is_connection_refused, not_running, Session};
use crate::output::{self, format_bytes};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const LOW_DISK_BYTES: u64 = 1 << 30;
const NEARLY_FULL_PERCENT: u64 = 90;
const SLOW_NODE_MS: u32 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    Ok,
    Warning,
    Problem,
}

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    verdict: Verdict,
    detail: String,
}

fn check(name: &'static str, verdict: Verdict, detail: impl Into<String>) -> Check {
    Check { name, verdict, detail: detail.into() }
}

fn check_address(status: &Status) -> Check {
    match status.local_ip {
        Some(ip) => check("Network address", Verdict::Ok, ip.to_string()),
        None => check("Network address", Verdict::Problem, "this machine has no address on a local network; connect to Wi-Fi or Ethernet"),
    }
}

fn check_page(status: &Status, is_reachable: Option<bool>) -> Check {
    let port = status.http_port;
    match is_reachable {
        Some(true) => check("Page for other devices", Verdict::Ok, format!("answering on TCP port {port}")),
        Some(false) => check(
            "Page for other devices",
            Verdict::Problem,
            format!("nothing answers on TCP port {port} at this machine's network address"),
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

fn check_discovery(status: &Status) -> Check {
    match status.discovery_port {
        Some(port) => {
            check("Discovery", Verdict::Ok, format!("announcing on UDP port {port}; nodes connect on UDP port {}", status.quic_port))
        }
        None => check("Discovery", Verdict::Warning, "switched off; other nodes must be given with --peer"),
    }
}

fn check_nodes(status: &Status) -> Check {
    if status.nodes.is_empty() {
        let hint = "none connected. If other nodes are running: are they on the same network, is client isolation off on the \
                    router, and does a firewall block the UDP ports above?";
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

fn checks_of(status: &Status, is_page_reachable: Option<bool>) -> Vec<Check> {
    let running = format!("{} (version {}, id {})", status.node_name, status.version, status.node_id);
    vec![
        check("Node", Verdict::Ok, running),
        check_address(status),
        check_page(status, is_page_reachable),
        check_encryption(status),
        check_passphrase(status),
        check_discovery(status),
        check_nodes(status),
        check_storage(status),
        check_disk(status),
    ]
}

fn checks_without_a_node(data_dir: &Path) -> Vec<Check> {
    let address = match tls::local_ipv4_addresses().first() {
        Some(ip) => check("Network address", Verdict::Ok, ip.to_string()),
        None => check("Network address", Verdict::Problem, "this machine has no address on a local network; connect to Wi-Fi or Ethernet"),
    };
    vec![check("Node", Verdict::Problem, format!("{:#}", not_running(data_dir))), address]
}

async fn is_reachable(address: SocketAddr) -> bool {
    matches!(tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(address)).await, Ok(Ok(_)))
}

async fn examine(data_dir: &Path) -> anyhow::Result<Vec<Check>> {
    let Ok(session) = Session::open(data_dir, true) else { return Ok(checks_without_a_node(data_dir)) };
    let status = match session.call(&Request::Status).await {
        Ok(Reply::Status(status)) => *status,
        Ok(_) => anyhow::bail!("the node answered something else"),
        Err(e) if is_connection_refused(&e) => return Ok(checks_without_a_node(data_dir)),
        Err(e) => return Err(e),
    };
    let is_page_reachable = match status.local_ip {
        Some(ip) => Some(is_reachable(SocketAddr::new(ip, status.http_port)).await),
        None => None,
    };
    Ok(checks_of(&status, is_page_reachable))
}

fn print_checks(checks: &[Check]) {
    let mark = |verdict: Verdict| match verdict {
        Verdict::Ok => "ok",
        Verdict::Warning => "warning",
        Verdict::Problem => "PROBLEM",
    };
    let rows: Vec<Vec<String>> = checks.iter().map(|c| vec![c.name.to_string(), mark(c.verdict).to_string(), c.detail.clone()]).collect();
    output::print_table(&["CHECK", "RESULT", "DETAIL"], &rows);
}

/// Runs every check and returns the exit code: failure if any found a problem.
pub async fn run(data_dir: &Path, is_json: bool) -> i32 {
    let checks = match examine(data_dir).await {
        Ok(checks) => checks,
        Err(e) => vec![check("Node", Verdict::Problem, format!("{e:#}"))],
    };
    if is_json {
        println!("{}", serde_json::json!({ "checks": checks }));
    } else {
        print_checks(&checks);
    }
    checks.iter().any(|check| check.verdict == Verdict::Problem) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use ladex_core::control::Node;

    fn healthy() -> Status {
        Status {
            version: "3.0.0".into(),
            node_id: "node_1".into(),
            node_name: "laptop".into(),
            local_ip: Some("192.168.1.20".parse().unwrap()),
            http_port: 8080,
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

    fn verdict_of(checks: &[Check], name: &str) -> Verdict {
        checks.iter().find(|check| check.name == name).expect("the check exists").verdict
    }

    #[test]
    fn a_healthy_node_passes_every_check() {
        let checks = checks_of(&healthy(), Some(true));
        assert!(checks.iter().all(|check| check.verdict == Verdict::Ok), "{checks:?}");
    }

    #[test]
    fn a_page_nothing_answers_on_is_a_problem() {
        assert_eq!(verdict_of(&checks_of(&healthy(), Some(false)), "Page for other devices"), Verdict::Problem);
    }

    #[test]
    fn a_machine_without_a_network_address_is_a_problem() {
        let status = Status { local_ip: None, ..healthy() };
        assert_eq!(verdict_of(&checks_of(&status, None), "Network address"), Verdict::Problem);
    }

    #[test]
    fn an_open_unencrypted_node_only_gets_warnings() {
        let status = Status { is_encrypted: false, has_passphrase: false, ..healthy() };
        let checks = checks_of(&status, Some(true));
        assert_eq!(verdict_of(&checks, "Encryption"), Verdict::Warning);
        assert_eq!(verdict_of(&checks, "Passphrase"), Verdict::Warning);
    }

    #[test]
    fn a_node_alone_on_the_network_is_warned_about() {
        let status = Status { nodes: Vec::new(), ..healthy() };
        assert_eq!(verdict_of(&checks_of(&status, Some(true)), "Other nodes"), Verdict::Warning);
    }

    #[test]
    fn a_nearly_full_disk_is_a_problem() {
        let status = Status { disk_free_bytes: Some(100 << 20), ..healthy() };
        assert_eq!(verdict_of(&checks_of(&status, Some(true)), "Disk"), Verdict::Problem);
    }

    #[test]
    fn a_store_close_to_its_limit_is_warned_about() {
        let status = Status { store_used_bytes: 19 << 30, ..healthy() };
        assert_eq!(verdict_of(&checks_of(&status, Some(true)), "Storage"), Verdict::Warning);
    }
}
