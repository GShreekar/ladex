//! An in-process connection to a node's own web server, so the app's window reaches the node with no socket, certificate or browser between.

use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::stream::{self, SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::{Body, Frame, Incoming};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::fs;
use tokio::io::{AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::server::{BoxError, PeerAddr};
use crate::{validate, NodeState};

pub(crate) const PIPE_BYTES: usize = 256 * 1024;
const UPLOAD_BACKLOG: usize = 4;
const ORIGIN: &str = "http://localhost";
const HOST: &str = "localhost";
const SESSION_LABEL: &str = "LADEX app";

/// Opens a new pipe to the node's web server; the other end is already being served.
pub(crate) type OpenPipe = Arc<dyn Fn() -> DuplexStream + Send + Sync>;

/// The address the node sees these connections come from: this machine, so the person at it may manage guests and trust.
pub(crate) fn local_peer() -> PeerAddr {
    PeerAddr { addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), tls: false }
}

/// Why a request over the link did not give an answer to use.
#[derive(Debug)]
pub enum LinkError {
    /// The node answered with a refusal; `message` is what it said, when it said anything.
    Refused {
        status: u16,
        message: String,
    },
    /// The node could not be reached, or the connection broke.
    Broken(String),
    Io(std::io::Error),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused { status, message } if message.is_empty() => write!(f, "the node answered {status}"),
            Self::Refused { message, .. } => f.write_str(message),
            Self::Broken(reason) => write!(f, "could not reach the node: {reason}"),
            Self::Io(e) => write!(f, "could not save the file: {e}"),
        }
    }
}

impl std::error::Error for LinkError {}

impl From<std::io::Error> for LinkError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

fn broken(error: impl std::fmt::Display) -> LinkError {
    LinkError::Broken(error.to_string())
}

/// A complete answer from the node.
#[derive(Debug)]
pub struct LocalResponse {
    pub status: u16,
    pub body: Bytes,
}

/// A file written to disk by `LocalLink::save_download`.
#[derive(Debug)]
pub struct SavedFile {
    pub path: PathBuf,
    pub size_bytes: u64,
}

/// A file or folder archive the node has begun to send, to be written wherever the caller chooses.
pub struct Download {
    body: Incoming,
}

impl Download {
    /// Writes the whole download into `file`; returns how many bytes were written. On failure the caller removes what it made.
    pub async fn write_to(mut self, file: &mut (impl AsyncWrite + Unpin), mut on_progress: impl FnMut(u64)) -> Result<u64, LinkError> {
        let mut written: u64 = 0;
        while let Some(frame) = self.body.frame().await {
            let Ok(data) = frame.map_err(broken)?.into_data() else { continue };
            file.write_all(&data).await?;
            written += data.len() as u64;
            on_progress(written);
        }
        file.flush().await?;
        Ok(written)
    }
}

/// The way the app's window talks to its node; every request goes through the same handlers a browser's would.
#[derive(Clone)]
pub struct LocalLink {
    state: NodeState,
    open_pipe: OpenPipe,
    node_stopped: watch::Receiver<bool>,
    login_token: Arc<Mutex<Option<String>>>,
    runtime: tokio::runtime::Handle,
}

impl LocalLink {
    pub(crate) fn new(state: NodeState, open_pipe: OpenPipe, node_stopped: watch::Receiver<bool>, runtime: tokio::runtime::Handle) -> Self {
        Self { state, open_pipe, node_stopped, login_token: Arc::new(Mutex::new(None)), runtime }
    }

    /// Sends a request and waits for the whole answer; `headers` are added to the ones the link always sends.
    pub async fn request(&self, method: Method, path: &str, headers: &[(String, String)], body: Bytes) -> Result<LocalResponse, LinkError> {
        let response = self.send(method, path, headers, Full::new(body)).await?;
        let status = response.status().as_u16();
        let body = response.into_body().collect().await.map_err(broken)?.to_bytes();
        Ok(LocalResponse { status, body })
    }

