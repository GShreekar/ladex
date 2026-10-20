//! The HTTP routes browser guests use: joining, the live catalog, devices and downloads.
//! Uploads are in `tus.rs` and the host's controls over guests in `host.rs`.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream;
use http_body_util::{BodyExt, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::http::request::Parts;
use hyper::{Method, Request, Response, StatusCode};
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc, Semaphore};

use super::sessions::GuestSession;
use crate::files_api::{cookie_value, drain, fail, full, json, same_origin, FilesApi};
use crate::server::{peer_ip, Api, BoxError, PeerAddr, ServerBody};
use crate::{control, state, tls, validate, NodeState};

pub(super) const GUEST_COOKIE: &str = "guest";
const MAX_JOIN_BODY_BYTES: usize = 1024;
const MAX_SECRET_CHARS: usize = 64;
const MAX_CATALOG_STREAMS: usize = 64;
const CATALOG_DEBOUNCE: Duration = Duration::from_millis(250);
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// The page served at /join. `{{fingerprint}}` in it is replaced by the node's certificate fingerprint.
#[derive(Clone)]
pub struct JoinPage {
    html: Arc<str>,
}

impl JoinPage {
    /// A page whose script and styles are files the node serves elsewhere.
    pub fn new(html: String) -> Self {
        Self { html: html.into() }
    }
}

/// The response that turns a request away; boxed because a response is large and every `?` carries one.
pub(super) type Refusal = Box<Response<ServerBody>>;

/// A handler's answer: a refusal is a response too, so `?` can end a handler early.
pub(super) type Outcome = Result<Response<ServerBody>, Refusal>;

pub(super) fn refuse(status: StatusCode, message: &str) -> Refusal {
    Box::new(fail(status, message))
}

#[derive(Clone)]
pub struct GuestApi {
    pub(super) state: NodeState,
    pub(super) files: FilesApi,
    join_page: JoinPage,
    catalog_streams: Arc<Semaphore>,
}

impl GuestApi {
    pub fn new(state: NodeState, files: FilesApi, join_page: JoinPage) -> Self {
        Self { state, files, join_page, catalog_streams: Arc::new(Semaphore::new(MAX_CATALOG_STREAMS)) }
    }
}

impl Api for GuestApi {
    fn claims(&self, path: &str) -> bool {
        matches!(path, "/join" | "/uploads" | "/api/catalog" | "/api/devices" | "/api/me")
            || path.starts_with("/blobs/")
            || path.starts_with("/uploads/")
            || path.starts_with("/api/guests/")
    }

    fn handle(&self, req: Request<Incoming>, peer: PeerAddr) -> impl std::future::Future<Output = Response<ServerBody>> + Send {
        let api = self.clone();
        async move { api.route(req, peer).await }
    }
}

#[derive(Deserialize)]
struct JoinRequest {
    token: Option<String>,
    code: Option<String>,
}

pub(super) fn require_same_origin(headers: &HeaderMap) -> Result<(), Refusal> {
    match same_origin(headers) {
        true => Ok(()),
        false => Err(refuse(StatusCode::FORBIDDEN, "cross-origin request refused")),
    }
}

fn page(body: String, content_type: &'static str) -> Response<ServerBody> {
    let mut response = Response::new(full(body));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    // The invitation token rides in the URL fragment: it must not leak through a Referer or be framed.
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
    );
    response
}

fn cookie(token: &str, max_age: Duration, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{GUEST_COOKIE}={token}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict{secure}", max_age.as_secs())
}

