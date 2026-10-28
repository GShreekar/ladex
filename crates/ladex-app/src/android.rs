//! Android: the multicast lock, the sharing notification, pausing off Wi-Fi, the node outliving its window, and passing
//! on what Kotlin reports about files shared into the app and the save folder.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use ladex_core::doctor::Check;
use tauri::{AppHandle, Manager, WebviewUrl};
use tauri_plugin_ladex_android::LadexAndroidExt;
use tokio::sync::mpsc;

use crate::phone_files::{self, PhoneFiles};
use crate::running::AppNode;
use crate::sharing::{self, DeviceEvent, NetworkReport, PauseClock, Situation, UploadWatch, Wanted};
use crate::{open_window, WINDOW_LABEL};

const TICK: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Seen {
    network: Option<NetworkReport>,
    is_paused: bool,
    is_sharing: bool,
    // Android closed the window and the node was kept running to share.
    is_windowless: bool,
}

/// What the phone last reported and was last asked, for the Network page and for deciding what happens when the window closes.
#[derive(Default)]
pub struct Phone(Mutex<Seen>);

/// Starts listening to Android and keeping the phone in step with the node; call once, after the window has opened.
pub fn start(app: &AppHandle) {
    app.manage(Phone::default());
    app.manage(PhoneFiles::default());
    let (sender, events) = mpsc::unbounded_channel();
    if let Err(error) = app.ladex_android().watch(move |event: DeviceEvent| {
        let _ = sender.send(event);
    }) {
        tracing::error!("Android will not report the network: {error}");
    }
    match app.get_webview_window(WINDOW_LABEL) {
        Some(window) => {
            if let Err(error) = app.ladex_android().connect(&window) {
                tracing::error!("Could not reach LADEX's Android code: {error}");
            }
        }
        None => tracing::error!("No window to reach LADEX's Android code through"),
    }
    tauri::async_runtime::spawn(keep_in_step(app.clone(), events));
}

/// Whether the node keeps running now that Android has closed the window: it does while it shares outside the app.
pub fn keep_running_without_window(app: &AppHandle) -> bool {
    let Some(phone) = app.try_state::<Phone>() else { return false };
    let mut seen = phone.0.lock().unwrap();
    seen.is_windowless = seen.is_sharing;
    seen.is_windowless
}

// Android closed the window while sharing went on, and the app is on screen again in a new activity.
fn reopen_window(app: &AppHandle) {
    let is_windowless = app.try_state::<Phone>().is_some_and(|phone| phone.0.lock().unwrap().is_windowless);
    if !is_windowless || app.get_webview_window(WINDOW_LABEL).is_some() {
        return;
    }
    let handle = app.clone();
    let opened = app.run_on_main_thread(move || match open_window(&handle, WebviewUrl::default()) {
        Ok(()) => handle.state::<Phone>().0.lock().unwrap().is_windowless = false,
        Err(error) => tracing::error!("Could not open the window again: {error}"),
    });
    if let Err(error) = opened {
        tracing::error!("Could not open the window again: {error}");
    }
}

#[tauri::command]
pub fn ladex_device_checks(app: AppHandle) -> Vec<Check> {
    let Some(phone) = app.try_state::<Phone>() else { return Vec::new() };
    let seen = phone.0.lock().unwrap();
    sharing::checks(seen.network.as_ref(), seen.is_paused)
}

async fn keep_in_step(app: AppHandle, mut events: mpsc::UnboundedReceiver<DeviceEvent>) {
    let mut pause = PauseClock::default();
    let mut uploads = UploadWatch::default();
    let mut delivered: Option<Wanted> = None;
    let mut tick = tokio::time::interval(TICK);
    loop {
        tokio::select! {
            Some(event) = events.recv() => match event {
                DeviceEvent::Stop => return quit(&app, "the notification's Stop button").await,
                DeviceEvent::Shown => reopen_window(&app),
                DeviceEvent::Incoming { files } => phone_files::incoming_changed(&app, files),
                DeviceEvent::SaveFolder { name } => phone_files::folder_picked(&app, name),
                DeviceEvent::Network(report) => {
                    app.state::<Phone>().0.lock().unwrap().network = Some(report);
                    if pause.report(&report, Instant::now()) {
                        tracing::info!("Back on a local network");
                    }
                }
            },
            _ = tick.tick() => {}
        }

        if is_node_running(&app) && pause.tick(Instant::now()) {
            tracing::info!("Off Wi-Fi for {} seconds", sharing::PAUSE_GRACE.as_secs());
        }
        // Android can report the network back before the device has its address there, so the address is kept fresh.
        if let Some(node) = app.try_state::<AppNode>() {
            node.keep_seeding_in_step(pause.is_paused());
        }

        let wanted = sharing::wanted(&situation(&app, pause.is_paused(), &mut uploads).await);
        let phone = app.state::<Phone>();
        let is_left_with_nothing_to_share = {
            let mut seen = phone.0.lock().unwrap();
            seen.is_paused = pause.is_paused();
            seen.is_sharing = wanted.sharing.is_some();
            seen.is_windowless && !seen.is_sharing
        };
        if delivered.as_ref() != Some(&wanted) && app.ladex_android().apply(&wanted) {
            delivered = Some(wanted);
        }
        if is_left_with_nothing_to_share {
            return quit(&app, "the end of sharing with the app closed").await;
        }
    }
}

fn is_node_running(app: &AppHandle) -> bool {
    app.try_state::<AppNode>().is_some_and(|node| node.state().is_some())
}

async fn situation(app: &AppHandle, is_paused: bool, uploads: &mut UploadWatch) -> Situation {
    let Some(state) = app.try_state::<AppNode>().and_then(|node| node.state()) else { return Situation::default() };
    let other_devices = state.mesh_peers.read().await.len();
    Situation {
        is_node_running: true,
        keeps_sharing_in_background: state.preferences.get().keep_sharing_in_background,
        is_paused,
        other_devices,
        downloads: state.transfers.active(),
        is_uploading: uploads.is_uploading(state.transfers.uploads().in_use(), Instant::now()),
    }
}

// The other devices are told goodbye before the process ends, which also ends the service and its notification.
// Whoever takes the node shuts it down and ends the app, so a quit already under way is left to finish.
async fn quit(app: &AppHandle, reason: &str) {
    let Some(node) = app.try_state::<AppNode>().and_then(|node| node.take_handle()) else { return };
    tracing::info!("Quitting at {reason}");
    node.shutdown().await;
    app.ladex_android().apply(&sharing::wanted(&Situation::default()));
    app.exit(0);
}
