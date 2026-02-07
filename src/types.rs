use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

pub type SessionId = String;

/// Per-peer sender channel — each connected WebSocket gets its own mpsc sender
/// so the server can route messages to a specific peer instead of broadcasting.
pub type PeerSender = mpsc::UnboundedSender<ServerMessage>;
pub type PeerSenders = Arc<RwLock<HashMap<SessionId, PeerSender>>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub session_id: SessionId,
    pub connected_at: chrono::DateTime<chrono::Utc>,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthRequest {
    pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResponse {
    pub success: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub mime_type: String,
    pub uploader_id: SessionId,
    pub hosts: HashSet<SessionId>,
    pub uploaded_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextMessage {
    pub id: String,
    pub content: String,
    pub sender_id: SessionId,
    pub sender_name: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

// ---------------------------------------------------------------------------
// Client → Server messages
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    /// Peer announces itself to the network
    #[serde(rename = "join")]
    Join {
        session_id: SessionId,
        user_agent: Option<String>,
    },

    /// Peer registers a file it is willing to share (metadata only, zero bytes)
    #[serde(rename = "file_upload")]
    FileUpload {
        session_id: SessionId,
        file: FileMetadata,
    },

    /// Peer wants to download a file — server picks a host and tells it to
    /// initiate a WebRTC connection back to the requester
    #[serde(rename = "request_download")]
    RequestDownload {
        session_id: SessionId,
        file_id: String,
    },

    /// Peer finished downloading a file and is now also a host
    #[serde(rename = "file_downloaded")]
    FileDownloaded {
        session_id: SessionId,
        file_id: String,
    },

    // ── WebRTC signaling ─────────────────────────────────────────────────
    /// SDP offer from initiator → target (relayed via server)
    #[serde(rename = "webrtc_offer")]
    WebRTCOffer {
        session_id: SessionId,
        target_session_id: SessionId,
        sdp: String,
    },

    /// SDP answer from target → initiator (relayed via server)
    #[serde(rename = "webrtc_answer")]
    WebRTCAnswer {
        session_id: SessionId,
        target_session_id: SessionId,
        sdp: String,
    },

    /// ICE candidate exchange (both directions, relayed via server)
    #[serde(rename = "ice_candidate")]
    ICECandidate {
        session_id: SessionId,
        target_session_id: SessionId,
        candidate: String,
    },

    // ── Misc ─────────────────────────────────────────────────────────────
    #[serde(rename = "ping")]
    Ping {
        session_id: SessionId,
    },

    #[serde(rename = "text_message")]
    TextMessage {
        session_id: SessionId,
        content: String,
    },
}

// ---------------------------------------------------------------------------
// Server → Client messages
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    /// A new peer joined the network
    #[serde(rename = "peer_joined")]
    PeerJoined {
        peer: PeerInfo,
        total_peers: usize,
    },

    /// A peer left the network
    #[serde(rename = "peer_left")]
    PeerLeft {
        session_id: SessionId,
        total_peers: usize,
    },

    /// Full file catalog (sent on join and whenever the catalog changes)
    #[serde(rename = "file_list_update")]
    FileListUpdate {
        files: Vec<FileMetadata>,
    },

    /// A single file was removed (host went offline, no remaining hosts)
    #[serde(rename = "file_removed")]
    FileRemoved {
        file_id: String,
    },

    /// Server tells a host: "peer X wants file Y — initiate WebRTC to them"
    #[serde(rename = "download_request")]
    DownloadRequest {
        file_id: String,
        requester_session_id: SessionId,
    },

    // ── WebRTC signaling (targeted to a single peer) ─────────────────────
    /// Forwarded SDP offer
    #[serde(rename = "webrtc_offer")]
    WebRTCOffer {
        from_session_id: SessionId,
        sdp: String,
    },

    /// Forwarded SDP answer
    #[serde(rename = "webrtc_answer")]
    WebRTCAnswer {
        from_session_id: SessionId,
        sdp: String,
    },

    /// Forwarded ICE candidate
    #[serde(rename = "ice_candidate")]
    ICECandidate {
        from_session_id: SessionId,
        candidate: String,
    },

    // ── Misc ─────────────────────────────────────────────────────────────
    #[serde(rename = "error")]
    Error {
        message: String,
    },

    #[serde(rename = "pong")]
    Pong,

    #[serde(rename = "text_message")]
    TextMessage {
        message: TextMessage,
    },

    #[serde(rename = "message_history")]
    MessageHistory {
        messages: Vec<TextMessage>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerStats {
    pub total_peers: usize,
    pub peers: Vec<PeerInfo>,
}
