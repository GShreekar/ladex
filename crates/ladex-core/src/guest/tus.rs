//! Resumable guest uploads over the tus protocol (core, creation and termination): POST /uploads, then HEAD and PATCH /uploads/:id.

use std::collections::HashMap;

use data_encoding::{BASE64, BASE64_NOPAD};
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::http::request::Parts;
use hyper::{Response, StatusCode};

use super::api::{refuse, require_same_origin, GuestApi, Outcome, Refusal};
use super::sessions::GuestSession;
use super::uploads::{new_upload_id, Upload, UploadState};
use super::{share_upload, Permission};
use crate::files_api::{header_str, header_u64, store_error, write_body};
use crate::server::ServerBody;
use crate::store::{Blob, Origin, CHUNK_SIZE};
use crate::{state, validate};

const TUS_VERSION: &str = "1.0.0";
const OFFSET_CONTENT_TYPE: &str = "application/offset+octet-stream";
const STATE_HEADER: &str = "ladex-upload-state";

// `key base64value,key2 base64value2`; a key may come with no value.
fn parse_metadata(header_value: &str) -> HashMap<String, String> {
    let decode = |encoded: &str| {
        let bytes = BASE64.decode(encoded.as_bytes()).or_else(|_| BASE64_NOPAD.decode(encoded.as_bytes())).ok()?;
        String::from_utf8(bytes).ok()
    };
    header_value
        .split(',')
        .filter_map(|pair| {
            let mut words = pair.split_whitespace();
            let key = words.next()?.to_string();
            let value = words.next().and_then(decode).unwrap_or_default();
            Some((key, value))
        })
        .collect()
}

