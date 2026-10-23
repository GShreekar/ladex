//! Android: files shared in from other apps, the folder received files are saved in, and handing saved files to other apps.
//! The questions and answers match `DeviceBridge.kt` in the Android plugin.

use std::fs::File;
use std::os::fd::{FromRawFd, RawFd};
use std::sync::Mutex;

use ladex_core::control::{self, Event, Events, OpenedFile};
use ladex_core::node::Bridge;
use ladex_core::{validate, NodeState};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_ladex_android::LadexAndroidExt;
use tokio::sync::{mpsc, oneshot};

use crate::ipc::SavedDownload;
use crate::running::AppNode;
use crate::sharing::IncomingFile;

/// Tells the window that the files shared into the app changed.
const INCOMING_EVENT: &str = "ladex-incoming-files";
const PROGRESS_BACKLOG: usize = 16;
const OWN_STORAGE: &str = "LADEX's own storage. Choose a folder under Settings › Receiving files to find received files in the Files app";

/// What the phone keeps between the window's commands.
#[derive(Default)]
pub struct PhoneFiles {
    incoming: Mutex<Vec<IncomingFile>>,
    // Where each file this run saved went: a document in the save folder, or a path in the app's own storage.
    // The window names them by their place in this list, so it can only hand over files LADEX saved.
    saved: Mutex<Vec<String>>,
    folder_pick: Mutex<Option<oneshot::Sender<Option<String>>>>,
}

#[derive(Serialize)]
struct Nothing {}

#[derive(Serialize)]
struct ById {
    id: String,
}

#[derive(Serialize)]
struct Named {
    name: String,
}

#[derive(Serialize)]
struct AtUri {
    uri: String,
}

#[derive(Serialize)]
struct At {
    #[serde(rename = "where")]
    place: String,
}

#[derive(Deserialize)]
struct Done {}

#[derive(Deserialize)]
struct Opened {
    descriptor: RawFd,
    size: u64,
}

#[derive(Deserialize)]
struct FolderName {
    name: Option<String>,
}

#[derive(Deserialize)]
struct Created {
    document: Option<Document>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    uri: String,
    descriptor: RawFd,
    shown_as: String,
}

#[derive(Clone, Serialize)]
pub struct ShareProgress {
    done: u64,
    total: u64,
}

// Kotlin asks Android's providers, which can block, so the question is put on a thread that may wait.
async fn ask<T: DeserializeOwned + Send + 'static>(
    app: &AppHandle,
    method: &'static str,
    question: impl Serialize + Send + 'static,
) -> Result<T, String> {
    let app = app.clone();
    let answer = tauri::async_runtime::spawn_blocking(move || app.ladex_android().ask(method, &question)).await;
    answer.map_err(|error| error.to_string())?.map_err(|error| error.to_string())
}

// Kotlin detached this descriptor from its ParcelFileDescriptor, so it is this side's alone to close.
fn take_descriptor(descriptor: RawFd) -> File {
    unsafe { File::from_raw_fd(descriptor) }
}

fn node_state(app: &AppHandle) -> Result<NodeState, String> {
    app.try_state::<AppNode>().and_then(|node| node.state()).ok_or_else(|| "LADEX has not started yet".to_string())
}

fn bridge(app: &AppHandle) -> Result<std::sync::Arc<Bridge>, String> {
    app.try_state::<AppNode>().map(|node| node.bridge()).ok_or_else(|| "LADEX has not started yet".to_string())
}

/// Kotlin reported the files shared into the app now; the window is told, so it can offer them.
pub fn incoming_changed(app: &AppHandle, files: Vec<IncomingFile>) {
    *app.state::<PhoneFiles>().incoming.lock().unwrap() = files.clone();
    if let Err(error) = app.emit(INCOMING_EVENT, files) {
        tracing::warn!("Could not tell the window about files shared into LADEX: {error}");
    }
}

/// The folder picker closed; the command waiting for it gets the folder's name.
pub fn folder_picked(app: &AppHandle, name: Option<String>) {
    if let Some(waiting) = app.state::<PhoneFiles>().folder_pick.lock().unwrap().take() {
        let _ = waiting.send(name);
    }
}

#[tauri::command]
pub fn ladex_incoming_files(app: AppHandle) -> Vec<IncomingFile> {
    app.state::<PhoneFiles>().incoming.lock().unwrap().clone()
}

/// Shares the files shared into the app with everyone; returns how many were shared. They are set aside either way,
/// since those before a failure are already shared.
#[tauri::command]
pub async fn ladex_share_incoming(app: AppHandle, on_progress: Channel<ShareProgress>) -> Result<usize, String> {
    let state = node_state(&app)?;
    let files = app.state::<PhoneFiles>().incoming.lock().unwrap().clone();
    let shared = share(&app, &state, files, on_progress).await;
    set_incoming_aside(&app).await;
    shared
}

