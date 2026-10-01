// Validation and sanitizing of everything that arrives from outside this node:
// browser tabs on /ws and other nodes on /mesh.
//
// The file-name rules here are mirrored by static/receive-policy.js, which
// sanitizes names again on the receiving browser (the node can't be trusted to
// have done it). Both are tested against tests/filename_cases.json so they
// can't drift apart.

use crate::hlc::wall_clock_ms;
use crate::types::*;

// Largest integer a browser can represent exactly; sizes above it can't be shown or tracked.
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

const MAX_NAME_BYTES: usize = 255;
const MAX_EXTENSION_BYTES: usize = 16;
const MAX_ID_LEN: usize = 64;
const MAX_MESSAGE_ID_LEN: usize = 128;
const MAX_MIME_LEN: usize = 255;
const MAX_HOSTS: usize = 256;
pub const MAX_USER_AGENT_CHARS: usize = 256;
pub const MAX_NICKNAME_CHARS: usize = 60;
pub const MAX_NODE_NAME_CHARS: usize = 128;
pub const MAX_MESSAGE_CHARS: usize = 4000;

const WINDOWS_RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

// Control characters plus invisible and bidirectional-override characters
// that let a name like "photo\u{202E}gpj.exe" display as "photoexe.jpg".
fn is_stripped(c: char) -> bool {
    matches!(c,
        '\u{0}'..='\u{1F}' | '\u{7F}'..='\u{9F}' | '\u{AD}' | '\u{61C}' | '\u{200B}'..='\u{200F}'
        | '\u{2028}' | '\u{2029}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
        | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}')
}

fn is_forbidden_in_names(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
}

// Safe as a single file name on Windows, macOS and Linux.
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !is_stripped(*c))
        .map(|c| if is_forbidden_in_names(c) { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim_start_matches(' ').trim_end_matches([' ', '.']);
    let mut name = if trimmed.is_empty() { "unnamed".to_string() } else { trimmed.to_string() };

    // Windows treats "CON.txt" as the device too, so only the part before the first dot counts.
    let base = name.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
    if WINDOWS_RESERVED.contains(&base.as_str()) {
        name.insert(0, '_');
    }
    truncate_to_bytes(&name)
}

fn truncate_to_bytes(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_string();
    }
    let extension = match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= MAX_EXTENSION_BYTES => &name[i..],
        _ => "",
    };
    let stem = &name[..name.len() - extension.len()];
    let mut kept = String::new();
    for ch in stem.chars() {
        if kept.len() + ch.len_utf8() + extension.len() > MAX_NAME_BYTES {
            break;
        }
        kept.push(ch);
    }
    let kept = kept.trim_end_matches([' ', '.']);
    format!("{}{extension}", if kept.is_empty() { "unnamed" } else { kept })
}

pub fn is_valid_id(s: &str) -> bool {
    is_valid_id_up_to(s, MAX_ID_LEN)
}

fn is_valid_id_up_to(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn is_valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn sanitize_mime(mime: &str) -> String {
    const FALLBACK: &str = "application/octet-stream";
    let is_token = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&b))
    };
    match mime.split_once('/') {
        Some((kind, subtype)) if mime.len() <= MAX_MIME_LEN && is_token(kind) && is_token(subtype) => mime.to_string(),
        _ => FALLBACK.to_string(),
    }
}

// At most `max` characters (not bytes), with control and invisible characters removed.
pub fn clean_label(s: &str, max: usize) -> String {
    s.chars().filter(|c| !is_stripped(*c)).take(max).collect::<String>().trim().to_string()
}

// Chat text keeps newlines and tabs; returns None when nothing is left.
pub fn clean_message(s: &str) -> Option<String> {
    let cleaned: String = s
        .chars()
        .filter(|c| matches!(c, '\n' | '\t') || !is_stripped(*c))
        .take(MAX_MESSAGE_CHARS)
        .collect();
    let cleaned = cleaned.trim().to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

// What a browser tab may say about a file it shares; everything else in the
// catalog entry is decided by the node.
pub fn file_request(mut req: FileUploadRequest) -> Result<FileUploadRequest, &'static str> {
    if !is_valid_id(&req.id) {
        return Err("invalid file id");
    }
    if req.size > MAX_SAFE_INTEGER {
        return Err("file size is out of range");
    }
    req.name = sanitize_file_name(&req.name);
    req.mime_type = sanitize_mime(&req.mime_type);
    req.sha256 = match req.sha256 {
        Some(hash) if is_valid_sha256(&hash) => Some(hash),
        Some(_) => return Err("invalid checksum"),
        None => None,
    };
    Ok(req)
}

