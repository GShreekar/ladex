//! Preferences, passphrase and doctor, as the app's settings and doctor pages and `ladex settings` use them.

use std::sync::Arc;

use bytes::Bytes;
use hyper::Method;
use ladex_core::control::server::{self, Endpoint};
use ladex_core::control::{Reply, Request};
use ladex_core::node::{LocalLink, Node, NodeConfig, NodeHandle, UiAssets, WebUi};
use ladex_core::preferences::{AutoAccept, AutoAcceptFrom, Update};
use ladex_core::saved_passphrase;
use serde_json::{json, Value};

struct StubUi;

impl UiAssets for StubUi {
    fn file(&self, path: &str) -> Option<&[u8]> {
        (path == "index.html").then_some(b"<!doctype html><title>stub ui</title>".as_slice())
    }
}

fn config_in(data_dir: &std::path::Path) -> NodeConfig {
    let mut config = NodeConfig::new(data_dir.to_path_buf(), WebUi { assets: Arc::new(StubUi), is_legacy: false });
    config.http_port = 0;
    config.discovery_port = None;
    config.use_tls = false;
    config.use_keychain = false;
    config
}

async fn started(data_dir: &std::path::Path) -> NodeHandle {
    Node::start(config_in(data_dir)).await.expect("the node starts")
}

async fn call(link: &LocalLink, method: Method, path: &str, body: Value) -> (u16, Value) {
    let headers = [("Content-Type".to_string(), "application/json".to_string())];
    let reply = link.request(method, path, &headers, Bytes::from(body.to_string())).await.expect("the node answers");
    (reply.status, serde_json::from_slice(&reply.body).unwrap_or(Value::Null))
}

async fn preferences_by_command(endpoint: &Endpoint, update: Option<Update>) -> ladex_core::preferences::View {
    match server::call(endpoint, &Request::Preferences { update }, |_| {}).await.unwrap() {
        Reply::Preferences(view) => *view,
        other => panic!("expected the preferences, got {other:?}"),
    }
}

#[tokio::test]
async fn the_preferences_are_the_defaults_on_a_new_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;

    let (status, view) = call(&node.local_link(), Method::GET, "/api/preferences", Value::Null).await;

    assert_eq!(status, 200);
    assert_eq!(view["preferences"]["is_setup_done"], false);
    assert_eq!(view["preferences"]["storage_limit_gib"], 20);
    assert_eq!(view["has_passphrase"], false);
    node.shutdown().await;
}

#[tokio::test]
async fn a_new_storage_limit_takes_effect_in_the_running_node_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;

    let (status, _) = call(&node.local_link(), Method::PUT, "/api/preferences", json!({ "storage_limit_gib": 3 })).await;

    assert_eq!(status, 200);
    assert_eq!(node.state().store.quota(), 3 << 30);
    node.shutdown().await;
    let mut restarted = config_in(dir.path());
    restarted.use_saved_storage_limit();
    assert_eq!(restarted.storage_limit_bytes, 3 << 30);
}

#[tokio::test]
async fn a_limit_the_node_cannot_keep_is_refused_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;

    let (status, reply) = call(&node.local_link(), Method::PUT, "/api/preferences", json!({ "storage_limit_gib": 0 })).await;

    assert_eq!(status, 400);
    assert!(reply["message"].as_str().unwrap().contains("storage limit"), "{reply}");
    assert_eq!(node.state().store.quota(), 20 << 30);
    node.shutdown().await;
}

#[tokio::test]
async fn a_changed_passphrase_is_saved_for_the_next_start_and_the_node_asks_for_one() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;

    let (status, view) = call(&node.local_link(), Method::PUT, "/api/passphrase", json!({ "passphrase": "maple-river-stone-lamp" })).await;

    assert_eq!(status, 200);
    assert_eq!(view["needs_restart"], true);
    assert_eq!(view["has_passphrase"], false, "the running node keeps the passphrase it started with");
    node.shutdown().await;
    let mut restarted = config_in(dir.path());
    assert!(restarted.use_saved_passphrase().unwrap());
    assert_eq!(restarted.passphrase.as_deref(), Some("maple-river-stone-lamp"));
}

#[tokio::test]
async fn a_passphrase_given_on_the_command_line_wins_over_the_saved_one() {
    let dir = tempfile::tempdir().unwrap();
    saved_passphrase::save(dir.path(), false, "saved-one").unwrap();
    let mut config = config_in(dir.path());
    config.passphrase = Some("given-one".into());

    assert!(!config.use_saved_passphrase().unwrap());
    assert_eq!(config.passphrase.as_deref(), Some("given-one"));
}

#[tokio::test]
async fn an_empty_passphrase_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;

    let (status, _) = call(&node.local_link(), Method::PUT, "/api/passphrase", json!({ "passphrase": "" })).await;

    assert_eq!(status, 400);
    node.shutdown().await;
}

#[tokio::test]
async fn a_suggested_passphrase_is_a_fresh_one_each_time() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;
    let link = node.local_link();

    let (_, first) = call(&link, Method::GET, "/api/passphrase/suggestion", Value::Null).await;
    let (_, second) = call(&link, Method::GET, "/api/passphrase/suggestion", Value::Null).await;

    assert_ne!(first["passphrase"], second["passphrase"]);
    assert!(first["passphrase"].as_str().unwrap().len() >= 12);
    node.shutdown().await;
}

#[tokio::test]
async fn the_doctor_reports_the_node_it_runs_in() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;

    let (status, reply) = call(&node.local_link(), Method::GET, "/api/doctor", Value::Null).await;

    assert_eq!(status, 200);
    let checks = reply["checks"].as_array().unwrap();
    let names: Vec<&str> = checks.iter().map(|check| check["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"Node") && names.contains(&"Storage") && names.contains(&"Disk"), "{names:?}");
    node.shutdown().await;
}

#[tokio::test]
async fn the_same_preferences_are_reached_by_command() {
    let dir = tempfile::tempdir().unwrap();
    let node = started(dir.path()).await;
    let endpoint = Endpoint::load(dir.path()).expect("the node publishes its control socket");
    let rule = AutoAccept { from: AutoAcceptFrom::PairedDevices, ask_above_mib: Some(500) };

    let view =
        preferences_by_command(&endpoint, Some(Update { auto_accept: Some(rule), storage_limit_gib: Some(7), ..Default::default() })).await;

    assert_eq!((view.preferences.auto_accept, view.preferences.storage_limit_gib), (rule, 7));
    assert_eq!(node.state().store.quota(), 7 << 30);
    let (_, over_http) = call(&node.local_link(), Method::GET, "/api/preferences", Value::Null).await;
    assert_eq!(over_http["preferences"]["auto_accept"]["from"], "paired");
    node.shutdown().await;
}
