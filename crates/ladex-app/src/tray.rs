//! The tray icon (menu bar on macOS) and its menu: where the app stays while its window is closed.

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_opener::OpenerExt as _;

use crate::{downloads_folder, WINDOW_LABEL};

const OPEN_ID: &str = "open";
const DOWNLOADS_ID: &str = "downloads";
const AUTOSTART_ID: &str = "autostart";
const QUIT_ID: &str = "quit";

/// Present only when the tray icon was created, which is what lets the window close into it.
pub struct Tray {
    status: MenuItem<Wry>,
    has_explained_closing: AtomicBool,
}

impl Tray {
    /// True the first time it is asked, so the person is told once that closing the window keeps LADEX running.
    pub fn should_explain_closing(&self) -> bool {
        !self.has_explained_closing.swap(true, Ordering::Relaxed)
    }
}

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", status_text(None), false, None::<&str>)?;
    let open = MenuItem::with_id(app, OPEN_ID, "Open LADEX", true, None::<&str>)?;
    let downloads = MenuItem::with_id(app, DOWNLOADS_ID, "Open Downloads Folder", true, None::<&str>)?;
    let is_autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
    let autostart = CheckMenuItem::with_id(app, AUTOSTART_ID, "Launch at Login", true, is_autostart_on, None::<&str>)?;
    let quit = MenuItem::with_id(app, QUIT_ID, "Quit LADEX", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&status, &PredefinedMenuItem::separator(app)?, &open, &downloads, &autostart, &PredefinedMenuItem::separator(app)?, &quit],
    )?;

    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("LADEX")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            OPEN_ID => show_main_window(app),
            DOWNLOADS_ID => open_downloads_folder(app),
            AUTOSTART_ID => toggle_autostart(app, &autostart),
            QUIT_ID => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;

    app.manage(Tray { status, has_explained_closing: AtomicBool::new(false) });
    Ok(())
}

pub fn show_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window(WINDOW_LABEL) else { return };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}

/// Shows how many other devices are connected, in the tray menu and as the icon's tooltip.
pub fn set_other_devices(app: &AppHandle, count: usize) {
    let Some(tray) = app.try_state::<Tray>() else { return };
    let text = status_text(Some(count));
    let _ = tray.status.set_text(&text);
    if let Some(icon) = app.tray_by_id("main") {
        let _ = icon.set_tooltip(Some(format!("LADEX: {text}")));
    }
}

fn status_text(other_devices: Option<usize>) -> String {
    match other_devices {
        None => "Starting…".to_string(),
        Some(0) => "No other devices yet".to_string(),
        Some(1) => "Connected to 1 device".to_string(),
        Some(count) => format!("Connected to {count} devices"),
    }
}

fn open_downloads_folder(app: &AppHandle) {
    let Some(folder) = downloads_folder(app) else { return };
    if let Err(error) = app.opener().open_path(folder.display().to_string(), None::<&str>) {
        tracing::warn!("Could not open the downloads folder: {error}");
    }
}

fn toggle_autostart(app: &AppHandle, item: &CheckMenuItem<Wry>) {
    let autolaunch = app.autolaunch();
    let result = match autolaunch.is_enabled() {
        Ok(true) => autolaunch.disable(),
        _ => autolaunch.enable(),
    };
    if let Err(error) = result {
        tracing::warn!("Could not change launching at login: {error}");
    }
    // The system has the last word, so the tick shows what is really set.
    let _ = item.set_checked(autolaunch.is_enabled().unwrap_or(false));
}

#[cfg(test)]
mod tests {
    use super::status_text;

    #[test]
    fn the_status_is_starting_until_the_devices_are_known() {
        assert_eq!(status_text(None), "Starting…");
    }

    #[test]
    fn the_status_counts_devices_in_the_singular_and_plural() {
        assert_eq!(status_text(Some(0)), "No other devices yet");
        assert_eq!(status_text(Some(1)), "Connected to 1 device");
        assert_eq!(status_text(Some(4)), "Connected to 4 devices");
    }
}
