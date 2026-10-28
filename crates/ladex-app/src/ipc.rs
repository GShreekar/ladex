//! The commands the window uses to reach its node, in place of HTTP: thin, since `Bridge` does the work.

use ladex_core::node::{Reply, SocketEvent};
use serde::Serialize;
use tauri::ipc::{Channel, InvokeBody, Request};
use tauri::State;

use crate::running::AppNode;

const UPLOAD_ID_HEADER: &str = "x-upload-id";

#[tauri::command]
pub async fn ladex_request(
    node: State<'_, AppNode>,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
) -> Result<Reply, String> {
    node.bridge().request(&method, &path, &headers, body).await
}

#[tauri::command]
pub fn ladex_upload_begin(node: State<'_, AppNode>, path: String, headers: Vec<(String, String)>, content_length: u64) -> u64 {
    node.bridge().begin_upload(&path, &headers, content_length)
}

#[tauri::command]
pub async fn ladex_upload_write(node: State<'_, AppNode>, request: Request<'_>) -> Result<(), String> {
    let InvokeBody::Raw(piece) = request.body() else {
        return Err("an upload piece must be sent as raw bytes".to_string());
    };
    let upload_id = request
        .headers()
        .get(UPLOAD_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("missing {UPLOAD_ID_HEADER} header"))?;
    node.bridge().write_upload(upload_id, piece.clone()).await
}

#[tauri::command]
pub async fn ladex_upload_finish(node: State<'_, AppNode>, upload_id: u64) -> Result<Reply, String> {
    node.bridge().finish_upload(upload_id).await
}

#[tauri::command]
pub fn ladex_upload_abort(node: State<'_, AppNode>, upload_id: u64) {
    node.bridge().abort_upload(upload_id);
}

#[tauri::command]
pub async fn ladex_socket_open(node: State<'_, AppNode>, on_event: Channel<SocketEvent>) -> Result<u64, String> {
    node.bridge()
        .open_socket(move |event| {
            // A window that has gone away has nobody left to tell.
            let _ = on_event.send(event);
        })
        .await
}

#[tauri::command]
pub async fn ladex_socket_send(node: State<'_, AppNode>, socket_id: u64, text: String) -> Result<(), String> {
    node.bridge().send_to_socket(socket_id, text).await
}

#[tauri::command]
pub async fn ladex_socket_close(node: State<'_, AppNode>, socket_id: u64) -> Result<(), String> {
    node.bridge().close_socket(socket_id).await;
    Ok(())
}

/// Where a saved file went, as the window shows it; on a phone, also the id it is opened or shared onward by.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedDownload {
    pub shown_as: String,
    pub saved_id: Option<usize>,
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
pub async fn ladex_save_download(node: State<'_, AppNode>, path: String, file_name: String) -> Result<SavedDownload, String> {
    let saved = node.bridge().save_download(&path, &file_name).await.map_err(|error| error.to_string())?;
    Ok(SavedDownload { shown_as: saved, saved_id: None })
}

// On a phone the file goes into the folder the person chose through Android, not into a path.
#[cfg(target_os = "android")]
#[tauri::command]
pub async fn ladex_save_download(app: tauri::AppHandle, path: String, file_name: String) -> Result<SavedDownload, String> {
    crate::phone_files::save_download(&app, &path, &file_name).await
}