fn with_tus(mut response: Response<ServerBody>) -> Response<ServerBody> {
    response.headers_mut().insert("tus-resumable", HeaderValue::from_static(TUS_VERSION));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn tus_reply(status: StatusCode, offset: u64, state: UploadState) -> Response<ServerBody> {
    let mut response = with_tus(Response::new(crate::files_api::empty()));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert("upload-offset", HeaderValue::from(offset));
    headers.insert(STATE_HEADER, HeaderValue::from_static(state.as_str()));
    response
}

fn require_tus_version(headers: &HeaderMap) -> Result<(), Refusal> {
    if header_str(headers, "tus-resumable") == Some(TUS_VERSION) {
        return Ok(());
    }
    let mut response = refuse(StatusCode::PRECONDITION_FAILED, "unsupported tus version");
    response.headers_mut().insert("tus-version", HeaderValue::from_static(TUS_VERSION));
    Err(response)
}

// How far an upload can continue from: whole chunks only, because a chunk is stored and verified as a unit.
fn confirmed_offset(blob: &Blob) -> u64 {
    (blob.leading_chunks() as u64 * CHUNK_SIZE).min(blob.size())
}

impl GuestApi {
    pub(super) fn upload_options(&self) -> Response<ServerBody> {
        let mut response = tus_reply_empty(StatusCode::NO_CONTENT);
        let headers = response.headers_mut();
        headers.insert("tus-version", HeaderValue::from_static(TUS_VERSION));
        headers.insert("tus-extension", HeaderValue::from_static("creation,termination"));
        headers.insert("tus-max-size", HeaderValue::from(validate::MAX_SAFE_INTEGER));
        response
    }

    pub(super) async fn create_upload(&self, parts: &Parts) -> Outcome {
        require_same_origin(&parts.headers)?;
        let guest = self.require_guest(&parts.headers)?;
        require_tus_version(&parts.headers)?;
        if !guest.permission.can_upload() {
            return Err(refuse(StatusCode::FORBIDDEN, "this invitation does not allow uploads"));
        }
        let size = header_u64(&parts.headers, "upload-length")
            .filter(|size| *size <= validate::MAX_SAFE_INTEGER)
            .ok_or_else(|| refuse(StatusCode::BAD_REQUEST, "missing or invalid Upload-Length"))?;
        let metadata = parse_metadata(header_str(&parts.headers, "upload-metadata").unwrap_or(""));
        let name = metadata.get("filename").map_or_else(|| "unnamed".to_string(), |name| validate::sanitize_file_name(name));
        let mime_type = validate::sanitize_mime(metadata.get("filetype").map_or("", String::as_str));

        let state = &self.state;
        let id = new_upload_id();
        state
            .guests
            .uploads
            .begin(&id, &guest.id, name, mime_type, size)
            .map_err(|refused| refuse(StatusCode::TOO_MANY_REQUESTS, &refused.to_string()))?;
        state::make_room(state, size).await;
        let blob = state.store.create_with(&id, size, Origin::Published).map_err(|e| {
            state.guests.uploads.remove(&id);
            Box::new(store_error(e))
        })?;

        let mut upload_state = UploadState::Receiving;
        if size == 0 {
            let upload = state.guests.uploads.get(&id).expect("the upload was just recorded");
            upload_state = self.finish_upload(&upload, &blob, guest.permission).await?;
        }
        let mut response = tus_reply(StatusCode::CREATED, 0, upload_state);
        response.headers_mut().insert(header::LOCATION, HeaderValue::from_str(&format!("/uploads/{id}")).expect("an id is plain ASCII"));
        Ok(response)
    }

    pub(super) fn upload_status(&self, parts: &Parts, id: &str) -> Outcome {
        let guest = self.require_guest(&parts.headers)?;
        let (upload, blob) = self.owned_upload(&guest, id)?;
        let offset = if upload.state == UploadState::Receiving { confirmed_offset(&blob) } else { upload.size };
        let mut response = tus_reply(StatusCode::OK, offset, upload.state);
        response.headers_mut().insert("upload-length", HeaderValue::from(upload.size));
        Ok(response)
    }

    pub(super) async fn append_to_upload(&self, parts: &Parts, id: &str, body: &mut Option<Incoming>) -> Outcome {
        require_same_origin(&parts.headers)?;
        let guest = self.require_guest(&parts.headers)?;
        require_tus_version(&parts.headers)?;
        if header_str(&parts.headers, "content-type") != Some(OFFSET_CONTENT_TYPE) {
            return Err(refuse(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Content-Type must be application/offset+octet-stream"));
        }
        let offset = header_u64(&parts.headers, "upload-offset").ok_or_else(|| refuse(StatusCode::BAD_REQUEST, "missing Upload-Offset"))?;
        let (upload, blob) = self.owned_upload(&guest, id)?;
        if upload.state != UploadState::Receiving {
            return Err(refuse(StatusCode::CONFLICT, "this upload is already complete"));
        }

        let resume_at = confirmed_offset(&blob);
        if offset != resume_at {
            let mut refusal = refuse(StatusCode::CONFLICT, "Upload-Offset does not match the server's; ask for it with HEAD");
            refusal.headers_mut().insert("upload-offset", HeaderValue::from(resume_at));
            return Err(refusal);
        }
        if header_u64(&parts.headers, "content-length").is_some_and(|length| length > upload.size - offset) {
            return Err(refuse(StatusCode::BAD_REQUEST, "more data than announced"));
        }
        let _slot = self
            .files
            .uploads
            .try_acquire()
            .map_err(|_| refuse(StatusCode::SERVICE_UNAVAILABLE, "too many uploads in progress; try again shortly"))?;
        let _writer =
            blob.try_write_lock().ok_or_else(|| refuse(StatusCode::CONFLICT, "this upload is being written by another request"))?;

        let incoming = body.take().ok_or_else(|| refuse(StatusCode::INTERNAL_SERVER_ERROR, "no request body"))?;
        // Bytes short of a whole chunk are not kept; the offset in the answer tells the client where to carry on.
        write_body(&blob, upload.size, offset, incoming).await?;

        if !self.state.guests.sessions.is_active(&guest.id) {
            self.state.guests.uploads.remove(id);
            self.state.store.remove(id);
            return Err(refuse(StatusCode::UNAUTHORIZED, "your session has ended"));
        }
        if !blob.bitmap().is_full() {
            return Ok(tus_reply(StatusCode::NO_CONTENT, confirmed_offset(&blob), UploadState::Receiving));
        }
        let final_state = self.finish_upload(&upload, &blob, guest.permission).await?;
        Ok(tus_reply(StatusCode::NO_CONTENT, upload.size, final_state))
    }

    pub(super) fn cancel_upload(&self, parts: &Parts, id: &str) -> Outcome {
        require_same_origin(&parts.headers)?;
        let guest = self.require_guest(&parts.headers)?;
        let (upload, blob) = self.owned_upload(&guest, id)?;
        if upload.state == UploadState::Shared {
            return Err(refuse(StatusCode::CONFLICT, "this upload is already shared"));
        }
        let _writer =
            blob.try_write_lock().ok_or_else(|| refuse(StatusCode::CONFLICT, "this upload is being written by another request"))?;
        self.state.guests.uploads.remove(id);
        self.state.store.remove(id);
        Ok(tus_reply_empty(StatusCode::NO_CONTENT))
    }

    // Another guest's upload looks exactly like one that does not exist.
    fn owned_upload(&self, guest: &GuestSession, id: &str) -> Result<(Upload, std::sync::Arc<Blob>), Refusal> {
        let not_found = || refuse(StatusCode::NOT_FOUND, "no such upload");
        let upload = self.state.guests.uploads.get(id).filter(|upload| upload.guest_id == guest.id).ok_or_else(not_found)?;
        let blob = self.state.store.get(id).ok_or_else(not_found)?;
        Ok((upload, blob))
    }

    // Seals a complete upload and either lists it or holds it for the host, as the guest's permission says.
    async fn finish_upload(&self, upload: &Upload, blob: &Blob, permission: Permission) -> Result<UploadState, Refusal> {
        let state = &self.state;
        let discard = || {
            state.guests.uploads.remove(&upload.id);
            state.store.remove(&upload.id);
        };
        if let Err(why) = blob.seal().await {
            discard();
            return Err(refuse(StatusCode::INTERNAL_SERVER_ERROR, why));
        }
        if permission.needs_approval() {
            state.guests.uploads.transition(&upload.id, UploadState::Receiving, UploadState::AwaitingApproval);
            tracing::info!("Guest: {} ({} bytes) from {} waits for approval", upload.name, upload.size, upload.guest_id);
            return Ok(UploadState::AwaitingApproval);
        }
        match share_upload(state, upload, blob).await {
            Ok(view) => {
                state.guests.uploads.transition(&upload.id, UploadState::Receiving, UploadState::Shared);
                tracing::info!("Guest: {} ({} bytes) shared by {}", view.name, view.size, upload.guest_id);
                Ok(UploadState::Shared)
            }
            Err(e) => {
                discard();
                Err(refuse(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
            }
        }
    }
}

fn tus_reply_empty(status: StatusCode) -> Response<ServerBody> {
    let mut response = with_tus(Response::new(crate::files_api::empty()));
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(text: &str) -> String {
        BASE64.encode(text.as_bytes())
    }

    #[test]
    fn metadata_values_are_base64_text() {
        let header = format!("filename {},filetype {}", encoded("holiday ñ.jpg"), encoded("image/jpeg"));
        let metadata = parse_metadata(&header);
        assert_eq!(metadata["filename"], "holiday ñ.jpg");
        assert_eq!(metadata["filetype"], "image/jpeg");
    }

    #[test]
    fn metadata_with_a_bare_key_or_bad_base64_is_tolerated() {
        let metadata = parse_metadata("is_confidential,filename !!!not-base64!!!");
        assert_eq!(metadata["is_confidential"], "");
        assert_eq!(metadata["filename"], "");
    }

    #[test]
    fn a_missing_or_wrong_tus_version_is_refused_with_the_supported_one() {
        let mut headers = HeaderMap::new();
        assert_eq!(require_tus_version(&headers).unwrap_err().status(), StatusCode::PRECONDITION_FAILED);
        headers.insert("tus-resumable", HeaderValue::from_static("0.2.2"));
        let refusal = require_tus_version(&headers).unwrap_err();
        assert_eq!(refusal.headers()["tus-version"], TUS_VERSION);
        headers.insert("tus-resumable", HeaderValue::from_static(TUS_VERSION));
        assert!(require_tus_version(&headers).is_ok());
    }
}