    /// Starts a PUT whose body is written piece by piece with `LocalUpload::write`; `content_length` is the total to be written.
    /// Safe to call from any thread: the request runs on the node's runtime.
    pub fn begin_upload(&self, path: &str, headers: &[(String, String)], content_length: u64) -> LocalUpload {
        let (pieces, receiver) = mpsc::channel::<Bytes>(UPLOAD_BACKLOG);
        let frames = stream::unfold(receiver, |mut receiver| async { receiver.recv().await.map(|piece| (piece, receiver)) })
            .map(|piece| Ok::<_, Infallible>(Frame::data(piece)));
        let mut headers = headers.to_vec();
        headers.push(("content-length".to_string(), content_length.to_string()));

        let link = self.clone();
        let path = path.to_string();
        let answer = self.runtime.spawn(async move {
            let response = link.send(Method::PUT, &path, &headers, StreamBody::new(frames)).await?;
            let status = response.status().as_u16();
            let body = response.into_body().collect().await.map_err(broken)?.to_bytes();
            Ok(LocalResponse { status, body })
        });
        LocalUpload { pieces: Mutex::new(Some(pieces)), answer: Mutex::new(Some(answer)) }
    }

    /// Asks the node for a file or folder archive; the answer is checked before anything is written anywhere.
    pub async fn start_download(&self, path: &str) -> Result<Download, LinkError> {
        let response = self.send(Method::GET, path, &[], Empty::<Bytes>::new()).await?;
        if response.status() != StatusCode::OK {
            let status = response.status().as_u16();
            let body = response.into_body().collect().await.map_err(broken)?.to_bytes();
            return Err(refusal(status, &body));
        }
        Ok(Download { body: response.into_body() })
    }

    /// Fetches a file or folder archive from the node and writes it into `directory` under a name that is not taken yet.
    pub async fn save_download(
        &self,
        path: &str,
        directory: &Path,
        file_name: &str,
        on_progress: impl FnMut(u64),
    ) -> Result<SavedFile, LinkError> {
        let download = self.start_download(path).await?;
        fs::create_dir_all(directory).await?;
        let (mut file, target) = create_unique(directory, &validate::sanitize_file_name(file_name)).await?;
        match download.write_to(&mut file, on_progress).await {
            Ok(size_bytes) => Ok(SavedFile { path: target, size_bytes }),
            Err(error) => {
                drop(file);
                let _ = fs::remove_file(&target).await;
                Err(error)
            }
        }
    }

    /// Opens the same WebSocket a browser tab uses, for the live room: devices, files, chat and offers.
    pub async fn open_socket(&self) -> Result<(SocketWriter, SocketReader), LinkError> {
        let mut request = format!("ws://{HOST}/ws").into_client_request().map_err(broken)?;
        request.headers_mut().insert("origin", HeaderValue::from_static(ORIGIN));
        if let Some(cookie) = self.login_cookie() {
            request.headers_mut().insert("cookie", HeaderValue::from_str(&cookie).map_err(broken)?);
        }
        let (socket, _) = tokio_tungstenite::client_async(request, (self.open_pipe)()).await.map_err(broken)?;
        let (sink, stream) = socket.split();
        Ok((SocketWriter { sink }, SocketReader { stream, node_stopped: self.node_stopped.clone() }))
    }

    async fn send<B>(
        &self,
        method: Method,
        path: &str,
        headers: &[(String, String)],
        body: B,
    ) -> Result<hyper::Response<Incoming>, LinkError>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<BoxError>,
    {
        let mut request = Request::builder().method(method).uri(path).body(body).map_err(broken)?;
        let map: &mut HeaderMap = request.headers_mut();
        map.insert("host", HeaderValue::from_static(HOST));
        map.insert("origin", HeaderValue::from_static(ORIGIN));
        if let Some(cookie) = self.login_cookie() {
            map.insert("cookie", HeaderValue::from_str(&cookie).map_err(broken)?);
        }
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()).map_err(broken)?;
            map.insert(name, HeaderValue::from_str(value).map_err(broken)?);
        }

        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new((self.open_pipe)())).await.map_err(broken)?;
        tokio::spawn(async move {
            // Ends when the response body has been read or dropped.
            let _ = connection.await;
        });
        sender.send_request(request).await.map_err(broken)
    }

    // A node with a passphrase only answers a signed-in browser, so the link signs itself in, again if that login ended.
    fn login_cookie(&self) -> Option<String> {
        self.state.passphrase.as_ref()?;
        let mut token = self.login_token.lock().unwrap();
        let is_valid = token.as_deref().is_some_and(|token| self.state.sessions.authenticate(token).is_some());
        if !is_valid {
            *token = Some(self.state.sessions.create(Ipv4Addr::LOCALHOST.into(), Some(SESSION_LABEL)).0);
        }
        token.as_ref().map(|token| format!("auth={token}"))
    }
}

