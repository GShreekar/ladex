//! Native notifications, sent only when nobody is looking at the window.

use tauri::{AppHandle, Manager};
use tauri_plugin_notification::{NotificationExt as _, PermissionState};

use crate::WINDOW_LABEL;

pub fn is_window_in_use(app: &AppHandle) -> bool {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else { return false };
    window.is_visible().unwrap_or(false) && window.is_focused().unwrap_or(false) && !window.is_minimized().unwrap_or(false)
}

pub fn send(app: &AppHandle, title: &str, body: &str) {
    let notification = app.notification();
    // macOS and Windows ask once; a refusal is the person's choice and not worth reporting.
    if !matches!(notification.permission_state(), Ok(PermissionState::Granted))
        && !matches!(notification.request_permission(), Ok(PermissionState::Granted))
    {
        return;
    }
    if let Err(error) = notification.builder().title(title).body(body).show() {
        tracing::warn!("Could not show a notification: {error}");
    }
}