fn set_cookie(mut response: Response<ServerBody>, value: &str) -> Response<ServerBody> {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

fn event(name: &str, data: &str) -> Frame<Bytes> {
    Frame::data(Bytes::from(format!("event: {name}\ndata: {data}\n\n")))
}

impl GuestApi {
    async fn route(&self, req: Request<Incoming>, peer: PeerAddr) -> Response<ServerBody> {
        let (parts, body) = req.into_parts();
        let mut body = Some(body);
        let outcome = self.dispatch(&parts, &mut body, peer).await;
        // A refused body is read and dropped, or the client sees a broken connection instead of the reason.
        if let Some(body) = body {
            drain(body).await;
        }
        outcome.unwrap_or_else(|refusal| *refusal)
    }

    async fn dispatch(&self, parts: &Parts, body: &mut Option<Incoming>, peer: PeerAddr) -> Outcome {
        let path = parts.uri.path();
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match (parts.method.clone(), segments.as_slice()) {
            (Method::GET | Method::HEAD, ["join"]) => Ok(self.join_page()),
            (Method::POST, ["join"]) => self.join(parts, body, peer).await,
            (Method::GET, ["api", "me"]) => self.me(parts),
            (Method::DELETE, ["api", "me"]) => self.leave(parts, peer),
            (Method::GET, ["api", "catalog"]) => self.catalog(parts).await,
            (Method::GET, ["api", "devices"]) => self.devices(parts).await,
            (Method::GET | Method::HEAD, ["blobs", id]) => self.blob(parts, id).await,
            (Method::OPTIONS, ["uploads"]) => Ok(self.upload_options()),
            (Method::POST, ["uploads"]) => self.create_upload(parts).await,
            (Method::HEAD, ["uploads", id]) => self.upload_status(parts, id),
            (Method::PATCH, ["uploads", id]) => self.append_to_upload(parts, id, body).await,
            (Method::DELETE, ["uploads", id]) => self.cancel_upload(parts, id),
            (method, ["api", "guests", rest @ ..]) => self.host(parts, method, rest, body, peer).await,
            _ => Err(refuse(StatusCode::NOT_FOUND, "not found")),
        }
    }

    pub(super) fn guest_of(&self, headers: &HeaderMap) -> Option<GuestSession> {
        cookie_value(headers, GUEST_COOKIE).and_then(|token| self.state.guests.sessions.authenticate(token))
    }

    pub(super) fn require_guest(&self, headers: &HeaderMap) -> Result<GuestSession, Refusal> {
        self.guest_of(headers).ok_or_else(|| refuse(StatusCode::UNAUTHORIZED, "join with an invitation first"))
    }

    fn join_page(&self) -> Response<ServerBody> {
        let fingerprint = match self.state.tls_fingerprint.is_empty() {
            true => "This node is not using encryption.".to_string(),
            false => tls::format_fingerprint(&self.state.tls_fingerprint),
        };
        page(self.join_page.html.replace("{{fingerprint}}", &fingerprint), "text/html; charset=utf-8")
    }

    async fn join(&self, parts: &Parts, body: &mut Option<Incoming>, peer: PeerAddr) -> Outcome {
        require_same_origin(&parts.headers)?;
        let body = body.take().ok_or_else(|| refuse(StatusCode::INTERNAL_SERVER_ERROR, "no request body"))?;
        let bytes = Limited::new(body, MAX_JOIN_BODY_BYTES)
            .collect()
            .await
            .map_err(|_| refuse(StatusCode::PAYLOAD_TOO_LARGE, "request too large"))?
            .to_bytes();
        let request: JoinRequest = serde_json::from_slice(&bytes).map_err(|_| refuse(StatusCode::BAD_REQUEST, "invalid request"))?;
        let secret = request.token.or(request.code).filter(|s| !s.is_empty() && s.chars().count() <= MAX_SECRET_CHARS);
        let secret = secret.ok_or_else(|| refuse(StatusCode::BAD_REQUEST, "send the invitation token or code"))?;

        // Counted before the secret is checked, so parallel guesses can't slip through before the first failure.
        let guests = &self.state.guests;
        let ip = peer_ip(Some(peer));
        let ticket = guests.join_limiter.begin(ip).map_err(|retry_after| {
            let secs = retry_after.as_secs().max(1);
            tracing::warn!("Guest: {ip} is locked out of joining for another {secs}s");
            let mut response = refuse(StatusCode::TOO_MANY_REQUESTS, &format!("Too many wrong codes. Try again in {secs} seconds."));
            response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(secs));
            response
        })?;
        let Some(permission) = guests.invites.redeem(&secret) else {
            tracing::warn!("Guest: wrong invitation from {ip}");
            return Err(refuse(StatusCode::UNAUTHORIZED, "That invitation is wrong, or it has expired or been used."));
        };
        guests.join_limiter.succeed(ip, ticket);

        let user_agent = parts.headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
        let (token, session) = guests.sessions.create(ip, user_agent, permission);
        tracing::info!("Guest: {} joined from {ip} ({permission:?})", session.id);
        let reply = json(StatusCode::OK, &serde_json::json!({ "success": true, "permission": permission }));
        Ok(set_cookie(reply, &cookie(&token, guests.sessions.lifetime(), peer.tls)))
    }

    fn me(&self, parts: &Parts) -> Outcome {
        let guest = self.require_guest(&parts.headers)?;
        Ok(json(
            StatusCode::OK,
            &serde_json::json!({
                "id": guest.id,
                "label": guest.label,
                "permission": guest.permission,
                "can_upload": guest.permission.can_upload(),
                "needs_approval": guest.permission.needs_approval(),
                "expires_in_secs": guest.expires_in.as_secs(),
                "node_id": self.state.node_id,
                "node_name": self.state.node_name,
            }),
        ))
    }

    fn leave(&self, parts: &Parts, peer: PeerAddr) -> Outcome {
        require_same_origin(&parts.headers)?;
        let guest = self.require_guest(&parts.headers)?;
        super::revoke_guest(&self.state, &guest.id);
        tracing::info!("Guest: {} left", guest.id);
        Ok(set_cookie(json(StatusCode::OK, &serde_json::json!({ "success": true })), &cookie("", Duration::ZERO, peer.tls)))
    }

    async fn catalog(&self, parts: &Parts) -> Outcome {
        let guest = self.require_guest(&parts.headers)?;
        let wants_stream = parts.headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("text/event-stream"));
        if !wants_stream {
            return Ok(json(StatusCode::OK, &serde_json::json!({ "files": state::file_views(&self.state) })));
        }
        let slot = self
            .catalog_streams
            .clone()
            .try_acquire_owned()
            .map_err(|_| refuse(StatusCode::SERVICE_UNAVAILABLE, "too many guests are watching the catalog; try again shortly"))?;

        let (frames, receiver) = mpsc::channel::<Result<Frame<Bytes>, BoxError>>(4);
        let state = self.state.clone();
        tokio::spawn(async move {
            let _slot = slot;
            stream_catalog(&state, &guest.id, frames).await;
        });
        let stream = stream::unfold(receiver, |mut receiver| async { receiver.recv().await.map(|item| (item, receiver)) });
        let mut response = Response::new(StreamBody::new(stream).boxed_unsync());
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
        Ok(response)
    }

    async fn devices(&self, parts: &Parts) -> Outcome {
        self.require_guest(&parts.headers)?;
        Ok(json(StatusCode::OK, &serde_json::json!({ "devices": control::devices(&self.state).await })))
    }

    async fn blob(&self, parts: &Parts, id: &str) -> Outcome {
        self.require_guest(&parts.headers)?;
        if let Some(folder_id) = id.strip_suffix(".zip") {
            return Ok(self.files.download_zip(&parts.method, folder_id).await);
        }
        if !validate::is_valid_id(id) {
            return Err(refuse(StatusCode::BAD_REQUEST, "invalid file id"));
        }
        if self.state.catalog.listed(id).is_some_and(|listed| listed.entry.is_folder()) {
            return Err(refuse(StatusCode::NOT_FOUND, "this is a folder; ask for it as <id>.zip"));
        }
        let (blob, name, manifest_root) = self.files.listed_file(id).await?;
        Ok(self.files.respond_with_blob(&parts.headers, &parts.method, blob, &name, &manifest_root).await)
    }
}