#[tauri::command]
pub async fn ladex_set_incoming_aside(app: AppHandle) {
    set_incoming_aside(&app).await;
}

async fn set_incoming_aside(app: &AppHandle) {
    if let Err(error) = ask::<Done>(app, "forgetIncoming", Nothing {}).await {
        tracing::warn!("Android did not set the shared files aside: {error}");
    }
    incoming_changed(app, Vec::new());
}

async fn share(app: &AppHandle, state: &NodeState, files: Vec<IncomingFile>, on_progress: Channel<ShareProgress>) -> Result<usize, String> {
    let mut opened = Vec::new();
    for file in files {
        opened.push(open_incoming(app, file).await?);
    }
    let (sender, mut progress) = mpsc::channel(PROGRESS_BACKLOG);
    tauri::async_runtime::spawn(async move {
        while let Some(event) = progress.recv().await {
            if let Event::Progress { done, total, .. } = event {
                let _ = on_progress.send(ShareProgress { done, total });
            }
        }
    });
    let shared = control::share_opened(state, opened, &Events::new(sender)).await.map_err(|error| format!("{error:#}"))?;
    Ok(shared.len())
}

async fn open_incoming(app: &AppHandle, file: IncomingFile) -> Result<OpenedFile, String> {
    let opened: Opened = ask(app, "openIncoming", ById { id: file.id }).await?;
    Ok(OpenedFile { file: take_descriptor(opened.descriptor), name: file.name, mime_type: file.mime_type, size: opened.size })
}

#[tauri::command]
pub async fn ladex_save_folder(app: AppHandle) -> Result<Option<String>, String> {
    Ok(ask::<FolderName>(&app, "saveFolder", Nothing {}).await?.name)
}

/// Shows Android's folder picker; returns the folder now chosen, which is the old one if the person picked none.
#[tauri::command]
pub async fn ladex_pick_save_folder(app: AppHandle) -> Result<Option<String>, String> {
    let (sender, picked) = oneshot::channel();
    *app.state::<PhoneFiles>().folder_pick.lock().unwrap() = Some(sender);
    ask::<Done>(&app, "pickSaveFolder", Nothing {}).await?;
    picked.await.map_err(|_| "the folder picker was opened again".to_string())
}

#[tauri::command]
pub async fn ladex_forget_save_folder(app: AppHandle) -> Result<(), String> {
    ask::<Done>(&app, "forgetSaveFolder", Nothing {}).await.map(|_| ())
}

/// Saves a download into the folder the person chose, through Android, or into the app's own storage if there is none.
pub async fn save_download(app: &AppHandle, path: &str, file_name: &str) -> Result<SavedDownload, String> {
    let bridge = bridge(app)?;
    let name = validate::sanitize_file_name(file_name);
    let created: Created = ask(app, "createInSaveFolder", Named { name }).await?;
    let (place, shown_as) = match created.document {
        Some(document) => {
            write_into(app, &bridge, path, &document).await?;
            (document.uri, document.shown_as)
        }
        None => (bridge.save_download(path, file_name).await.map_err(|error| error.to_string())?, OWN_STORAGE.to_string()),
    };
    let phone_files = app.state::<PhoneFiles>();
    let mut saved = phone_files.saved.lock().unwrap();
    saved.push(place);
    Ok(SavedDownload { shown_as, saved_id: Some(saved.len() - 1) })
}

// The download streams from the node straight into the new document; the store's own copy stays in private storage.
async fn write_into(app: &AppHandle, bridge: &Bridge, path: &str, document: &Document) -> Result<(), String> {
    let mut file = tokio::fs::File::from_std(take_descriptor(document.descriptor));
    let written = match bridge.start_download(path).await {
        Ok(download) => download.write_to(&mut file, |_| {}).await,
        Err(refused) => Err(refused),
    };
    drop(file);
    let Err(error) = written else { return Ok(()) };
    // An unfinished file is not left in the person's folder.
    if let Err(not_removed) = ask::<Done>(app, "deleteDocument", AtUri { uri: document.uri.clone() }).await {
        tracing::warn!("Could not remove the unfinished {}: {not_removed}", document.shown_as);
    }
    Err(error.to_string())
}

#[tauri::command]
pub async fn ladex_open_saved(app: AppHandle, saved_id: usize) -> Result<(), String> {
    hand_over(&app, "openReceived", saved_id).await
}

#[tauri::command]
pub async fn ladex_share_saved(app: AppHandle, saved_id: usize) -> Result<(), String> {
    hand_over(&app, "shareReceived", saved_id).await
}

async fn hand_over(app: &AppHandle, method: &'static str, saved_id: usize) -> Result<(), String> {
    let place = app.state::<PhoneFiles>().saved.lock().unwrap().get(saved_id).cloned();
    let place = place.ok_or_else(|| "LADEX has no record of saving that file".to_string())?;
    ask::<Done>(app, method, At { place }).await.map(|_| ())
}
