//! The HTTP side of file sharing: resumable uploads, Range downloads, folder publishing and zip downloads.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::stream;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Semaphore};

use crate::server::{Api, BoxError, PeerAddr, ServerBody};
use crate::sessions::SessionHandle;
use crate::store::{chunk_len, Blob, StoreError, CHUNK_SIZE};
use crate::transfer::{self, FetchError};
use crate::types::{FileMetadata, Holder};
use crate::{state, validate, zip, NodeState};

const MAX_CONCURRENT_UPLOADS: usize = 8;
const MAX_CONCURRENT_DOWNLOADS: usize = 64;
const MAX_FOLDER_LISTING_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct FilesApi {
    state: NodeState,
    uploads: Arc<Semaphore>,
    downloads: Arc<Semaphore>,
}

impl FilesApi {
    pub fn new(state: NodeState) -> Self {
        Self {
            state,
            uploads: Arc::new(Semaphore::new(MAX_CONCURRENT_UPLOADS)),
            downloads: Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS)),
        }
    }
}

impl Api for FilesApi {
    fn claims(&self, path: &str) -> bool {
        path.starts_with("/api/files/") || path.starts_with("/api/folders/")
    }

    fn handle(&self, req: Request<Incoming>, _peer: PeerAddr) -> impl std::future::Future<Output = Response<ServerBody>> + Send {
        let api = self.clone();
        async move { api.route(req).await }
    }
}

