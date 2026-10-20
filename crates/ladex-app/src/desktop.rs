//! What the window asks of the device around it: notifications, the tray, folders, login item and restarts.

use std::path::PathBuf;

use tauri::{AppHandle, Manager};
#[cfg(desktop)]
use tauri_plugin_dialog::DialogExt;

use crate::paths::AppPaths;
use crate::running::{self, AppNode};
#[cfg(desktop)]
use crate::{launch, tray};
use crate::{notify, report_fatal};

/// Whether the window is on a phone, where there is no tray, login item, folder picker or command line.
#[tauri::command]
pub fn ladex_is_mobile() -> bool {
    cfg!(mobile)
}

#[tauri::command]
pub fn ladex_notify(app: AppHandle, title: String, body: String) {
    if !notify::is_window_in_use(&app) {
        notify::send(&app, &title, &body);
    }
}

// Phones have no tray to show the count in.
#[cfg_attr(mobile, allow(unused_variables))]
#[tauri::command]
pub fn ladex_set_other_devices(app: AppHandle, count: usize) {
    #[cfg(desktop)]
    tray::set_other_devices(&app, count);
}

/// Where saved files go now: the folder the person chose, else the platform's default.
pub fn downloads_folder(app: &AppHandle) -> Option<PathBuf> {
    match app.try_state::<AppNode>() {
        Some(node) => Some(node.bridge().downloads_folder()),
        None => app.state::<AppPaths>().default_downloads.clone(),
    }
}

#[tauri::command]
pub fn ladex_downloads_folder(app: AppHandle) -> Option<String> {
    downloads_folder(&app).map(|folder| folder.display().to_string())
}

#[cfg(desktop)]
#[tauri::command]
pub async fn ladex_pick_folder(app: AppHandle, title: String) -> Option<String> {
    let dialog = app.dialog().file().set_title(title);
    let picked = tauri::async_runtime::spawn_blocking(move || dialog.blocking_pick_folder()).await.ok()??;
    picked.into_path().ok().map(|path| path.display().to_string())
}

#[cfg(desktop)]
#[tauri::command]
pub fn ladex_launch_at_login(app: AppHandle) -> bool {
    launch::is_enabled(&app)
}

#[cfg(desktop)]
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
