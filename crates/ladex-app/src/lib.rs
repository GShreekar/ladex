//! The LADEX app: a window whose UI talks to the node running inside this process, on desktop and Android.

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "macos")]
mod cli_link;
mod desktop;
mod ipc;
#[cfg(desktop)]
mod launch;
mod logging;
mod notify;
mod paths;
mod running;
mod setup;
#[cfg(any(target_os = "android", test))]
mod sharing;
#[cfg(desktop)]
mod tray;

use ladex_core::control::server::{self as control, Endpoint};
use ladex_core::control::{Reply as CommandReply, Request as Command};
use ladex_core::node::StartError;
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use crate::paths::AppPaths;
use crate::running::{AppNode, StartFailure};

pub const WINDOW_LABEL: &str = "main";
// Passed by the login item, so the app starts in the tray instead of opening a window.
#[cfg(desktop)]
const BACKGROUND_FLAG: &str = "--background";

// Started at login with a tray to live in, the window waits to be opened.
#[cfg(desktop)]
fn starts_hidden(app: &AppHandle) -> bool {
    std::env::args().any(|argument| argument == BACKGROUND_FLAG) && app.try_state::<tray::Tray>().is_some()
}

#[cfg(mobile)]
fn starts_hidden(_app: &AppHandle) -> bool {
    false
}

pub fn open_window(app: &AppHandle, url: WebviewUrl) -> tauri::Result<()> {
    // Dropped files are handled by the page, which the shell's own drag-and-drop would otherwise hide them from.
    WebviewWindowBuilder::new(app, WINDOW_LABEL, url)
        .title("LADEX")
        .visible(!starts_hidden(app))
        .inner_size(1100.0, 760.0)
        .min_inner_size(420.0, 520.0)
        .disable_drag_drop_handler()
        .build()?;
    Ok(())
}

// The page of the node already running on this data folder, if it serves one to this machine.
fn running_node_page(running: Option<&Endpoint>) -> Option<WebviewUrl> {
    let reply = tauri::async_runtime::block_on(control::call(running?, &Command::Status, |_| {})).ok()?;
    let CommandReply::Status(status) = reply else { return None };
    let url = format!("http://localhost:{}", status.local_http_port?);
    Some(WebviewUrl::External(url.parse().ok()?))
}

// The window attaches to the node that owns the data folder; the app never starts a second one.
fn report_start_failure(app: &AppHandle, failure: StartFailure) -> tauri::Result<()> {
    if let StartFailure::Node(StartError::DataDirInUse { running, .. }) = &failure {
        if let Some(page) = running_node_page(running.as_ref()) {
            tracing::info!("A node already runs on this data folder; opening its page");
            return open_window(app, page);
        }
    }
    tracing::error!("The node could not start: {failure}");
    let message = match &failure {
        StartFailure::Node(StartError::DataDirInUse { .. }) => {
            format!("{failure}.\n\nUse that one, or quit it before opening LADEX again.")
        }
        _ => failure.to_string(),
    };
    report_fatal(app, &message);
    Ok(())
}

/// Tells the person what went wrong and quits once they have read it.
pub fn report_fatal(app: &AppHandle, message: &str) {
    let handle = app.clone();
    app.dialog().message(message).title("LADEX could not start").kind(MessageDialogKind::Error).show(move |_| handle.exit(1));
}

// The first run shows the wizard in an empty window; the node starts once the wizard is done.
fn start(app: &AppHandle) -> tauri::Result<()> {
    if setup::is_needed(&app.state::<AppPaths>().data_dir) {
        return open_window(app, WebviewUrl::default());
    }
    match tauri::async_runtime::block_on(running::start_once(app)) {
        Ok(()) => open_window(app, WebviewUrl::default()),
        Err(failure) => report_start_failure(app, failure),
    }
}

#[cfg(desktop)]
fn with_platform_plugins(builder: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    builder
        // Opening the app again brings the running one forward instead of starting a second window.
        .plugin(tauri_plugin_single_instance::init(|app, _arguments, _working_dir| tray::show_main_window(app)))
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec![BACKGROUND_FLAG])))
        .plugin(tauri_plugin_opener::init())
}