// Sends the whole catalog now and again after every change, until the guest leaves or their session ends.
async fn stream_catalog(state: &NodeState, guest_id: &str, frames: mpsc::Sender<Result<Frame<Bytes>, BoxError>>) {
    let mut changes = state.catalog_changes.subscribe();
    let mut ended = state.guests.sessions.subscribe_ended();
    let mut keep_alive = tokio::time::interval_at(tokio::time::Instant::now() + KEEP_ALIVE, KEEP_ALIVE);
    let _ = frames.send(Ok(Frame::data(Bytes::from_static(b"retry: 3000\n\n")))).await;
    let mut is_stale = true;
    loop {
        if is_stale {
            changes.borrow_and_update();
            let files = serde_json::to_string(&serde_json::json!({ "files": state::file_views(state) })).unwrap_or_default();
            if frames.send(Ok(event("catalog", &files))).await.is_err() {
                return;
            }
            is_stale = false;
        }
        tokio::select! {
            changed = changes.changed() => {
                if changed.is_err() {
                    return;
                }
                // Changes come in bursts; one send covers them all.
                tokio::time::sleep(CATALOG_DEBOUNCE).await;
                is_stale = true;
            }
            ended_id = ended.recv() => match ended_id {
                Ok(id) if id != guest_id => {}
                Err(broadcast::error::RecvError::Lagged(_)) if state.guests.sessions.is_active(guest_id) => {}
                _ => {
                    let _ = frames.send(Ok(event("ended", "{}"))).await;
                    return;
                }
            },
            _ = keep_alive.tick() => {
                if !state.guests.sessions.is_active(guest_id) {
                    let _ = frames.send(Ok(event("ended", "{}"))).await;
                    return;
                }
                if frames.send(Ok(Frame::data(Bytes::from_static(b": keep-alive\n\n")))).await.is_err() {
                    return;
                }
            }
            _ = frames.closed() => return,
        }
    }
}
