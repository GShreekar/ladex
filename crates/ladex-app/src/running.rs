//! The node this app runs inside its own process: starting it, replacing it with a fresh one, and stopping it.

use std::sync::{Arc, Mutex, RwLock};

use include_dir::{include_dir, Dir};
use ladex_core::node::{self, Bridge, Node, NodeConfig, NodeHandle, StartError, UiAssets};
use tauri::{AppHandle, Manager};

// The web UI built from `ui/`; the node serves it to browsers on the network, the window shows it itself.
static UI_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../ui/dist");

pub const PASSPHRASE_VARIABLE: &str = "LADEX_PASSPHRASE";

struct EmbeddedUi(&'static Dir<'static>);

impl UiAssets for EmbeddedUi {
    fn file(&self, path: &str) -> Option<&[u8]> {
        self.0.get_file(path).map(|file| file.contents())
    }
}

struct Running {
    bridge: Arc<Bridge>,
    handle: Mutex<Option<NodeHandle>>,
}

/// The app's node, as the window's commands reach it. It can be swapped for a fresh one without the window noticing the type change.
pub struct AppNode {
    running: RwLock<Arc<Running>>,
}

impl AppNode {
    pub fn bridge(&self) -> Arc<Bridge> {
        self.running.read().unwrap().bridge.clone()
    }

    pub fn take_handle(&self) -> Option<NodeHandle> {
        self.running.read().unwrap().handle.lock().unwrap().take()
    }
}

pub enum StartFailure {
    Node(StartError),
    /// Something saved for the node, such as its passphrase, could not be read, so starting would not be safe.
    Saved(anyhow::Error),
}

impl std::fmt::Display for StartFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Node(error) => write!(f, "{error}"),
            Self::Saved(error) => write!(f, "{error:#}"),
        }
    }
}

pub fn passphrase_from_environment() -> Option<String> {
    std::env::var(PASSPHRASE_VARIABLE).ok().filter(|passphrase| !passphrase.is_empty())
}

// What was chosen in the wizard or settings is in the data folder; the environment still wins for anyone who sets it.
fn node_config() -> Result<NodeConfig, StartFailure> {
    let mut config = NodeConfig::new(node::default_data_dir(), Arc::new(EmbeddedUi(&UI_DIR)));
    config.passphrase = passphrase_from_environment();
    config.use_saved_passphrase().map_err(StartFailure::Saved)?;
    config.use_saved_storage_limit();
    Ok(config)
}

async fn start(app: &AppHandle) -> Result<Running, StartFailure> {
    let node = Node::start(node_config()?).await.map_err(StartFailure::Node)?;
    for notice in &node.info().notices {
        tracing::warn!("{notice:?}");
    }
    let downloads = app.path().download_dir().unwrap_or_else(|_| node.info().data_dir.join("received"));
    let bridge = Bridge::new(node.local_link(), downloads).with_preferences(node.state().preferences.clone());
    Ok(Running { bridge: Arc::new(bridge), handle: Mutex::new(Some(node)) })
}

/// Starts the node and makes it the one the window's commands use; does nothing if one is already running.
pub async fn start_once(app: &AppHandle) -> Result<(), StartFailure> {
    if app.try_state::<AppNode>().is_some() {
        return Ok(());
    }
    let running = start(app).await?;
    app.manage(AppNode { running: RwLock::new(Arc::new(running)) });
    Ok(())
}

/// Stops the node and starts another from what is saved now, which is how a new passphrase takes effect.
pub async fn restart(app: &AppHandle) -> Result<(), StartFailure> {
    let Some(node) = app.try_state::<AppNode>() else { return start_once(app).await };
    if let Some(handle) = node.take_handle() {
        handle.shutdown().await;
    }
    let running = start(app).await?;
    *node.running.write().unwrap() = Arc::new(running);
    Ok(())
}
