//! `ladex doctor`: checks that the node is running, can be reached from the network and has room to work.

use std::path::Path;

use ladex_core::control::{Reply, Request};
use ladex_core::doctor::{self, Check, Verdict};

use crate::commands::{is_connection_refused, not_running, Session};
use crate::output;

fn without_a_node(data_dir: &Path) -> Vec<Check> {
    doctor::checks_without_a_node(&format!("{:#}", not_running(data_dir)))
}

async fn examine(data_dir: &Path) -> anyhow::Result<Vec<Check>> {
    let Ok(session) = Session::open(data_dir, true) else { return Ok(without_a_node(data_dir)) };
    let status = match session.call(&Request::Status).await {
        Ok(Reply::Status(status)) => *status,
        Ok(_) => anyhow::bail!("the node answered something else"),
        Err(e) if is_connection_refused(&e) => return Ok(without_a_node(data_dir)),
        Err(e) => return Err(e),
    };
    Ok(doctor::examine(&status).await)
}

fn print_checks(checks: &[Check]) {
    let mark = |verdict: Verdict| match verdict {
        Verdict::Ok => "ok",
        Verdict::Warning => "warning",
        Verdict::Problem => "PROBLEM",
    };
    let rows: Vec<Vec<String>> = checks.iter().map(|c| vec![c.name.clone(), mark(c.verdict).to_string(), c.detail.clone()]).collect();
    output::print_table(&["CHECK", "RESULT", "DETAIL"], &rows);
}

/// Runs every check and returns the exit code: failure if any found a problem.
pub async fn run(data_dir: &Path, is_json: bool) -> i32 {
    let checks = match examine(data_dir).await {
        Ok(checks) => checks,
        Err(e) => vec![doctor::check("Node", Verdict::Problem, format!("{e:#}"))],
    };
    if is_json {
        println!("{}", serde_json::json!({ "checks": checks }));
    } else {
        print_checks(&checks);
    }
    doctor::has_problem(&checks) as i32
}