#[cfg(mobile)]
fn with_platform_plugins(builder: tauri::Builder<tauri::Wry>) -> tauri::Builder<tauri::Wry> {
    #[cfg(target_os = "android")]
    let builder = builder.plugin(tauri_plugin_ladex_android::init());
    builder
}

#[cfg(desktop)]
fn set_up_desktop(app: &AppHandle) {
    // Without a tray (some Linux desktops have none) closing the window quits, as it always did.
    if let Err(error) = tray::build(app) {
        tracing::warn!("No tray icon: {error}");
    }
    #[cfg(target_os = "macos")]
    cli_link::ensure_linked(app);
}

/// Builds and runs the app until it quits; Android calls this when the app is opened.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logging::init();

    let app = with_platform_plugins(tauri::Builder::default())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            ipc::ladex_request,
            ipc::ladex_upload_begin,
            ipc::ladex_upload_write,
            ipc::ladex_upload_finish,
            ipc::ladex_upload_abort,
            ipc::ladex_socket_open,
            ipc::ladex_socket_send,
            ipc::ladex_socket_close,
            ipc::ladex_save_download,
            desktop::ladex_is_mobile,
            desktop::ladex_notify,
            desktop::ladex_set_other_devices,
            desktop::ladex_downloads_folder,
            #[cfg(desktop)]
            desktop::ladex_pick_folder,
            #[cfg(desktop)]
            desktop::ladex_launch_at_login,
            #[cfg(desktop)]
            desktop::ladex_set_launch_at_login,
            desktop::ladex_restart_node,
            #[cfg(target_os = "android")]
            android::ladex_device_checks,
            setup::ladex_setup_state,
            setup::ladex_suggest_passphrase,
            setup::ladex_setup_finish,
        ])
        .setup(|app| {
            let paths = AppPaths::resolve(app.handle())?;
            tracing::info!("LADEX {} starting, data folder {}", env!("CARGO_PKG_VERSION"), paths.data_dir.display());
            app.manage(paths);
            #[cfg(desktop)]
            set_up_desktop(app.handle());
            start(app.handle())?;
            #[cfg(target_os = "android")]
            android::start(app.handle());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("could not build the LADEX app");

    app.run(|app, event| match event {
        #[cfg(desktop)]
        RunEvent::WindowEvent { label, event: tauri::WindowEvent::CloseRequested { api, .. }, .. } if label == WINDOW_LABEL => {
            hide_into_tray(app, &api)
        }
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => tray::show_main_window(app),
        RunEvent::ExitRequested { code, api, .. } => on_exit_requested(app, code, &api),
        _ => {}
    });
}

// Closing the window (no code) on Android keeps the node while it shares outside the app; anything else stops it.
#[cfg_attr(not(target_os = "android"), allow(unused_variables))]
fn on_exit_requested(app: &AppHandle, code: Option<i32>, api: &tauri::ExitRequestApi) {
    #[cfg(target_os = "android")]
    if code.is_none() && android::keep_running_without_window(app) {
        tracing::info!("Android closed the window; LADEX keeps sharing in the background");
        api.prevent_exit();
        return;
    }
    shut_down_node(app, api);
}

// The node keeps sharing while the window is closed; quitting is done from the tray.
#[cfg(desktop)]
fn hide_into_tray(app: &AppHandle, close: &tauri::CloseRequestApi) {
    let Some(tray) = app.try_state::<tray::Tray>() else { return };
    close.prevent_close();
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        let _ = window.hide();
    }
    if tray.should_explain_closing() {
        notify::send(app, "LADEX is still running", "Sharing continues from the tray. Choose Quit LADEX there to stop.");
    }
}

fn shut_down_node(app: &AppHandle, api: &tauri::ExitRequestApi) {
    let Some(node) = app.try_state::<AppNode>().and_then(|node| node.take_handle()) else { return };
    // Peers are told goodbye and state is saved before the process ends.
    api.prevent_exit();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        node.shutdown().await;
        app.exit(0);
    });
}
