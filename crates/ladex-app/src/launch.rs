//! Whether the app starts when the person logs in.

use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt as _;

use crate::tray;

pub fn is_enabled(app: &AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Turns launching at login on or off; returns what the system says is set afterwards.
pub fn set_enabled(app: &AppHandle, should_launch: bool) -> Result<bool, String> {
    let autolaunch = app.autolaunch();
    let result = if should_launch { autolaunch.enable() } else { autolaunch.disable() };
    result.map_err(|error| error.to_string())?;
    let is_enabled = is_enabled(app);
    tray::show_launch_at_login(app, is_enabled);
    Ok(is_enabled)
}