/// A request body being written to the node; finish it to get the node's answer.
pub struct LocalUpload {
    pieces: Mutex<Option<mpsc::Sender<Bytes>>>,
    answer: Mutex<Option<JoinHandle<Result<LocalResponse, LinkError>>>>,
}

impl LocalUpload {
    /// Queues the next piece; waits while earlier ones are still being taken. Fails once the node has answered, so call `finish`.
    pub async fn write(&self, piece: Bytes) -> Result<(), LinkError> {
        let pieces = self.pieces.lock().unwrap().clone().ok_or_else(|| broken("the upload has ended"))?;
        pieces.send(piece).await.map_err(|_| broken("the node stopped reading the upload"))
    }

    /// Ends the body and returns what the node answered, which it may have done before the body was complete.
    pub async fn finish(&self) -> Result<LocalResponse, LinkError> {
        self.pieces.lock().unwrap().take();
        let answer = self.answer.lock().unwrap().take().ok_or_else(|| broken("the upload has ended"))?;
        answer.await.map_err(broken)?
    }

    /// Gives up: the request is dropped without being completed.
    pub fn abort(&self) {
        self.pieces.lock().unwrap().take();
        if let Some(answer) = self.answer.lock().unwrap().take() {
            answer.abort();
        }
    }
}

/// The sending half of the room's WebSocket.
pub struct SocketWriter {
    sink: SplitSink<WebSocketStream<DuplexStream>, Message>,
}

impl SocketWriter {
    pub async fn send(&mut self, text: String) -> Result<(), LinkError> {
        self.sink.send(Message::text(text)).await.map_err(broken)
    }

    pub async fn close(&mut self) {
        let _ = self.sink.close().await;
    }
}

/// The receiving half of the room's WebSocket.
pub struct SocketReader {
    stream: SplitStream<WebSocketStream<DuplexStream>>,
    node_stopped: watch::Receiver<bool>,
}

impl SocketReader {
    /// The next message from the node, or None once the connection has closed or the node has stopped.
    pub async fn next(&mut self) -> Option<String> {
        loop {
            let incoming = tokio::select! {
                incoming = self.stream.next() => incoming?,
                _ = self.node_stopped.wait_for(|is_stopped| *is_stopped) => return None,
            };
            match incoming {
                Ok(Message::Text(text)) => return Some(text.to_string()),
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }
}

fn refusal(status: u16, body: &[u8]) -> LinkError {
    let message = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|reply| reply.get("message").and_then(|message| message.as_str().map(str::to_string)))
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    LinkError::Refused { status, message }
}

/// Creates `directory/name`, or `name (2).ext`, `name (3).ext`... when that is taken, and never overwrites a file.
async fn create_unique(directory: &Path, name: &str) -> std::io::Result<(fs::File, PathBuf)> {
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    for attempt in 1.. {
        let candidate = match attempt {
            1 => name.to_string(),
            n => format!("{stem} ({n}){extension}"),
        };
        let path = directory.join(&candidate);
        match fs::OpenOptions::new().write(true).create_new(true).open(&path).await {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!("the attempt counter does not run out")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_taken_name_gets_a_number_before_its_extension() {
        let dir = tempfile::tempdir().unwrap();
        let (_, first) = create_unique(dir.path(), "notes.txt").await.unwrap();
        let (_, second) = create_unique(dir.path(), "notes.txt").await.unwrap();
        let (_, third) = create_unique(dir.path(), "notes.txt").await.unwrap();

        let names: Vec<_> = [first, second, third].iter().map(|path| path.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, ["notes.txt", "notes (2).txt", "notes (3).txt"]);
    }

    #[tokio::test]
    async fn a_name_without_an_extension_is_numbered_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        create_unique(dir.path(), "Makefile").await.unwrap();
        let (_, second) = create_unique(dir.path(), "Makefile").await.unwrap();
        assert_eq!(second.file_name().unwrap().to_string_lossy(), "Makefile (2)");
    }

    #[test]
    fn a_refusal_carries_the_message_the_node_gave() {
        let error = refusal(409, br#"{"success":false,"message":"a file with this id already exists"}"#);
        assert_eq!(error.to_string(), "a file with this id already exists");
        assert_eq!(refusal(503, b"").to_string(), "the node answered 503");
    }
}
