//! What the window asks of the desktop around it: notifications, the tray, folders, login item and restarts.

use std::path::PathBuf;

use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

use crate::running::{self, AppNode};
use crate::{launch, notify, report_fatal, tray};

#[tauri::command]
pub fn ladex_notify(app: AppHandle, title: String, body: String) {
    if !notify::is_window_in_use(&app) {
        notify::send(&app, &title, &body);
    }
}

#[tauri::command]
pub fn ladex_set_other_devices(app: AppHandle, count: usize) {
    tray::set_other_devices(&app, count);
}

/// Where saved files go now: the folder the person chose, else the system's downloads folder.
pub fn downloads_folder(app: &AppHandle) -> Option<PathBuf> {
    match app.try_state::<AppNode>() {
        Some(node) => Some(node.bridge().downloads_folder()),
        None => app.path().download_dir().ok(),
    }
}

#[tauri::command]
pub fn ladex_downloads_folder(app: AppHandle) -> Option<String> {
    downloads_folder(&app).map(|folder| folder.display().to_string())
}

#[tauri::command]
pub async fn ladex_pick_folder(app: AppHandle, title: String) -> Option<String> {
    let dialog = app.dialog().file().set_title(title);
    let picked = tauri::async_runtime::spawn_blocking(move || dialog.blocking_pick_folder()).await.ok()??;
    picked.into_path().ok().map(|path| path.display().to_string())
}

#[tauri::command]
pub fn ladex_launch_at_login(app: AppHandle) -> bool {
    launch::is_enabled(&app)
}

#[tauri::command]
pub fn ladex_set_launch_at_login(app: AppHandle, should_launch: bool) -> Result<bool, String> {
    launch::set_enabled(&app, should_launch)
}

#[tauri::command]
pub async fn ladex_restart_node(app: AppHandle) -> Result<(), String> {
    if let Err(failure) = running::restart(&app).await {
        // The old node is already stopped, so there is nothing to go back to.
        report_fatal(&app, &failure.to_string());
        return Err(failure.to_string());
    }
    if let Some(window) = app.get_webview_window(crate::WINDOW_LABEL) {
        let _ = window.eval("location.reload()");
    }
    Ok(())
}