/// What a folder's listing file contains (it is stored like any other file).
#[derive(Debug, Serialize, Deserialize)]
pub struct FolderListing {
    pub v: u32,
    pub name: String,
    pub children: Vec<ListingChild>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListingChild {
    pub path: String,
    pub file_id: String,
    pub size: u64,
}

#[derive(Deserialize)]
struct FolderUpload {
    name: String,
    children: Vec<FolderChild>,
}

#[derive(Deserialize)]
struct FolderChild {
    path: String,
    file_id: String,
}

fn empty() -> ServerBody {
    Full::new(Bytes::new()).map_err(|e| match e {}).boxed_unsync()
}

fn full(bytes: impl Into<Bytes>) -> ServerBody {
    Full::new(bytes.into()).map_err(|e| match e {}).boxed_unsync()
}

fn json(status: StatusCode, value: &impl Serialize) -> Response<ServerBody> {
    let mut response = Response::new(full(serde_json::to_vec(value).unwrap_or_default()));
    *response.status_mut() = status;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn fail(status: StatusCode, message: &str) -> Response<ServerBody> {
    json(status, &serde_json::json!({ "success": false, "message": message }))
}

fn store_error(e: StoreError) -> Response<ServerBody> {
    let status = match e {
        StoreError::InvalidId => StatusCode::BAD_REQUEST,
        StoreError::SizeMismatch => StatusCode::CONFLICT,
        StoreError::QuotaExceeded | StoreError::DiskFull => StatusCode::INSUFFICIENT_STORAGE,
        StoreError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    fail(status, &e.to_string())
}

const MAX_DRAIN_BYTES: u64 = 64 * 1024 * 1024;

async fn drain(mut body: Incoming) {
    let mut drained = 0u64;
    while let Some(Ok(frame)) = body.frame().await {
        drained += frame.data_ref().map_or(0, |d| d.len() as u64);
        if drained > MAX_DRAIN_BYTES {
            return;
        }
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    header_str(headers, name)?.trim().parse().ok()
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| pair.trim().strip_prefix(name)?.strip_prefix('='))
}

// A browser always sends Origin on the requests that change things; it has to name this host.
fn same_origin(headers: &HeaderMap) -> bool {
    let origin = header_str(headers, "origin").and_then(|o| o.strip_prefix("https://").or_else(|| o.strip_prefix("http://")));
    matches!((origin, header_str(headers, "host")), (Some(o), Some(h)) if o == h)
}

fn content_disposition(name: &str) -> String {
    const ATTR_CHAR: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'!')
        .remove(b'#')
        .remove(b'$')
        .remove(b'&')
        .remove(b'+')
        .remove(b'-')
        .remove(b'.')
        .remove(b'^')
        .remove(b'_')
        .remove(b'`')
        .remove(b'|')
        .remove(b'~');
    let ascii: String =
        name.chars().map(|c| if (c.is_ascii_graphic() || c == ' ') && !matches!(c, '"' | '\\' | '%' | ';') { c } else { '_' }).collect();
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{}", utf8_percent_encode(name, ATTR_CHAR))
}

// Some(Ok) is an inclusive byte range, Some(Err) unsatisfiable, None the whole file.
fn parse_range(value: Option<&str>, size: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = value?.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || size == 0 {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let range = match (start.trim(), end.trim()) {
        ("", suffix) => {
            let suffix: u64 = suffix.parse().ok()?;
            if suffix == 0 {
                return Some(Err(()));
            }
            (size.saturating_sub(suffix), size - 1)
        }
        (start, "") => (start.parse().ok()?, size - 1),
        (start, end) => (start.parse().ok()?, end.parse::<u64>().ok()?.min(size - 1)),
    };
    Some(if range.0 >= size || range.0 > range.1 { Err(()) } else { Ok(range) })
}

// Sends bytes `start..=end`, waiting for chunks still arriving; false if it stopped early.
async fn send_range(
    state: &NodeState,
    blob: &Arc<Blob>,
    start: u64,
    end: u64,
    frames: &mpsc::Sender<Result<Frame<Bytes>, BoxError>>,
    mut on_bytes: impl FnMut(&[u8]),
) -> bool {
    let stall_timeout = state.transfers.stall_timeout();
    let reader = transfer::open_reader(state, blob);
    for index in (start / CHUNK_SIZE) as u32..=(end / CHUNK_SIZE) as u32 {
        if let Some(reader) = &reader {
            reader.at(index);
        }
        if !blob.wait_for_chunk(index, stall_timeout).await {
            tracing::warn!("Download of {} stalled at chunk {index}", blob.id());
            let _ = frames.send(Err("the file stopped arriving".into())).await;
            return false;
        }
        let bytes = match blob.read_chunk_verified(index).await {
            Ok(bytes) => bytes,
            Err(e) => {
                let _ = frames.send(Err(Box::new(e) as BoxError)).await;
                return false;
            }
        };
        let chunk_start = index as u64 * CHUNK_SIZE;
        let from = start.saturating_sub(chunk_start) as usize;
        let to = ((end + 1).min(chunk_start + bytes.len() as u64) - chunk_start) as usize;
        let piece = &bytes[from..to];
        on_bytes(piece);
        if frames.send(Ok(Frame::data(Bytes::copy_from_slice(piece)))).await.is_err() {
            return false;
        }
    }
    true
}

fn busy() -> Response<ServerBody> {
    let mut response = fail(StatusCode::SERVICE_UNAVAILABLE, "this node is serving as many downloads as it can; try again in a moment");
    response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    response
}

fn fetch_error(error: FetchError) -> Response<ServerBody> {
    match error {
        FetchError::NotFound => fail(StatusCode::NOT_FOUND, "no such file"),
        FetchError::NoOneHasIt => {
            let mut response = fail(StatusCode::SERVICE_UNAVAILABLE, "no device that has this file is online right now");
            response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("10"));
            response
        }
        FetchError::Store(e) => store_error(e),
    }
}

const ZIP_PREFETCH: usize = 3;

// `If-Range` makes a range request conditional on the file being unchanged.
fn range_still_valid(if_range: Option<&str>, etag: Option<&str>) -> bool {
    match (if_range, etag) {
        (None, _) => true,
        (Some(wanted), Some(current)) => wanted.trim() == current,
        (Some(_), None) => false,
    }
}

impl FilesApi {
    async fn route(&self, req: Request<Incoming>) -> Response<ServerBody> {
        let path = req.uri().path().to_string();
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();

        let auth: Option<SessionHandle> = if self.state.passphrase.is_some() {
            match cookie_value(req.headers(), "auth").and_then(|token| self.state.sessions.authenticate(token)) {
                Some(session) => Some(session),
                None => {
                    drain(req.into_body()).await;
                    return fail(StatusCode::UNAUTHORIZED, "sign in first");
                }
            }
        } else {
            None
        };

        match (req.method().clone(), segments.as_slice()) {
            (Method::PUT, ["api", "files", id]) => self.upload(req, id, auth).await,
            (Method::GET, ["api", "files", id, "upload"]) => self.upload_status(req, id).await,
            (Method::GET | Method::HEAD, ["api", "files", id]) => self.download(req, id).await,
            (Method::PUT, ["api", "folders", id]) => self.publish_folder(req, id, auth).await,
            (Method::GET | Method::HEAD, ["api", "folders", name]) if name.ends_with(".zip") => {
                self.download_zip(req, &name[..name.len() - 4]).await
            }
            _ => fail(StatusCode::NOT_FOUND, "not found"),
        }
    }

    // The upload's device must have joined over a WebSocket owned by this request's login session.
    async fn owned_device(&self, headers: &HeaderMap, auth: &Option<SessionHandle>) -> Result<String, Box<Response<ServerBody>>> {
        let device = header_str(headers, "x-ladex-session").unwrap_or_default().to_string();
        if !validate::is_valid_id(&device) {
            return Err(Box::new(fail(StatusCode::BAD_REQUEST, "missing or invalid X-Ladex-Session")));
        }
        let owners = self.state.session_owners.read().await;
        match owners.get(&device) {
            Some(owner) if owner.auth_id == auth.as_ref().map(|a| a.id.clone()) => Ok(device),
            _ => Err(Box::new(fail(StatusCode::FORBIDDEN, "that device is not connected here"))),
        }
    }

    async fn upload(&self, req: Request<Incoming>, id: &str, auth: Option<SessionHandle>) -> Response<ServerBody> {
        let (parts, body) = req.into_parts();
        let mut body = Some(body);
        let response = self.upload_inner(&parts.headers, id, auth, &mut body).await;
        // Read and drop a refused body, or the client sees a broken connection instead of the reason.
        if let Some(body) = body {
            drain(body).await;
        }
        response
    }

    async fn upload_inner(
        &self,
        headers: &HeaderMap,
        id: &str,
        auth: Option<SessionHandle>,
        body_slot: &mut Option<Incoming>,
    ) -> Response<ServerBody> {
        if !same_origin(headers) {
            return fail(StatusCode::FORBIDDEN, "cross-origin request refused");
        }
        let state = &self.state;
        if !validate::is_valid_id(id) {
            return fail(StatusCode::BAD_REQUEST, "invalid file id");
        }
        let Some(size) = header_u64(headers, "x-ladex-size").filter(|s| *s <= validate::MAX_SAFE_INTEGER) else {
            return fail(StatusCode::BAD_REQUEST, "missing or invalid X-Ladex-Size");
        };
        let offset = header_u64(headers, "x-ladex-offset").unwrap_or(0);
        if !offset.is_multiple_of(CHUNK_SIZE) || offset > size {
            return fail(StatusCode::BAD_REQUEST, "X-Ladex-Offset must be a multiple of 1 MiB and within the file");
        }
        match header_u64(headers, "content-length") {
            None => return fail(StatusCode::LENGTH_REQUIRED, "Content-Length is required"),
            Some(length) if length != size - offset => return fail(StatusCode::BAD_REQUEST, "body length does not match size and offset"),
            Some(_) => {}
        }
        let name = header_str(headers, "x-ladex-name")
            .and_then(|n| percent_decode_str(n).decode_utf8().ok())
            .map(|n| validate::sanitize_file_name(&n))
            .unwrap_or_else(|| "unnamed".to_string());
        let mime = validate::sanitize_mime(header_str(headers, "content-type").unwrap_or(""));
        let parent = match header_str(headers, "x-ladex-parent") {
            Some(p) if validate::is_valid_id(p) => Some(p.to_string()),
            Some(_) => return fail(StatusCode::BAD_REQUEST, "invalid X-Ladex-Parent"),
            None => None,
        };
        let device = match self.owned_device(headers, &auth).await {
            Ok(device) => device,
            Err(response) => return *response,
        };
        let Ok(_slot) = self.uploads.try_acquire() else {
            return fail(StatusCode::SERVICE_UNAVAILABLE, "too many uploads in progress; try again shortly");
        };

        if let Some(existing) = state.store.get(id) {
            if existing.is_complete() {
                let entry = state.files.read().await.get(id).filter(|f| !f.deleted && f.uploader_id == device).cloned();
                return match entry {
                    Some(file) => json(StatusCode::OK, &serde_json::json!({ "success": true, "file": file })),
                    None => fail(StatusCode::CONFLICT, "a file with this id already exists"),
                };
            }
        }
        let blob = match state.store.create(id, size) {
            Ok(blob) => blob,
            Err(e) => return store_error(e),
        };
        if blob.expected_root().is_some() {
            return fail(StatusCode::CONFLICT, "a file with this id is being fetched from other devices");
        }
        let Some(_writer) = blob.try_write_lock() else {
            return fail(StatusCode::CONFLICT, "this file is already being uploaded");
        };

        // The node says where it can continue from; the client must not skip ahead.
        let resume_at = (blob.leading_chunks() as u64 * CHUNK_SIZE).min(size);
        if offset > resume_at {
            return json(
                StatusCode::CONFLICT,
                &serde_json::json!({ "success": false, "message": "resume from an earlier offset", "offset": resume_at }),
            );
        }

        let Some(mut body) = body_slot.take() else {
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "no request body");
        };
        let mut buffer: Vec<u8> = Vec::with_capacity(CHUNK_SIZE as usize);
        let mut index = (offset / CHUNK_SIZE) as u32;
        while let Some(frame) = body.frame().await {
            let Ok(frame) = frame else {
                blob.persist().await;
                return fail(StatusCode::BAD_REQUEST, "upload interrupted");
            };
            let Ok(mut data) = frame.into_data() else { continue };
            while !data.is_empty() {
                if index >= blob.chunk_count() {
                    return fail(StatusCode::BAD_REQUEST, "more data than announced");
                }
                let wanted = chunk_len(size, index) - buffer.len();
                buffer.extend_from_slice(&data.split_to(wanted.min(data.len())));
                if buffer.len() == chunk_len(size, index) {
                    let chunk = std::mem::replace(&mut buffer, Vec::with_capacity(CHUNK_SIZE as usize));
                    if let Err(e) = blob.write_chunk_hashing(index, chunk).await {
                        tracing::warn!("Upload of {id}: could not store chunk {index}: {e:?}");
                        return fail(StatusCode::INTERNAL_SERVER_ERROR, "could not store the file");
                    }
                    index += 1;
                }
            }
        }
        blob.persist().await;
        if !buffer.is_empty() {
            return fail(StatusCode::BAD_REQUEST, "the body ended before the file did");
        }
        if !blob.bitmap().is_full() {
            let done = (blob.leading_chunks() as u64 * CHUNK_SIZE).min(size);
            return json(StatusCode::OK, &serde_json::json!({ "success": true, "complete": false, "offset": done }));
        }

        let root = match blob.seal() {
            Ok(root) => root,
            Err(why) => return fail(StatusCode::INTERNAL_SERVER_ERROR, why),
        };
        let stamp = state.clock.now();
        let entry = FileMetadata {
            id: id.to_string(),
            name,
            size,
            mime_type: mime,
            uploader_id: device,
            uploader_node: state.node_id.clone(),
            holders: [(state.node_id.clone(), Holder { since: stamp.clone(), present: true })].into(),
            uploaded_at: chrono::Utc::now(),
            created_at: stamp.wall,
            version: stamp,
            deleted: false,
            deleted_at: 0,
            manifest_root: Some(root),
            is_folder: false,
            parent,
            folder_bytes: 0,
            folder_files: 0,
        };
        state::publish_entry(state, entry.clone()).await;
        tracing::info!("Upload: {} ({} bytes) shared by {}", entry.name, size, entry.uploader_id);
        json(StatusCode::CREATED, &serde_json::json!({ "success": true, "complete": true, "file": entry }))
    }

    async fn upload_status(&self, req: Request<Incoming>, id: &str) -> Response<ServerBody> {
        let size =
            req.uri().query().and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("size="))).and_then(|s| s.parse::<u64>().ok());
        let blob = self.state.store.get(id).filter(|b| Some(b.size()) == size && b.expected_root().is_none());
        let offset = blob.as_ref().map_or(0, |b| (b.leading_chunks() as u64 * CHUNK_SIZE).min(b.size()));
        let complete = blob.is_some_and(|b| b.is_complete());
        json(StatusCode::OK, &serde_json::json!({ "offset": offset, "complete": complete }))
    }

    async fn download(&self, req: Request<Incoming>, id: &str) -> Response<ServerBody> {
        let state = &self.state;
        let blob = match transfer::ensure_file(state, id).await {
            Ok(blob) => blob,
            Err(e) => return fetch_error(e),
        };
        let (name, etag) = {
            let files = state.files.read().await;
            match files.get(id) {
                Some(f) => (f.name.clone(), f.manifest_root.as_ref().map(|root| format!("\"{root}\""))),
                None => return fail(StatusCode::NOT_FOUND, "no such file"),
            }
        };
        let size = blob.size();

        let range_header =
            header_str(req.headers(), "range").filter(|_| range_still_valid(header_str(req.headers(), "if-range"), etag.as_deref()));
        let (status, start, end) = match parse_range(range_header, size) {
            None => (StatusCode::OK, 0, size.saturating_sub(1)),
            Some(Ok((start, end))) => (StatusCode::PARTIAL_CONTENT, start, end),
            Some(Err(())) => {
                let mut response = fail(StatusCode::RANGE_NOT_SATISFIABLE, "range not satisfiable");
                response.headers_mut().insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes */{size}")).unwrap());
                return response;
            }
        };
        let length = if size == 0 { 0 } else { end - start + 1 };

        let mut response = Response::new(empty());
        *response.status_mut() = status;
        let headers = response.headers_mut();
        // Only ever offered as a download: a shared HTML or SVG file must not run as part of this page.
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
        headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
        headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        if let Some(value) = etag.as_deref().and_then(|e| HeaderValue::from_str(e).ok()) {
            headers.insert(header::ETAG, value);
        }
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
        if let Ok(value) = HeaderValue::from_str(&content_disposition(&name)) {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
        if status == StatusCode::PARTIAL_CONTENT {
            headers.insert(header::CONTENT_RANGE, HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")).unwrap());
        }
        if req.method() == Method::HEAD || length == 0 {
            return response;
        }

        let Ok(slot) = self.downloads.clone().try_acquire_owned() else {
            return busy();
        };
        let (frames, receiver) = mpsc::channel::<Result<Frame<Bytes>, BoxError>>(4);
        let state = state.clone();
        tokio::spawn(async move {
            let _slot = slot;
            send_range(&state, &blob, start, end, &frames, |_| {}).await;
        });
        let stream = stream::unfold(receiver, |mut receiver| async { receiver.recv().await.map(|item| (item, receiver)) });
        *response.body_mut() = StreamBody::new(stream).boxed_unsync();
        response
    }

    async fn download_zip(&self, req: Request<Incoming>, folder_id: &str) -> Response<ServerBody> {
        let state = &self.state;
        let listing_blob = match transfer::ensure_file(state, folder_id).await {
            Ok(blob) => blob,
            Err(e) => return fetch_error(e),
        };
        let folder = match state.files.read().await.get(folder_id) {
            Some(f) if f.is_folder && !f.deleted => f.clone(),
            _ => return fail(StatusCode::NOT_FOUND, "no such folder"),
        };

        let mut raw = Vec::with_capacity(listing_blob.size() as usize);
        for index in 0..listing_blob.chunk_count() {
            if !listing_blob.wait_for_chunk(index, state.transfers.stall_timeout()).await {
                return fail(StatusCode::SERVICE_UNAVAILABLE, "the folder's contents are not available right now");
            }
            match listing_blob.read_chunk_verified(index).await {
                Ok(bytes) => raw.extend(bytes),
                Err(_) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "could not read the folder"),
            }
        }
        let Ok(listing) = serde_json::from_slice::<FolderListing>(&raw) else {
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "the folder's listing is damaged");
        };
        if listing.children.len() > validate::MAX_FOLDER_FILES as usize {
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "the folder's listing is damaged");
        }

        // Sizes come from the catalog, not from the listing, so the length announced is the length sent.
        let mut children: Vec<(String, String, u64)> = Vec::with_capacity(listing.children.len());
        {
            let files = state.files.read().await;
            for child in &listing.children {
                let entry = files.get(&child.file_id).filter(|f| !f.deleted && f.parent.as_deref() == Some(folder_id));
                let (Some(entry), Some(path)) = (entry, validate::sanitize_relative_path(&child.path)) else {
                    return fail(StatusCode::CONFLICT, "part of this folder is no longer available");
                };
                children.push((path, entry.id.clone(), entry.size));
            }
        }
        for (_, id, _) in children.iter().take(ZIP_PREFETCH) {
            if let Err(e) = transfer::ensure_file(state, id).await {
                return fetch_error(e);
            }
        }

        let plan: Vec<(String, u64)> = children.iter().map(|(path, _, size)| (path.clone(), *size)).collect();
        let length = zip::archive_length(&plan);
        let archive_name = validate::sanitize_file_name(&format!("{}.zip", folder.name));

        let mut response = Response::new(empty());
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/zip"));
        headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
        if let Ok(value) = HeaderValue::from_str(&content_disposition(&archive_name)) {
            headers.insert(header::CONTENT_DISPOSITION, value);
        }
        if req.method() == Method::HEAD {
            return response;
        }

        let when = zip::dos_time(folder.uploaded_at);
        let Ok(slot) = self.downloads.clone().try_acquire_owned() else {
            return busy();
        };
        let (frames, receiver) = mpsc::channel::<Result<Frame<Bytes>, BoxError>>(4);
        let state = state.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let mut writer = zip::ZipWriter::new();
            let send = |bytes: Vec<u8>| frames.send(Ok(Frame::data(Bytes::from(bytes))));
            for (i, (path, id, size)) in children.iter().enumerate() {
                if let Some((_, ahead, _)) = children.get(i + ZIP_PREFETCH) {
                    let _ = transfer::ensure_file(&state, ahead).await;
                }
                let blob = match transfer::ensure_file(&state, id).await {
                    Ok(blob) => blob,
                    Err(e) => {
                        let _ = frames.send(Err(e.to_string().into())).await;
                        return;
                    }
                };
                if send(writer.start_file(path, *size, when)).await.is_err() {
                    return;
                }
                if *size > 0 && !send_range(&state, &blob, 0, size - 1, &frames, |piece| writer.file_data(piece)).await {
                    return;
                }
                if send(writer.finish_file()).await.is_err() {
                    return;
                }
            }
            let _ = send(writer.finish()).await;
        });
        let stream = stream::unfold(receiver, |mut receiver| async { receiver.recv().await.map(|item| (item, receiver)) });
        *response.body_mut() = StreamBody::new(stream).boxed_unsync();
        response
    }

    async fn publish_folder(&self, req: Request<Incoming>, id: &str, auth: Option<SessionHandle>) -> Response<ServerBody> {
        if !same_origin(req.headers()) {
            return fail(StatusCode::FORBIDDEN, "cross-origin request refused");
        }
        let state = &self.state;
        if !validate::is_valid_id(id) {
            return fail(StatusCode::BAD_REQUEST, "invalid folder id");
        }
        let device = match self.owned_device(req.headers(), &auth).await {
            Ok(device) => device,
            Err(response) => return *response,
        };
        let body = match Limited::new(req.into_body(), MAX_FOLDER_LISTING_BYTES).collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(_) => return fail(StatusCode::PAYLOAD_TOO_LARGE, "folder listing too large"),
        };
        let Ok(folder) = serde_json::from_slice::<FolderUpload>(&body) else {
            return fail(StatusCode::BAD_REQUEST, "invalid folder listing");
        };
        if folder.children.is_empty() || folder.children.len() > validate::MAX_FOLDER_FILES as usize {
            return fail(StatusCode::BAD_REQUEST, "a folder needs between 1 and 100000 files");
        }

        // Every file must have been uploaded by this device, for this folder.
        let mut listing = FolderListing { v: 1, name: validate::sanitize_file_name(&folder.name), children: Vec::new() };
        let mut seen = std::collections::HashSet::new();
        {
            let files = state.files.read().await;
            for child in &folder.children {
                let Some(path) = validate::sanitize_relative_path(&child.path) else {
                    return fail(StatusCode::BAD_REQUEST, "a file in the folder has an unusable path");
                };
                if !seen.insert(path.to_lowercase()) {
                    return fail(StatusCode::BAD_REQUEST, "two files in the folder have the same path");
                }
                let entry = files.get(&child.file_id).filter(|f| !f.deleted);
                match entry {
                    Some(f) if f.uploader_id == device && f.parent.as_deref() == Some(id) && f.manifest_root.is_some() => {
                        listing.children.push(ListingChild { path, file_id: f.id.clone(), size: f.size });
                    }
                    _ => return fail(StatusCode::CONFLICT, "a file in the folder was not uploaded for it"),
                }
            }
        }
        listing.children.sort_by(|a, b| a.path.cmp(&b.path));
        let bytes = serde_json::to_vec(&listing).unwrap_or_default();

        let blob = match state.store.create(id, bytes.len() as u64) {
            Ok(blob) => blob,
            Err(e) => return store_error(e),
        };
        for index in 0..blob.chunk_count() {
            let start = index as usize * CHUNK_SIZE as usize;
            let chunk = bytes[start..(start + CHUNK_SIZE as usize).min(bytes.len())].to_vec();
            if blob.write_chunk_hashing(index, chunk).await.is_err() {
                return fail(StatusCode::INTERNAL_SERVER_ERROR, "could not store the folder");
            }
        }
        let root = match blob.seal() {
            Ok(root) => root,
            Err(why) => return fail(StatusCode::INTERNAL_SERVER_ERROR, why),
        };
        let stamp = state.clock.now();
        let entry = FileMetadata {
            id: id.to_string(),
            name: listing.name.clone(),
            size: bytes.len() as u64,
            mime_type: "inode/directory".into(),
            uploader_id: device,
            uploader_node: state.node_id.clone(),
            holders: [(state.node_id.clone(), Holder { since: stamp.clone(), present: true })].into(),
            uploaded_at: chrono::Utc::now(),
            created_at: stamp.wall,
            version: stamp,
            deleted: false,
            deleted_at: 0,
            manifest_root: Some(root),
            is_folder: true,
            parent: None,
            folder_bytes: listing.children.iter().map(|c| c.size).sum(),
            folder_files: listing.children.len() as u32,
        };
        state::publish_entry(state, entry.clone()).await;
        json(StatusCode::CREATED, &serde_json::json!({ "success": true, "file": entry }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 100), None);
        assert_eq!(parse_range(Some("bytes=0-9"), 100), Some(Ok((0, 9))));
        assert_eq!(parse_range(Some("bytes=90-"), 100), Some(Ok((90, 99))));
        assert_eq!(parse_range(Some("bytes=-10"), 100), Some(Ok((90, 99))));
        assert_eq!(parse_range(Some("bytes=-500"), 100), Some(Ok((0, 99))), "a suffix longer than the file is the whole file");
        assert_eq!(parse_range(Some("bytes=50-5000"), 100), Some(Ok((50, 99))), "an end past the file is clamped");
        assert_eq!(parse_range(Some("bytes=100-"), 100), Some(Err(())));
        assert_eq!(parse_range(Some("bytes=60-50"), 100), Some(Err(())));
        assert_eq!(parse_range(Some("bytes=-0"), 100), Some(Err(())));
        assert_eq!(parse_range(Some("bytes=0-1,5-6"), 100), None);
        assert_eq!(parse_range(Some("items=0-1"), 100), None);
        assert_eq!(parse_range(Some("bytes=a-b"), 100), None);
        assert_eq!(parse_range(Some("bytes=0-0"), 0), None);
    }

    #[test]
    fn a_conditional_range_only_applies_to_an_unchanged_file() {
        assert!(range_still_valid(None, Some("\"abc\"")));
        assert!(range_still_valid(None, None));
        assert!(range_still_valid(Some("\"abc\""), Some("\"abc\"")));
        assert!(!range_still_valid(Some("\"old\""), Some("\"abc\"")));
        assert!(!range_still_valid(Some("\"abc\""), None));
        assert!(!range_still_valid(Some("Wed, 21 Oct 2015 07:28:00 GMT"), Some("\"abc\"")));
    }

    #[test]
    fn download_names_are_safe_in_a_header() {
        let plain = content_disposition("report.pdf");
        assert_eq!(plain, "attachment; filename=\"report.pdf\"; filename*=UTF-8''report.pdf");

        let nasty = content_disposition("a\"b\r\nX-Evil: 1;%.txt");
        assert!(!nasty.contains('\r') && !nasty.contains('\n'));
        assert!(nasty.starts_with("attachment; filename=\"a_b__X-Evil: 1__.txt\""));
        assert!(HeaderValue::from_str(&nasty).is_ok());

        let unicode = content_disposition("日本語 ñ.txt");
        assert!(unicode.contains("filename=\"___ _.txt\"") || unicode.contains("filename=\"_"));
        assert!(unicode.contains("filename*=UTF-8''%E6%97%A5%E6%9C%AC%E8%AA%9E%20%C3%B1.txt"), "{unicode}");
    }

    #[test]
    fn cookies_are_found_among_others() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_static("theme=dark; auth=abc123; other=1"));
        assert_eq!(cookie_value(&headers, "auth"), Some("abc123"));
        assert_eq!(cookie_value(&headers, "missing"), None);
        headers.insert(header::COOKIE, HeaderValue::from_static("xauth=evil; auth=good"));
        assert_eq!(cookie_value(&headers, "auth"), Some("good"));
        headers.insert(header::COOKIE, HeaderValue::from_static("xauth=evil"));
        assert_eq!(cookie_value(&headers, "auth"), None);
    }

    #[test]
    fn mutations_must_come_from_this_host() {
        let with = |origin: Option<&str>, host: &str| {
            let mut h = HeaderMap::new();
            if let Some(o) = origin {
                h.insert("origin", HeaderValue::from_str(o).unwrap());
            }
            h.insert("host", HeaderValue::from_str(host).unwrap());
            same_origin(&h)
        };
        assert!(with(Some("https://192.168.1.5:8080"), "192.168.1.5:8080"));
        assert!(with(Some("http://localhost:8081"), "localhost:8081"));
        assert!(!with(Some("https://evil.example"), "192.168.1.5:8080"));
        assert!(!with(None, "192.168.1.5:8080"));
    }
}
