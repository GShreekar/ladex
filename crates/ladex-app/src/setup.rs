//! The first run: the choices made in the wizard before any node starts, so the node never runs open on the network.

use std::path::{Path, PathBuf};

use ladex_core::node::{self, DataDirLock};
use ladex_core::preferences::{self, PreferencesStore, Update};
use ladex_core::{auth, saved_passphrase};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::running::{self, passphrase_from_environment, AppNode};
use crate::{launch, WINDOW_LABEL};

/// Whether the wizard should run: this data folder has never been set up and no node is running on it.
pub fn is_needed(data_dir: &Path) -> bool {
    if passphrase_from_environment().is_some() || preferences::load(data_dir).is_setup_done {
        return false;
    }
    // A node already running here belongs to someone who set it up their own way.
    DataDirLock::acquire(data_dir).is_ok()
}

#[derive(Serialize)]
pub struct SetupState {
    is_needed: bool,
    suggested_passphrase: String,
    default_downloads: Option<String>,
    default_storage_gib: u64,
    free_bytes: Option<u64>,
    data_dir: String,
}

#[tauri::command]
pub fn ladex_setup_state(app: AppHandle) -> SetupState {
    let data_dir = node::default_data_dir();
    let is_needed = app.try_state::<AppNode>().is_none() && is_needed(&data_dir);
    SetupState {
        is_needed,
        suggested_passphrase: auth::generate_passphrase(),
        default_downloads: app.path().download_dir().ok().map(|folder| folder.display().to_string()),
        default_storage_gib: preferences::Preferences::default().storage_limit_gib,
        free_bytes: preferences::free_space_at(&data_dir),
        data_dir: data_dir.display().to_string(),
    }
}

#[tauri::command]
pub fn ladex_suggest_passphrase() -> String {
    auth::generate_passphrase()
}

#[derive(Deserialize)]
pub struct Choices {
    passphrase: String,
    downloads_dir: Option<PathBuf>,
    storage_limit_gib: u64,
    launch_at_login: bool,
}

/// Saves what the wizard chose, starts the node with it and reloads the window onto the running app.
#[tauri::command]
pub async fn ladex_setup_finish(app: AppHandle, choices: Choices) -> Result<(), String> {
    let data_dir = node::default_data_dir();
    if choices.passphrase.is_empty() {
        return Err("choose a passphrase for your mesh".to_string());
    }
    // Set up as soon as the choices are kept: if the node then cannot start, the next launch says why instead of asking again.
    let update = Update {
        is_setup_done: Some(true),
        downloads_dir: choices.downloads_dir,
        storage_limit_gib: Some(choices.storage_limit_gib),
        ..Default::default()
    };
    PreferencesStore::open(&data_dir).update(&update, 0).map_err(|error| error.to_string())?;
    saved_passphrase::save(&data_dir, true, &choices.passphrase).map_err(|error| format!("{error:#}"))?;

    running::start_once(&app).await.map_err(|failure| failure.to_string())?;
    // Launching at login is a favour for later; the node is up, so failing to set it must not undo the setup.
    if choices.launch_at_login {
        if let Err(error) = launch::set_enabled(&app, true) {
            tracing::warn!("Could not turn on launching at login: {error}");
        }
    }
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.eval("location.reload()");
    }
    Ok(())
}
