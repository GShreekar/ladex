//! The stateful half of the app's commands: uploads and sockets the window opens are kept here under ids it can hold on to.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use hyper::Method;
use serde::Serialize;
use tokio::task::JoinHandle;

use crate::preferences::PreferencesStore;

use super::local::{LinkError, LocalLink, LocalUpload, SocketWriter};

/// What the window sees of the room's WebSocket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SocketEvent {
    Message { text: String },
    Closed,
}

/// A node's answer, as the window reads it.
#[derive(Debug, Serialize)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

struct OpenSocket {
    writer: Arc<tokio::sync::Mutex<SocketWriter>>,
    reader: JoinHandle<()>,
}

type Sockets = Arc<Mutex<HashMap<u64, OpenSocket>>>;

/// Everything the window may ask of its node, by id and plain values, so each command is one call.
pub struct Bridge {
    link: LocalLink,
    downloads: PathBuf,
    preferences: Option<Arc<PreferencesStore>>,
    next_id: AtomicU64,
    uploads: Mutex<HashMap<u64, Arc<LocalUpload>>>,
    sockets: Sockets,
}

impl Bridge {
    /// `downloads` is the folder that files the person saves are written into.
    pub fn new(link: LocalLink, downloads: PathBuf) -> Self {
        Self { link, downloads, preferences: None, next_id: AtomicU64::new(1), uploads: Mutex::default(), sockets: Sockets::default() }
    }

    /// Saves into the downloads folder the person chose, when they have chosen one.
    pub fn with_preferences(mut self, preferences: Arc<PreferencesStore>) -> Self {
        self.preferences = Some(preferences);
        self
    }

    /// Where saved files go now: the chosen folder, else the one this bridge was made with.
    pub fn downloads_folder(&self) -> PathBuf {
        let chosen = self.preferences.as_ref().and_then(|preferences| preferences.get().downloads_dir);
        chosen.unwrap_or_else(|| self.downloads.clone())
    }

    pub async fn request(&self, method: &str, path: &str, headers: &[(String, String)], body: String) -> Result<Reply, String> {
        let method = Method::from_bytes(method.as_bytes()).map_err(|_| format!("{method} is not an HTTP method"))?;
        let response = self.link.request(method, path, headers, Bytes::from(body)).await.map_err(|e| e.to_string())?;
        Ok(Reply { status: response.status, body: String::from_utf8_lossy(&response.body).into_owned() })
    }

    pub fn begin_upload(&self, path: &str, headers: &[(String, String)], content_length: u64) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let upload = Arc::new(self.link.begin_upload(path, headers, content_length));
        self.uploads.lock().unwrap().insert(id, upload);
        id
    }

    pub async fn write_upload(&self, id: u64, piece: Vec<u8>) -> Result<(), String> {
        let upload = self.uploads.lock().unwrap().get(&id).cloned().ok_or("no such upload")?;
        upload.write(Bytes::from(piece)).await.map_err(|e| e.to_string())
    }

    pub async fn finish_upload(&self, id: u64) -> Result<Reply, String> {
        let upload = self.uploads.lock().unwrap().remove(&id).ok_or("no such upload")?;
        let response = upload.finish().await.map_err(|e| e.to_string())?;
        Ok(Reply { status: response.status, body: String::from_utf8_lossy(&response.body).into_owned() })
    }

    pub fn abort_upload(&self, id: u64) {
        if let Some(upload) = self.uploads.lock().unwrap().remove(&id) {
            upload.abort();
        }
    }

    /// Opens the room's socket; `on_event` is called for each message and once when the socket ends.
    pub async fn open_socket(&self, on_event: impl Fn(SocketEvent) + Send + 'static) -> Result<u64, String> {
        let (writer, mut reader) = self.link.open_socket().await.map_err(|e| e.to_string())?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let sockets = self.sockets.clone();
        let task = tokio::spawn(async move {
            while let Some(text) = reader.next().await {
                on_event(SocketEvent::Message { text });
            }
            sockets.lock().unwrap().remove(&id);
            on_event(SocketEvent::Closed);
        });
        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        self.sockets.lock().unwrap().insert(id, OpenSocket { writer, reader: task });
        Ok(id)
    }

    pub async fn send_to_socket(&self, id: u64, text: String) -> Result<(), String> {
        let writer = self.sockets.lock().unwrap().get(&id).map(|socket| socket.writer.clone()).ok_or("that socket is closed")?;
        let result = writer.lock().await.send(text).await;
        result.map_err(|e| e.to_string())
    }

    pub async fn close_socket(&self, id: u64) {
        let socket = self.sockets.lock().unwrap().remove(&id);
        if let Some(socket) = socket {
            socket.reader.abort();
            socket.writer.lock().await.close().await;
        }
    }

    /// Writes a file or folder archive from the node into the downloads folder; returns where it went.
    pub async fn save_download(&self, path: &str, file_name: &str) -> Result<String, LinkError> {
        let saved = self.link.save_download(path, &self.downloads_folder(), file_name, |_| {}).await?;
        Ok(saved.path.display().to_string())
    }
}
