use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub type SessionId = String;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ClientMessage {
    #[serde(rename = "join")]
    Join {
        session_id: SessionId,
        user_agent: Option<String>,
    },
    #[serde(rename = "file_upload")]
    FileUpload {
        session_id: SessionId,
        file: FileMetadata,
    },
    #[serde(rename = "request_download")]
    RequestDownload {
        session_id: SessionId,
        file_id: String,
    },
    #[serde(rename = "file_downloaded")]
    FileDownloaded {
        session_id: SessionId,
        file_id: String,
    },
    #[serde(rename = "file_chunk")]
    FileChunk {
        session_id: SessionId,
        file_id: String,
        chunk_index: u32,
        total_chunks: u32,
        data: String,
        target_session_id: SessionId,
    },
    #[serde(rename = "file_metadata")]
    FileMetadata {
        session_id: SessionId,
        file_id: String,
        file_name: String,
        file_size: u64,
        mime_type: String,
        total_chunks: u32,
        target_session_id: SessionId,
    },
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "peer_joined")]
    PeerJoined {
        peer: PeerInfo,
        total_peers: usize,
    },
    #[serde(rename = "peer_left")]
    PeerLeft {
        session_id: SessionId,
        total_peers: usize,
    },
    #[serde(rename = "file_list_update")]
    FileListUpdate {
        files: Vec<FileMetadata>,
    },
    #[serde(rename = "file_added")]
    FileAdded {
        file: FileMetadata,
    },
    #[serde(rename = "file_removed")]
    FileRemoved {
        file_id: String,
    },
    #[serde(rename = "download_request")]
    DownloadRequest {
        from_session_id: SessionId,
        file_id: String,
        requester_session_id: SessionId,
    },
    #[serde(rename = "file_chunk")]
    FileChunk {
        file_id: String,
        chunk_index: u32,
        total_chunks: u32,
        data: String,
        from_session_id: SessionId,
        target_session_id: SessionId,
    },
    #[serde(rename = "file_metadata")]
    FileMetadata {
        file_id: String,
        file_name: String,
        file_size: u64,
        mime_type: String,
        total_chunks: u32,
        from_session_id: SessionId,
        target_session_id: SessionId,
    },
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
pub struct FileUploadRequest {
    pub name: String,
    pub size: u64,
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerStats {
    pub total_peers: usize,
    pub peers: Vec<PeerInfo>,
}
