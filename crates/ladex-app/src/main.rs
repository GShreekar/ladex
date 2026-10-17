//! The LADEX desktop app: a window whose UI talks to the node running inside this process.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod notify;
mod tray;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use include_dir::{include_dir, Dir};
use ladex_core::control::server::{self as control, Endpoint};
use ladex_core::control::{Reply as CommandReply, Request as Command};
use ladex_core::node::{self, Bridge, Node, NodeConfig, NodeHandle, Reply, SocketEvent, StartError, UiAssets, WebUi};
use tauri::ipc::{Channel, InvokeBody, Request};
use tauri::{AppHandle, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

// The web UI built from `ui/`; the node serves it to browsers on the network, the window shows it itself.
static UI_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../ui/dist");

pub const WINDOW_LABEL: &str = "main";
// Passed by the login item, so the app starts in the tray instead of opening a window.
const BACKGROUND_FLAG: &str = "--background";
const UPLOAD_ID_HEADER: &str = "x-upload-id";

struct EmbeddedUi(&'static Dir<'static>);

impl UiAssets for EmbeddedUi {
    fn file(&self, path: &str) -> Option<&[u8]> {
        self.0.get_file(path).map(|file| file.contents())
    }
}

struct AppNode {
    bridge: Bridge,
    downloads: PathBuf,
    handle: Mutex<Option<NodeHandle>>,
}

impl AppNode {
    fn take_handle(&self) -> Option<NodeHandle> {
        self.handle.lock().unwrap().take()
    }
}

#[tauri::command]
async fn ladex_request(
    node: State<'_, AppNode>,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
) -> Result<Reply, String> {
    node.bridge.request(&method, &path, &headers, body).await
}

#[tauri::command]
fn ladex_upload_begin(node: State<'_, AppNode>, path: String, headers: Vec<(String, String)>, content_length: u64) -> u64 {
    node.bridge.begin_upload(&path, &headers, content_length)
}

#[tauri::command]
async fn ladex_upload_write(node: State<'_, AppNode>, request: Request<'_>) -> Result<(), String> {
    let InvokeBody::Raw(piece) = request.body() else {
        return Err("an upload piece must be sent as raw bytes".to_string());
    };
    let upload_id = request
        .headers()
        .get(UPLOAD_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("missing {UPLOAD_ID_HEADER} header"))?;
    node.bridge.write_upload(upload_id, piece.clone()).await
}

#[tauri::command]
async fn ladex_upload_finish(node: State<'_, AppNode>, upload_id: u64) -> Result<Reply, String> {
    node.bridge.finish_upload(upload_id).await
}

#[tauri::command]
fn ladex_upload_abort(node: State<'_, AppNode>, upload_id: u64) {
    node.bridge.abort_upload(upload_id);
}

#[tauri::command]
async fn ladex_socket_open(node: State<'_, AppNode>, on_event: Channel<SocketEvent>) -> Result<u64, String> {
    node.bridge
        .open_socket(move |event| {
            // A window that has gone away has nobody left to tell.
            let _ = on_event.send(event);
        })
        .await
}

#[tauri::command]
async fn ladex_socket_send(node: State<'_, AppNode>, socket_id: u64, text: String) -> Result<(), String> {
    node.bridge.send_to_socket(socket_id, text).await
}

#[tauri::command]
async fn ladex_socket_close(node: State<'_, AppNode>, socket_id: u64) -> Result<(), String> {
    node.bridge.close_socket(socket_id).await;
    Ok(())
}

#[tauri::command]
async fn ladex_save_download(node: State<'_, AppNode>, path: String, file_name: String) -> Result<String, String> {
    node.bridge.save_download(&path, &file_name).await.map_err(|error| error.to_string())
}

#[tauri::command]
fn ladex_notify(app: AppHandle, title: String, body: String) {
    if !notify::is_window_in_use(&app) {
        notify::send(&app, &title, &body);
    }
}

#[tauri::command]
fn ladex_set_other_devices(app: AppHandle, count: usize) {
    tray::set_other_devices(&app, count);
}

// Where saved files go: the folder the running node was given, else the system downloads folder.
pub fn downloads_folder(app: &AppHandle) -> Option<PathBuf> {
    match app.try_state::<AppNode>() {
        Some(node) => Some(node.downloads.clone()),
        None => app.path().download_dir().ok(),
    }
}

// Until the first-run settings of Phase 7b, the passphrase and data folder come from the environment (the data folder as for `ladex`).
fn node_config() -> NodeConfig {
    let data_dir = node::default_data_dir();
    let assets: Arc<dyn UiAssets> = Arc::new(EmbeddedUi(&UI_DIR));
    let mut config = NodeConfig::new(data_dir, WebUi { assets, is_legacy: false });
    config.passphrase = std::env::var("LADEX_PASSPHRASE").ok().filter(|passphrase| !passphrase.is_empty());
    config
}

fn open_window(app: &AppHandle, url: WebviewUrl) -> tauri::Result<()> {
    // Started at login with a tray to live in, the window waits to be opened.
    let is_hidden = std::env::args().any(|argument| argument == BACKGROUND_FLAG) && app.try_state::<tray::Tray>().is_some();
    // Dropped files are handled by the page, which the shell's own drag-and-drop would otherwise hide them from.
    WebviewWindowBuilder::new(app, WINDOW_LABEL, url)
        .title("LADEX")
        .visible(!is_hidden)
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
fn report_start_failure(app: &tauri::App, error: StartError) -> tauri::Result<()> {
    if let StartError::DataDirInUse { running, .. } = &error {
        if let Some(page) = running_node_page(running.as_ref()) {
            tracing::info!("A node already runs on this data folder; opening its page");
            return open_window(app.handle(), page);
        }
    }
    tracing::error!("The node could not start: {error}");
    let message = match &error {
        StartError::DataDirInUse { .. } => format!("{error}.\n\nUse that one, or quit it before opening LADEX again."),
        _ => error.to_string(),
    };
    let handle = app.handle().clone();
    app.dialog().message(message).title("LADEX could not start").kind(MessageDialogKind::Error).show(move |_| handle.exit(1));
    Ok(())
}

fn run_node(app: &tauri::App) -> tauri::Result<()> {
    let node = match tauri::async_runtime::block_on(Node::start(node_config())) {
        Ok(node) => node,
        Err(error) => return report_start_failure(app, error),
    };
    for notice in &node.info().notices {
        tracing::warn!("{notice:?}");
    }
    let downloads = app.path().download_dir().unwrap_or_else(|_| node.info().data_dir.join("received"));
    let bridge = Bridge::new(node.local_link(), downloads.clone());
    app.manage(AppNode { bridge, downloads, handle: Mutex::new(Some(node)) });
    open_window(app.handle(), WebviewUrl::default())
}

fn main() {
    tracing_subscriber::fmt::init();

    let app = tauri::Builder::default()
        // Opening the app again brings the running one forward instead of starting a second window.
        .plugin(tauri_plugin_single_instance::init(|app, _arguments, _working_dir| tray::show_main_window(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![BACKGROUND_FLAG])))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            ladex_request,
            ladex_upload_begin,
            ladex_upload_write,
            ladex_upload_finish,
            ladex_upload_abort,
            ladex_socket_open,
            ladex_socket_send,
            ladex_socket_close,
            ladex_save_download,
            ladex_notify,
            ladex_set_other_devices,
        ])
        .setup(|app| {
            // Without a tray (some Linux desktops have none) closing the window quits, as it always did.
            if let Err(error) = tray::build(app.handle()) {
                tracing::warn!("No tray icon: {error}");
            }
            Ok(run_node(app)?)
        })
        .build(tauri::generate_context!())
        .expect("could not build the LADEX app");

    app.run(|app, event| match event {
        RunEvent::WindowEvent { label, event: WindowEvent::CloseRequested { api, .. }, .. } if label == WINDOW_LABEL => {
            hide_into_tray(app, &api)
        }
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => tray::show_main_window(app),
        RunEvent::ExitRequested { api, .. } => shut_down_node(app, &api),
        _ => {}
    });
}

// The node keeps sharing while the window is closed; quitting is done from the tray.
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
