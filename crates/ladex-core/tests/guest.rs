//! Browser guests over real HTTP: joining, permissions, live catalog, downloads and resumable uploads.

use std::net::SocketAddr;
use std::time::Duration;

use ladex_core::files_api::FilesApi;
use ladex_core::guest::api::GuestApi;
use ladex_core::server::{self, Chain};
use ladex_core::store::CHUNK_SIZE;
use ladex_core::testing::{spawn_node, NodeConfig, NodeHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use warp::Filter;

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&self.body)))
    }

    fn cookie(&self) -> String {
        let set = self.header("set-cookie").expect("a Set-Cookie header");
        set.split(';').next().unwrap().to_string()
    }
}

struct Site {
    addr: SocketAddr,
    node: NodeHandle,
}

async fn start_site() -> Site {
    let node = spawn_node(NodeConfig::named("host").passphrase("mesh-secret")).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let files = FilesApi::new(node.state.clone());
    let api = Chain(files.clone(), GuestApi::new(node.state.clone(), files));
    let routes = warp::service(warp::any().map(warp::reply));
    tokio::spawn(server::serve(listener, None, None, routes, api));
    Site { addr, node }
}

impl Site {
    async fn request(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        let mut stream = TcpStream::connect(self.addr).await.unwrap();
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n", self.addr);
        if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("origin")) {
            head.push_str(&format!("Origin: http://{}\r\n", self.addr));
        }
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if !body.is_empty() || matches!(method, "POST" | "PATCH") {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(b"\r\n").await.unwrap();
        stream.write_all(body).await.unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await.unwrap();
        parse_reply(&raw)
    }

    async fn host_json(&self, method: &str, path: &str, body: serde_json::Value) -> Reply {
        let bytes = if body.is_null() { Vec::new() } else { body.to_string().into_bytes() };
        self.request(method, path, &[("Content-Type", "application/json")], &bytes).await
    }

    // The invitation the host would show, as (token, code).
    async fn invite(&self, permission: &str) -> (String, String) {
        let reply = self.host_json("POST", "/api/guests/invites", serde_json::json!({ "permission": permission })).await;
        assert_eq!(reply.status, 201, "{}", String::from_utf8_lossy(&reply.body));
        let body = reply.json();
        let url = body["url"].as_str().unwrap();
        (url.split_once('#').unwrap().1.to_string(), body["code"].as_str().unwrap().to_string())
    }

    async fn join(&self, permission: &str) -> String {
        let (token, _) = self.invite(permission).await;
        let reply = self.host_json("POST", "/join", serde_json::json!({ "token": token })).await;
        assert_eq!(reply.status, 200);
        reply.cookie()
    }

    async fn as_guest(&self, cookie: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        let mut all = vec![("Cookie", cookie)];
        all.extend_from_slice(headers);
        self.request(method, path, &all, body).await
    }

    // Uploads the whole file in one PATCH; returns the upload's path and the final reply.
    async fn upload(&self, cookie: &str, name: &str, content: &[u8]) -> (String, Reply) {
        let (location, created) = self.create_upload(cookie, name, content.len()).await;
        if content.is_empty() {
            return (location, created);
        }
        let reply = self.patch(cookie, &location, 0, content).await;
        (location, reply)
    }

    async fn create_upload(&self, cookie: &str, name: &str, length: usize) -> (String, Reply) {
        let metadata = format!("filename {}", data_encoding::BASE64.encode(name.as_bytes()));
        let length = length.to_string();
        let headers = [("Tus-Resumable", "1.0.0"), ("Upload-Length", length.as_str()), ("Upload-Metadata", metadata.as_str())];
        let reply = self.as_guest(cookie, "POST", "/uploads", &headers, b"").await;
        assert_eq!(reply.status, 201, "{}", String::from_utf8_lossy(&reply.body));
        (reply.header("location").unwrap().to_string(), reply)
    }

    async fn patch(&self, cookie: &str, location: &str, offset: u64, bytes: &[u8]) -> Reply {
        let offset = offset.to_string();
        let headers = [("Tus-Resumable", "1.0.0"), ("Content-Type", "application/offset+octet-stream"), ("Upload-Offset", offset.as_str())];
        self.as_guest(cookie, "PATCH", location, &headers, bytes).await
    }

    async fn catalog(&self, cookie: &str) -> Vec<serde_json::Value> {
        let reply = self.as_guest(cookie, "GET", "/api/catalog", &[], b"").await;
        assert_eq!(reply.status, 200);
        reply.json()["files"].as_array().unwrap().clone()
    }

    async fn find_in_catalog(&self, cookie: &str, name: &str) -> Option<serde_json::Value> {
        self.catalog(cookie).await.into_iter().find(|file| file["name"] == name)
    }
}