// Entries from another node. Names and the like are cleaned rather than
// rejected so one odd name doesn't hide the file.
pub fn incoming_file(mut file: FileMetadata) -> Option<FileMetadata> {
    if !is_valid_id(&file.id) || !is_valid_id(&file.uploader_id) || file.size > MAX_SAFE_INTEGER || !file.version.is_set() {
        return None;
    }
    if file.hosts.len() > MAX_HOSTS {
        return None;
    }
    file.hosts.retain(|host| is_valid_id(host));
    file.name = sanitize_file_name(&file.name);
    file.mime_type = sanitize_mime(&file.mime_type);
    file.sha256 = file.sha256.filter(|hash| is_valid_sha256(hash));
    file.uploaded_at = file.uploaded_at.min(chrono::Utc::now());
    Some(file)
}

pub fn incoming_peer(mut peer: PeerInfo) -> Option<PeerInfo> {
    if !is_valid_id(&peer.session_id) || !peer.version.is_set() {
        return None;
    }
    if peer.hosting_node_id.as_deref().is_some_and(|id| !is_valid_id(id)) {
        return None;
    }
    peer.user_agent = peer.user_agent.map(|ua| clean_label(&ua, MAX_USER_AGENT_CHARS));
    peer.nickname = peer.nickname.map(|n| clean_label(&n, MAX_NICKNAME_CHARS)).filter(|n| !n.is_empty());
    peer.hosting_node_name = peer.hosting_node_name.map(|n| clean_label(&n, MAX_NODE_NAME_CHARS));
    peer.node_rtt_ms = None;
    Some(peer)
}

