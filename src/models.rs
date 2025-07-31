use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Metadata {
    pub id: String,
    pub name: String,
    pub file_type: String,
    pub size: u64,
    pub uploader_id: String,
    pub hosts: HashSet<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Peer {
    pub id: String,
    pub ip: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum MessageType {
    // Broadcast new/changed metadata to all peers
    MetadataUpdate(Metadata),
    // Request to download a file
    RequestDownload {
        metadata_id: String,
        requester_id: String,
    },
    // WebRTC offer for peer-to-peer connection
    Offer {
        metadata_id: String,
        from_peer: String,
        sdp: String,
    },
    // WebRTC answer for peer-to-peer connection
    Answer {
        metadata_id: String,
        from_peer: String,
        sdp: String,
    },
    // WebRTC ICE candidate
    IceCandidate {
        metadata_id: String,
        from_peer: String,
        candidate: String,
    },
    // Notify peer disconnection
    PeerDisconnect(String),
}