fn parse_reply(raw: &[u8]) -> Reply {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("a complete header");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut lines = head.lines();
    let status = lines.next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
    let headers = lines.filter_map(|line| line.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    Reply { status, headers, body: raw[split + 4..].to_vec() }
}

fn sample_bytes(length: usize) -> Vec<u8> {
    (0..length).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn a_token_admits_one_guest_once_and_sets_a_locked_down_cookie() {
    let site = start_site().await;
    let (token, _) = site.invite("read-only").await;
    let joined = site.host_json("POST", "/join", serde_json::json!({ "token": token })).await;
    assert_eq!(joined.status, 200);
    let set_cookie = joined.header("set-cookie").unwrap();
    assert!(set_cookie.starts_with("guest=") && set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"));

    let again = site.host_json("POST", "/join", serde_json::json!({ "token": token })).await;
    assert_eq!(again.status, 401);
}

#[tokio::test]
async fn the_typed_code_works_and_wrong_codes_are_throttled() {
    let site = start_site().await;
    let (_, code) = site.invite("read-only").await;
    // Five wrong codes are free; the sixth is still judged but earns the lockout.
    for _ in 0..6 {
        let wrong = site.host_json("POST", "/join", serde_json::json!({ "code": "AAAA-AAAA" })).await;
        assert_eq!(wrong.status, 401);
    }
    let locked = site.host_json("POST", "/join", serde_json::json!({ "code": code })).await;
    assert_eq!(locked.status, 429);
    assert!(locked.header("retry-after").is_some());
}

#[tokio::test]
async fn the_typed_code_admits_a_guest() {
    let site = start_site().await;
    let (_, code) = site.invite("upload").await;
    let joined = site.host_json("POST", "/join", serde_json::json!({ "code": code.to_lowercase() })).await;
    assert_eq!(joined.status, 200);
    assert_eq!(joined.json()["permission"], "upload");
}

#[tokio::test]
async fn joining_from_another_origin_is_refused() {
    let site = start_site().await;
    let (token, _) = site.invite("read-only").await;
    let body = serde_json::json!({ "token": token }).to_string();
    let reply = site.request("POST", "/join", &[("Origin", "https://evil.example")], body.as_bytes()).await;
    assert_eq!(reply.status, 403);
}

#[tokio::test]
async fn nothing_but_the_join_page_is_open_without_a_session() {
    let site = start_site().await;
    assert_eq!(site.request("GET", "/join", &[], b"").await.status, 200);
    for path in ["/api/catalog", "/api/devices", "/api/me", "/blobs/anything"] {
        assert_eq!(site.request("GET", path, &[], b"").await.status, 401, "{path}");
    }
    let created = site.request("POST", "/uploads", &[("Tus-Resumable", "1.0.0"), ("Upload-Length", "1")], b"").await;
    assert_eq!(created.status, 401);
}

#[tokio::test]
async fn the_join_page_shows_the_fingerprint_and_blocks_referrers() {
    let site = start_site().await;
    let page = site.request("GET", "/join", &[], b"").await;
    assert_eq!(page.header("referrer-policy"), Some("no-referrer"));
    assert!(page.header("content-security-policy").unwrap().contains("default-src 'none'"));
    assert!(String::from_utf8_lossy(&page.body).contains("not using encryption"));
}

#[tokio::test]
async fn a_guest_sees_who_they_are_and_what_they_may_do() {
    let site = start_site().await;
    let cookie = site.join("upload-with-approval").await;
    let me = site.as_guest(&cookie, "GET", "/api/me", &[], b"").await.json();
    assert_eq!(me["permission"], "upload-with-approval");
    assert_eq!((me["can_upload"].as_bool(), me["needs_approval"].as_bool()), (Some(true), Some(true)));
}

#[tokio::test]
async fn a_read_only_guest_cannot_upload() {
    let site = start_site().await;
    let cookie = site.join("read-only").await;
    let headers = [("Tus-Resumable", "1.0.0"), ("Upload-Length", "10")];
    let reply = site.as_guest(&cookie, "POST", "/uploads", &headers, b"").await;
    assert_eq!(reply.status, 403);
}

#[tokio::test]
async fn an_uploaded_file_is_listed_and_downloads_with_ranges() {
    let site = start_site().await;
    let cookie = site.join("upload").await;
    let content = sample_bytes(CHUNK_SIZE as usize * 2 + 1234);

    let (_, done) = site.upload(&cookie, "photo.bin", &content).await;
    assert_eq!(done.status, 204);
    assert_eq!(done.header("upload-offset"), Some(content.len().to_string().as_str()));
    assert_eq!(done.header("ladex-upload-state"), Some("shared"));

    let listed = site.find_in_catalog(&cookie, "photo.bin").await.expect("the upload is in the catalog");
    let id = listed["id"].as_str().unwrap();
    let whole = site.as_guest(&cookie, "GET", &format!("/blobs/{id}"), &[], b"").await;
    assert_eq!((whole.status, whole.body.len()), (200, content.len()));
    assert_eq!(whole.body, content);
    assert!(whole.header("content-disposition").unwrap().starts_with("attachment"));

    let from = CHUNK_SIZE as usize - 10;
    let range = format!("bytes={from}-{}", from + 99);
    let part = site.as_guest(&cookie, "GET", &format!("/blobs/{id}"), &[("Range", &range)], b"").await;
    assert_eq!(part.status, 206);
    assert_eq!(part.body, &content[from..from + 100]);
    assert_eq!(part.header("content-range"), Some(format!("bytes {from}-{}/{}", from + 99, content.len()).as_str()));
}

#[tokio::test]
async fn an_interrupted_upload_resumes_from_the_confirmed_offset() {
    let site = start_site().await;
    let cookie = site.join("upload").await;
    let content = sample_bytes(CHUNK_SIZE as usize * 3);
    let (location, _) = site.create_upload(&cookie, "big.bin", content.len()).await;

    let first = site.patch(&cookie, &location, 0, &content[..CHUNK_SIZE as usize + 500]).await;
    assert_eq!(first.status, 204);
    assert_eq!(first.header("upload-offset"), Some(CHUNK_SIZE.to_string().as_str()), "only whole chunks are confirmed");

    let head = site.as_guest(&cookie, "HEAD", &location, &[("Tus-Resumable", "1.0.0")], b"").await;
    assert_eq!(head.header("upload-offset"), Some(CHUNK_SIZE.to_string().as_str()));
    assert_eq!(head.header("upload-length"), Some(content.len().to_string().as_str()));

    let skipped = site.patch(&cookie, &location, CHUNK_SIZE * 2, &content[CHUNK_SIZE as usize * 2..]).await;
    assert_eq!(skipped.status, 409);
    assert_eq!(skipped.header("upload-offset"), Some(CHUNK_SIZE.to_string().as_str()));

    let rest = site.patch(&cookie, &location, CHUNK_SIZE, &content[CHUNK_SIZE as usize..]).await;
    assert_eq!((rest.status, rest.header("ladex-upload-state")), (204, Some("shared")));

    let id = site.find_in_catalog(&cookie, "big.bin").await.unwrap()["id"].as_str().unwrap().to_string();
    let downloaded = site.as_guest(&cookie, "GET", &format!("/blobs/{id}"), &[], b"").await;
    assert_eq!(downloaded.body, content);
}

#[tokio::test]
async fn an_empty_file_is_shared_as_soon_as_it_is_created() {
    let site = start_site().await;
    let cookie = site.join("upload").await;
    let (_, created) = site.upload(&cookie, "empty.txt", b"").await;
    assert_eq!(created.header("ladex-upload-state"), Some("shared"));
    assert!(site.find_in_catalog(&cookie, "empty.txt").await.is_some());
}

#[tokio::test]
async fn an_upload_that_needs_approval_is_not_listed_until_the_host_accepts_it() {
    let site = start_site().await;
    let cookie = site.join("upload-with-approval").await;
    let (_, done) = site.upload(&cookie, "report.txt", b"quarterly numbers").await;
    assert_eq!(done.header("ladex-upload-state"), Some("awaiting-approval"));
    assert!(site.find_in_catalog(&cookie, "report.txt").await.is_none());

    let waiting = site.host_json("GET", "/api/guests/uploads", serde_json::Value::Null).await.json();
    let upload = &waiting["uploads"][0];
    assert_eq!((upload["name"].as_str(), upload["size"].as_u64()), (Some("report.txt"), Some(17)));

    let id = upload["id"].as_str().unwrap();
    let approved = site.host_json("POST", &format!("/api/guests/uploads/{id}/approve"), serde_json::Value::Null).await;
    assert_eq!(approved.status, 200);
    let listed = site.find_in_catalog(&cookie, "report.txt").await.expect("listed after approval");
    let download = site.as_guest(&cookie, "GET", &format!("/blobs/{}", listed["id"].as_str().unwrap()), &[], b"").await;
    assert_eq!(download.body, b"quarterly numbers");

    let again = site.host_json("POST", &format!("/api/guests/uploads/{id}/approve"), serde_json::Value::Null).await;
    assert_eq!(again.status, 404);
}

#[tokio::test]
async fn a_declined_upload_is_deleted() {
    let site = start_site().await;
    let cookie = site.join("upload-with-approval").await;
    let (location, _) = site.upload(&cookie, "no.txt", b"not wanted").await;
    let id = location.trim_start_matches("/uploads/").to_string();

    let denied = site.host_json("POST", &format!("/api/guests/uploads/{id}/deny"), serde_json::Value::Null).await;
    assert_eq!(denied.status, 200);
    assert!(site.node.state.store.get(&id).is_none());
    assert!(site.find_in_catalog(&cookie, "no.txt").await.is_none());
    let head = site.as_guest(&cookie, "HEAD", &location, &[("Tus-Resumable", "1.0.0")], b"").await;
    assert_eq!(head.status, 404);
}

#[tokio::test]
async fn a_guest_cannot_touch_another_guests_upload() {
    let site = start_site().await;
    let owner = site.join("upload").await;
    let other = site.join("upload").await;
    let (location, _) = site.create_upload(&owner, "mine.bin", 10).await;
    let head = site.as_guest(&other, "HEAD", &location, &[("Tus-Resumable", "1.0.0")], b"").await;
    assert_eq!(head.status, 404);
    assert_eq!(site.patch(&other, &location, 0, b"0123456789").await.status, 404);
}

#[tokio::test]
async fn signing_a_guest_out_ends_their_session_and_drops_their_unfinished_uploads() {
    let site = start_site().await;
    let cookie = site.join("upload").await;
    let (location, _) = site.create_upload(&cookie, "half.bin", 100).await;
    let id = location.trim_start_matches("/uploads/").to_string();
    let guests = site.host_json("GET", "/api/guests/sessions", serde_json::Value::Null).await.json();
    let guest_id = guests["guests"][0]["id"].as_str().unwrap().to_string();

    let signed_out = site.host_json("DELETE", &format!("/api/guests/sessions/{guest_id}"), serde_json::Value::Null).await;
    assert_eq!(signed_out.status, 200);
    assert_eq!(site.as_guest(&cookie, "GET", "/api/me", &[], b"").await.status, 401);
    assert!(site.node.state.store.get(&id).is_none());
}

#[tokio::test]
async fn the_catalog_streams_changes_and_ends_when_the_guest_is_signed_out() {
    let site = start_site().await;
    let cookie = site.join("upload").await;
    let uploader = site.join("upload").await;

    let mut stream = TcpStream::connect(site.addr).await.unwrap();
    let request = format!("GET /api/catalog HTTP/1.1\r\nHost: {}\r\nCookie: {cookie}\r\nAccept: text/event-stream\r\n\r\n", site.addr);
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut seen = String::new();
    read_until(&mut stream, &mut seen, "event: catalog").await;
    assert!(seen.contains("text/event-stream"));

    site.upload(&uploader, "live.txt", b"hello").await;
    read_until(&mut stream, &mut seen, "live.txt").await;

    let guests = site.host_json("GET", "/api/guests/sessions", serde_json::Value::Null).await.json();
    let first = guests["guests"][0]["id"].as_str().unwrap().to_string();
    site.host_json("DELETE", &format!("/api/guests/sessions/{first}"), serde_json::Value::Null).await;
    read_until(&mut stream, &mut seen, "event: ended").await;
}

async fn read_until(stream: &mut TcpStream, seen: &mut String, needle: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut buffer = [0u8; 8192];
    while !seen.contains(needle) {
        let read =
            tokio::time::timeout_at(deadline, stream.read(&mut buffer)).await.unwrap_or_else(|_| panic!("never saw {needle:?} in {seen}"));
        let count = read.unwrap();
        assert!(count > 0, "the stream closed before {needle:?}; saw {seen}");
        seen.push_str(&String::from_utf8_lossy(&buffer[..count]));
    }
}

#[tokio::test]
async fn devices_are_listed_for_guests() {
    let site = start_site().await;
    let cookie = site.join("read-only").await;
    let reply = site.as_guest(&cookie, "GET", "/api/devices", &[], b"").await;
    assert_eq!(reply.status, 200);
    assert!(reply.json()["devices"].is_array());
}

#[tokio::test]
async fn a_folder_asked_for_as_a_file_points_to_the_zip() {
    let site = start_site().await;
    let cookie = site.join("read-only").await;
    let unknown = site.as_guest(&cookie, "GET", "/blobs/not_a_file", &[], b"").await;
    assert_eq!(unknown.status, 404);
}

#[tokio::test]
async fn uploads_need_the_tus_version_header() {
    let site = start_site().await;
    let cookie = site.join("upload").await;
    let reply = site.as_guest(&cookie, "POST", "/uploads", &[("Upload-Length", "10")], b"").await;
    assert_eq!(reply.status, 412);
    assert_eq!(reply.header("tus-version"), Some("1.0.0"));
}