// A timestamp in the future would pin the message to the top of every list.
pub fn incoming_message(mut message: TextMessage) -> Option<TextMessage> {
    if !is_valid_id_up_to(&message.id, MAX_MESSAGE_ID_LEN) || !is_valid_id(&message.sender_id) {
        return None;
    }
    message.content = clean_message(&message.content)?;
    message.sender_name = message.sender_name.map(|n| clean_label(&n, MAX_NICKNAME_CHARS));
    message.created_at = message.created_at.min(wall_clock_ms());
    message.timestamp = message.timestamp.min(chrono::Utc::now());
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hlc::Stamp;

    #[derive(serde::Deserialize)]
    struct Case {
        input: String,
        expected: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct Cases {
        file_names: Vec<Case>,
    }

    #[test]
    fn file_names_match_the_shared_fixture() {
        let cases: Cases = serde_json::from_str(include_str!("../tests/filename_cases.json")).unwrap();
        for case in cases.file_names {
            assert_eq!(sanitize_file_name(&case.input), case.expected.unwrap(), "input: {:?}", case.input);
        }
    }

    #[test]
    fn long_names_are_cut_to_255_bytes_and_keep_their_extension() {
        let name = sanitize_file_name(&format!("{}.mp4", "a".repeat(400)));
        assert_eq!(name.len(), 255);
        assert!(name.ends_with(".mp4"));

        // Multi-byte characters are never split.
        let name = sanitize_file_name(&"é".repeat(300));
        assert!(name.len() <= 255 && name.chars().all(|c| c == 'é'));

        // An absurdly long "extension" is just part of the name.
        let name = sanitize_file_name(&format!("a.{}", "b".repeat(300)));
        assert!(name.len() <= 255);
    }

    #[test]
    fn sanitizing_is_idempotent() {
        for input in ["con.txt", "a\\b/c", "trailing. .", "photo\u{202E}gpj.exe", ""] {
            let once = sanitize_file_name(input);
            assert_eq!(sanitize_file_name(&once), once, "input: {input:?}");
        }
    }

    #[test]
    fn ids_and_hashes() {
        assert!(is_valid_id("file_abc123xyz_1700000000000"));
        assert!(is_valid_id("peer_k3j2h1g0f_1700000000000"));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("has space"));
        assert!(!is_valid_id("<script>"));
        assert!(!is_valid_id(&"a".repeat(65)));
        assert!(is_valid_sha256(&"ab01".repeat(16)));
        assert!(!is_valid_sha256(&"AB01".repeat(16)));
        assert!(!is_valid_sha256("abc"));
    }

    #[test]
    fn mime_types_fall_back_to_octet_stream() {
        assert_eq!(sanitize_mime("image/png"), "image/png");
        assert_eq!(sanitize_mime("inode/directory"), "inode/directory");
        assert_eq!(sanitize_mime("application/vnd.ms-excel"), "application/vnd.ms-excel");
        for bad in ["", "png", "text/<script>", "a/b/c", "text/", "/plain", &"a/".repeat(200)] {
            assert_eq!(sanitize_mime(bad), "application/octet-stream", "input: {bad:?}");
        }
    }

    #[test]
    fn messages_keep_newlines_but_lose_control_characters() {
        assert_eq!(clean_message("hi\nthere\u{0}\u{202E}").as_deref(), Some("hi\nthere"));
        assert_eq!(clean_message("  \n\t "), None);
        assert_eq!(clean_message(&"x".repeat(5000)).unwrap().chars().count(), MAX_MESSAGE_CHARS);
    }

    fn request(id: &str, size: u64, sha256: Option<&str>) -> FileUploadRequest {
        FileUploadRequest {
            id: id.into(),
            name: "../evil/CON.txt".into(),
            size,
            mime_type: "bad".into(),
            sha256: sha256.map(String::from),
            is_folder: false,
        }
    }

    #[test]
    fn file_requests_are_sanitized_or_rejected() {
        let ok = file_request(request("file_1", 10, None)).unwrap();
        assert_eq!(ok.name, ".._evil_CON.txt");
        assert_eq!(ok.mime_type, "application/octet-stream");

        assert!(file_request(request("bad id", 10, None)).is_err());
        assert!(file_request(request("file_1", MAX_SAFE_INTEGER + 1, None)).is_err());
        assert!(file_request(request("file_1", 10, Some("nothex"))).is_err());
        assert!(file_request(request("file_1", 10, Some(&"a".repeat(64)))).is_ok());
    }

    fn stamp() -> Stamp {
        Stamp { wall: 1, counter: 0, node: "node_a".into() }
    }

    fn incoming() -> FileMetadata {
        FileMetadata {
            id: "file_1".into(),
            name: "a/b.txt".into(),
            size: 5,
            mime_type: "text/plain".into(),
            uploader_id: "peer_1".into(),
            hosts: ["peer_1".to_string(), "bad host".to_string()].into(),
            uploaded_at: chrono::Utc::now() + chrono::Duration::days(30),
            created_at: 0,
            deleted: false,
            deleted_at: 0,
            version: stamp(),
            sha256: Some("nothex".into()),
            is_folder: false,
        }
    }

    #[test]
    fn incoming_files_from_other_nodes_are_cleaned() {
        let file = incoming_file(incoming()).unwrap();
        assert_eq!(file.name, "a_b.txt");
        assert_eq!(file.hosts, ["peer_1".to_string()].into());
        assert_eq!(file.sha256, None);
        assert!(file.uploaded_at <= chrono::Utc::now());
    }

    #[test]
    fn incoming_files_without_a_stamp_or_with_bad_ids_are_dropped() {
        let mut f = incoming();
        f.version = Stamp::default();
        assert!(incoming_file(f).is_none());

        let mut f = incoming();
        f.id = "../x".into();
        assert!(incoming_file(f).is_none());

        let mut f = incoming();
        f.size = u64::MAX;
        assert!(incoming_file(f).is_none());

        let mut f = incoming();
        f.hosts = (0..300).map(|i| format!("peer_{i}")).collect();
        assert!(incoming_file(f).is_none());
    }
}
