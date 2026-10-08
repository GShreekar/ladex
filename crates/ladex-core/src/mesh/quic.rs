//! Mesh connections over QUIC: open stream 0, run the handshake unless both nodes already trust each other, then the control loop.
//!
//! Stream 0 starts with an [`Opening`] from the dialer. A dialer that trusts the listener asks to resume; the listener agrees
//! only if it trusts the dialer too, and otherwise both fall back to the full handshake on the same stream. TLS has already
//! proven each side's key, so resuming needs no passphrase. After that, stream 0 carries postcard-encoded [`MeshMessage`]s
//! and chunk frames, each a length-prefixed frame that starts with a kind byte.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::endpoint::{Accepting, MeshEndpoint, PeerConnection, HUNG_UP};
use super::frames::{FrameSender, FramedStream};
use super::{
    authenticate_inbound, connection_ended, dialer, dispatch, handshake_local, keeps_open_connection, note_clock_skew, start_session,
    AuthFailure, Intro, Liveness, MeshMessage, PeerAddress, Session, MAX_MESH_MESSAGE_BYTES, PROTOCOL_VERSION,
};
use crate::handshake::{self, Failure, Reason};
use crate::trust::Standing;
use crate::types::NodeId;
use crate::{validate, NodeState};

// Nothing before the handshake is large, so an unauthenticated peer can't make this node buffer much.
const OPENING_MAX_FRAME: usize = 64 * 1024;
const OPENING_TIMEOUT: Duration = Duration::from_secs(10);
// A refused dialer gets this long to read why before the connection is dropped.
const REFUSAL_LINGER: Duration = Duration::from_secs(2);
// A peer hanging up on purpose closes the connection right after ending stream 0, so its close code arrives within this.
const CLOSE_CODE_WAIT: Duration = Duration::from_secs(1);
const CONNECTION_LOST: quinn::VarInt = quinn::VarInt::from_u32(0);

const KIND_MESSAGE: u8 = 0;
const KIND_CHUNKS: u8 = 1;

/// The dialer's first frame on stream 0.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Opening {
    /// The dialer already trusts the listener and asks to skip the handshake.
    Resume { protocol: u32, name: String, intro: String },
    /// A handshake hello follows.
    Handshake,
}

/// The listener's answer to [`Opening::Resume`].
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResumeReply {
    Resumed {
        name: String,
        intro: String,
    },
    /// The listener doesn't trust the dialer, so the dialer must send a handshake hello next.
    HandshakeRequired,
    Refused {
        reason: Reason,
    },
}

/// Dials the node `expected` at `addr`, authenticates it and starts the control loop.
pub async fn connect(state: &NodeState, addr: SocketAddr, expected: &NodeId) -> anyhow::Result<()> {
    let endpoint = state.mesh_endpoint.as_ref().context("this node has no QUIC endpoint")?;
    let connection = endpoint.connect(addr, expected).await?;
    let (send, recv) = connection.connection.open_bi().await?;
    let mut stream = FramedStream::new(send, recv, OPENING_MAX_FRAME);
    let peer = open_outbound(state, &connection, &mut stream).await.map_err(|e| failure_error(addr, e))?;
    tracing::info!("Mesh: {addr} authenticated over QUIC (peer: {})", peer.node_id);
    begin(state, connection, peer, stream, state.node_id.clone()).await;
    Ok(())
}

/// Accepts mesh connections until the endpoint closes, each on its own task.
pub async fn accept_loop(state: NodeState, endpoint: Arc<MeshEndpoint>) {
    while let Some(accepting) = endpoint.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            let remote = accepting.remote_address();
            if let Err(e) = serve_inbound(&state, accepting).await {
                tracing::warn!("Mesh inbound (QUIC) from {remote}: {e:#}");
            }
        });
    }
}

async fn serve_inbound(state: &NodeState, accepting: Accepting) -> anyhow::Result<()> {
    let connection = accepting.establish().await?;
    let (send, recv) =
        tokio::time::timeout(OPENING_TIMEOUT, connection.connection.accept_bi()).await.context("the peer opened no stream in time")??;
    let mut stream = FramedStream::new(send, recv, OPENING_MAX_FRAME);
    let peer = match open_inbound(state, &connection, &mut stream).await {
        Ok(peer) => peer,
        Err(failure) => {
            linger(&connection, &mut stream).await;
            return Err(failure.into());
        }
    };
    tracing::info!("Mesh: inbound QUIC peer {} ({}) authenticated", peer.node_id, peer.name);
    let dialer = peer.node_id.clone();
    begin(state, connection, peer, stream, dialer).await;
    Ok(())
}

async fn open_outbound(state: &NodeState, connection: &PeerConnection, stream: &mut FramedStream) -> Result<handshake::Peer, Failure> {
    let binding = channel_binding(connection)?;
    let our_intro = Intro::ours(state);
    let local = handshake_local(state, &our_intro, &binding);
    match state.trust.standing(&connection.public_key).map_err(Failure::TrustStore)? {
        Standing::Revoked => return Err(Failure::Refused(Reason::Revoked)),
        Standing::Unknown => {
            send_json(stream, &Opening::Handshake).await?;
            return same_key(handshake::connect(stream, &local).await?, connection);
        }
        Standing::Trusted(_) => {}
    }
    let resume = Opening::Resume { protocol: PROTOCOL_VERSION, name: state.node_name.clone(), intro: our_intro.clone() };
    send_json(stream, &resume).await?;
    match recv_json(stream).await? {
        ResumeReply::Resumed { name, intro } => resumed(state, connection, name, intro),
        ResumeReply::HandshakeRequired => same_key(handshake::connect(stream, &local).await?, connection),
        ResumeReply::Refused { reason } => Err(Failure::RejectedBy(reason)),
    }
}

async fn open_inbound(state: &NodeState, connection: &PeerConnection, stream: &mut FramedStream) -> Result<handshake::Peer, Failure> {
    let opening = tokio::time::timeout(OPENING_TIMEOUT, recv_json(stream)).await.map_err(|_| Failure::TimedOut)??;
    if let Opening::Resume { protocol, name, intro } = opening {
        match resume_decision(state, connection, protocol).await? {
            Some(reason) => {
                send_json(stream, &ResumeReply::Refused { reason: reason.clone() }).await?;
                return Err(Failure::Refused(reason));
            }
            None if is_trusted(state, connection)? => {
                let reply = ResumeReply::Resumed { name: state.node_name.clone(), intro: Intro::ours(state) };
                send_json(stream, &reply).await?;
                return resumed(state, connection, name, intro);
            }
            None => send_json(stream, &ResumeReply::HandshakeRequired).await?,
        }
    }
    let binding = channel_binding(connection)?;
    let remote_ip = connection.connection.remote_address().ip();
    same_key(authenticate_inbound(stream, remote_ip, state, &binding).await?, connection)
}

/// Why a resuming dialer must be turned away outright, if it must.
async fn resume_decision(state: &NodeState, connection: &PeerConnection, protocol: u32) -> Result<Option<Reason>, Failure> {
    if protocol != PROTOCOL_VERSION {
        return Ok(Some(Reason::ProtocolMismatch { expected: PROTOCOL_VERSION }));
    }
    if matches!(state.trust.standing(&connection.public_key).map_err(Failure::TrustStore)?, Standing::Revoked) {
        return Ok(Some(Reason::Revoked));
    }
    if keeps_open_connection(state, &connection.node_id, &connection.node_id).await {
        return Ok(Some(Reason::Duplicate));
    }
    Ok(None)
}

fn is_trusted(state: &NodeState, connection: &PeerConnection) -> Result<bool, Failure> {
    Ok(matches!(state.trust.standing(&connection.public_key).map_err(Failure::TrustStore)?, Standing::Trusted(_)))
}

/// A peer that skipped the handshake: TLS proved its key, and the trust store vouches for it.
fn resumed(state: &NodeState, connection: &PeerConnection, name: String, intro: String) -> Result<handshake::Peer, Failure> {
    if intro.len() > handshake::MAX_INTRO_BYTES {
        return Err(Failure::Malformed("intro too long"));
    }
    let name = validate::clean_label(&name, validate::MAX_NODE_NAME_CHARS);
    state.trust.mark_seen(&connection.node_id, &name, crate::hlc::wall_clock_ms()).map_err(Failure::TrustStore)?;
    Ok(handshake::Peer { node_id: connection.node_id.clone(), public_key: connection.public_key, name, intro })
}

/// The handshake must prove the same key TLS did, or one node could borrow another's identity for half the exchange.
fn same_key(peer: handshake::Peer, connection: &PeerConnection) -> Result<handshake::Peer, Failure> {
    if peer.public_key != connection.public_key {
        return Err(Failure::ProofFailed);
    }
    Ok(peer)
}

fn channel_binding(connection: &PeerConnection) -> Result<Vec<u8>, Failure> {
    connection.channel_binding().map_err(|e| Failure::Transport(std::io::Error::other(e.to_string())))
}

async fn send_json<T: Serialize>(stream: &mut FramedStream, value: &T) -> Result<(), Failure> {
    let bytes = serde_json::to_vec(value).expect("opening frames always serialize");
    stream.sender.send(&bytes).await.map_err(Failure::Transport)
}

async fn recv_json<T: DeserializeOwned>(stream: &mut FramedStream) -> Result<T, Failure> {
    let bytes = stream.receiver.recv().await.map_err(Failure::Transport)?.ok_or(Failure::Closed)?;
    serde_json::from_slice(&bytes).map_err(|_| Failure::Malformed("unreadable opening frame"))
}

/// Gives the dialer a moment to read a refusal; it closes the connection itself once it has.
async fn linger(connection: &PeerConnection, stream: &mut FramedStream) {
    stream.sender.finish();
    let _ = tokio::time::timeout(REFUSAL_LINGER, connection.connection.closed()).await;
}

fn failure_error(addr: SocketAddr, failure: Failure) -> anyhow::Error {
    let message = format!("Peer {addr}: {failure}");
    if failure.is_authentication() {
        anyhow::Error::new(AuthFailure(message))
    } else {
        anyhow::anyhow!(message)
    }
}

/// Registers the peer and starts its control loop; hangs up instead if the connection already open with the peer stays.
async fn begin(state: &NodeState, connection: PeerConnection, peer: handshake::Peer, mut stream: FramedStream, dialer: NodeId) {
    let intro = Intro::theirs(&peer);
    note_clock_skew(state, &peer.name, intro.now_ms).await;
    // The browser-facing address is kept for the UI and restarts; a lost QUIC peer is redialed by discovery instead.
    let ip = intro.ip.parse().unwrap_or_else(|_| connection.connection.remote_address().ip());
    let address =
        PeerAddress { addr: SocketAddr::new(ip, intro.http_port), http_port: intro.http_port, dialer, liveness: Liveness::QuicIdleTimeout };
    let Some(session) = start_session(state, &peer.node_id, &peer.name, address).await else {
        tracing::warn!("Mesh: duplicate QUIC connection with {} — dropping", peer.node_id);
        connection.connection.close(HUNG_UP, b"already connected");
        return;
    };
    stream.allow_frames_up_to(MAX_MESH_MESSAGE_BYTES + 1);
    tokio::spawn(control_loop(state.clone(), connection, peer.node_id, stream, session));
}

async fn control_loop(state: NodeState, connection: PeerConnection, peer_node_id: NodeId, stream: FramedStream, session: Session) {
    let FramedStream { sender, mut receiver } = stream;
    let Session { sender: peer_tx, messages, data, hang_up } = session;
    let writer = tokio::spawn(write_frames(sender, messages, data));
    let we_hung_up = loop {
        let frame = tokio::select! {
            frame = receiver.recv() => frame,
            () = hang_up.notified() => break true,
        };
        match frame {
            Ok(Some(frame)) => handle_frame(&state, &peer_node_id, &frame).await,
            Ok(None) => break false,
            Err(e) => {
                tracing::warn!("Mesh: QUIC connection to {peer_node_id} ended: {e}");
                break false;
            }
        }
    };
    writer.abort();
    let deliberate = we_hung_up || peer_hung_up(&connection.connection).await;
    connection.connection.close(if deliberate { HUNG_UP } else { CONNECTION_LOST }, b"connection ended");
    if connection_ended(&state, &peer_node_id, &peer_tx).await && !deliberate {
        tracing::info!("Mesh: lost the connection to {peer_node_id}");
        dialer::dial(&state, &peer_node_id, connection.connection.remote_address());
    }
}

/// Whether the peer closed the connection on purpose, rather than the link failing or going silent.
async fn peer_hung_up(connection: &quinn::Connection) -> bool {
    let reason = tokio::time::timeout(CLOSE_CODE_WAIT, connection.closed()).await;
    matches!(reason, Ok(quinn::ConnectionError::ApplicationClosed(close)) if close.error_code == HUNG_UP)
}

async fn write_frames(mut sender: FrameSender, mut messages: mpsc::UnboundedReceiver<MeshMessage>, mut data: mpsc::Receiver<Vec<u8>>) {
    loop {
        // Control messages (heartbeats, catalog) always go before chunk data.
        let frame = tokio::select! {
            biased;
            message = messages.recv() => match message {
                Some(message) => message_frame(&message),
                None => break,
            },
            chunks = data.recv() => match chunks {
                Some(chunks) => [&[KIND_CHUNKS][..], &chunks].concat(),
                None => break,
            },
        };
        if sender.send(&frame).await.is_err() {
            break;
        }
    }
    sender.finish();
}

fn message_frame(message: &MeshMessage) -> Vec<u8> {
    let frame = vec![KIND_MESSAGE];
    postcard::to_extend(message, frame).expect("mesh messages always serialize")
}

fn decode_message(bytes: &[u8]) -> Result<MeshMessage, postcard::Error> {
    let (message, rest) = postcard::take_from_bytes(bytes)?;
    // Leftover bytes mean the peer encoded something else, so the message read from the front can't be trusted.
    if !rest.is_empty() {
        return Err(postcard::Error::DeserializeBadEncoding);
    }
    Ok(message)
}

async fn handle_frame(state: &NodeState, peer_node_id: &NodeId, frame: &[u8]) {
    match frame.split_first() {
        Some((&KIND_MESSAGE, bytes)) => match decode_message(bytes) {
            Ok(message) => dispatch(&message, peer_node_id, state).await,
            Err(e) => tracing::warn!("Mesh: unreadable message from {peer_node_id}: {e}"),
        },
        Some((&KIND_CHUNKS, chunks)) => crate::transfer::on_binary_frame(state, peer_node_id, chunks),
        _ => tracing::warn!("Mesh: unknown frame from {peer_node_id} — ignored"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hlc::Stamp;
    use crate::identity::Identity;
    use crate::revocation::Revocation;
    use crate::types::{FileMetadata, Holder, PeerInfo, ServerMessage, TextMessage};

    fn stamp() -> Stamp {
        Stamp { wall: 1_700_000_000_000, counter: 3, node: "node_a".into() }
    }

    fn file() -> FileMetadata {
        FileMetadata {
            id: "file_1".into(),
            name: "notes.txt".into(),
            size: 42,
            mime_type: "text/plain".into(),
            uploader_id: "device_1".into(),
            uploader_node: "node_a".into(),
            holders: [("node_a".to_string(), Holder { since: stamp(), present: true })].into(),
            uploaded_at: chrono::Utc::now(),
            created_at: 1,
            version: stamp(),
            deleted: false,
            deleted_at: 0,
            manifest_root: Some("ab".repeat(32)),
            is_folder: false,
            parent: Some("folder_1".into()),
            folder_bytes: 0,
            folder_files: 0,
        }
    }

    fn text(id: &str) -> TextMessage {
        TextMessage {
            id: id.into(),
            content: "hello".into(),
            sender_id: "device_1".into(),
            sender_name: Some("Ada".into()),
            timestamp: chrono::Utc::now(),
            created_at: 7,
        }
    }

    fn device() -> PeerInfo {
        PeerInfo {
            session_id: "device_1".into(),
            connected_at: chrono::Utc::now(),
            user_agent: Some("Firefox".into()),
            hosting_node_id: Some("node_a".into()),
            node_rtt_ms: Some(12),
            left: true,
            left_at: Some(chrono::Utc::now()),
            hosting_node_name: Some("laptop".into()),
            nickname: None,
            version: stamp(),
        }
    }

    fn encode(message: &MeshMessage) -> Vec<u8> {
        message_frame(message)[1..].to_vec()
    }

    /// Decodes the message and checks it encodes back to the same bytes, so no field was lost or changed.
    fn assert_survives_postcard(message: MeshMessage) -> MeshMessage {
        let bytes = encode(&message);
        let decoded = decode_message(&bytes).unwrap_or_else(|e| panic!("{message:?} did not decode: {e}"));
        assert_eq!(encode(&decoded), bytes);
        decoded
    }

    #[test]
    fn a_message_frame_starts_with_the_message_kind() {
        assert_eq!(message_frame(&MeshMessage::Ping { ts: 1 })[0], KIND_MESSAGE);
    }

    #[test]
    fn a_catalog_sync_survives_postcard() {
        assert_survives_postcard(MeshMessage::CatalogSync { files: vec![file()] });
    }

    #[test]
    fn presence_survives_postcard() {
        assert_survives_postcard(MeshMessage::PeerSync { peers: vec![device()] });
    }

    #[test]
    fn a_chat_history_survives_postcard() {
        assert_survives_postcard(MeshMessage::ChatSync { messages: vec![text("m1"), text("m2")] });
    }

    #[test]
    fn a_chat_message_survives_postcard() {
        assert_survives_postcard(MeshMessage::ChatMessage { message: text("m1") });
    }

    #[test]
    fn heartbeats_survive_postcard() {
        assert_survives_postcard(MeshMessage::Ping { ts: u64::MAX });
        assert_survives_postcard(MeshMessage::Pong { ts: 0 });
    }

    #[test]
    fn a_goodbye_survives_postcard() {
        assert_survives_postcard(MeshMessage::Goodbye { node_id: "node_a".into() });
    }

    #[test]
    fn transfer_requests_survive_postcard() {
        assert_survives_postcard(MeshMessage::GetManifest { file_id: "file_1".into() });
        assert_survives_postcard(MeshMessage::Manifest { file_id: "file_1".into(), size: 3, hashes: "cd".repeat(32) });
        assert_survives_postcard(MeshMessage::GetChunks { file_id: "file_1".into(), indices: vec![0, 5, 300] });
        assert_survives_postcard(MeshMessage::ChunkMap { file_id: "file_1".into(), chunks: 9, bitmap: "ff01".into() });
        assert_survives_postcard(MeshMessage::ChunkError { file_id: "file_1".into(), index: 2, reason: "gone".into() });
    }

    #[test]
    fn a_revocation_still_verifies_after_postcard() {
        let revocation = Revocation::issue(&Identity::generate(), &Identity::generate().public_key(), 5);
        let MeshMessage::Revocations { revocations } = assert_survives_postcard(MeshMessage::Revocations { revocations: vec![revocation] })
        else {
            panic!("decoded as another message");
        };
        assert!(revocations[0].is_signed());
    }

    #[test]
    fn a_relayed_signal_still_reads_as_a_server_message_after_postcard() {
        let offer = ServerMessage::IncomingFileOffer { file_id: "file_1".into(), from_session_id: "device_1".into() };
        let relay = MeshMessage::SignalRelay {
            to_node_id: "node_b".into(),
            from_node_id: "node_a".into(),
            to_session_id: "device_2".into(),
            signal: serde_json::to_string(&offer).unwrap(),
        };
        let MeshMessage::SignalRelay { signal, .. } = assert_survives_postcard(relay) else { panic!("decoded as another message") };
        assert!(matches!(serde_json::from_str(&signal).unwrap(), ServerMessage::IncomingFileOffer { .. }));
    }

    #[test]
    fn trailing_bytes_after_a_message_are_refused() {
        let mut bytes = encode(&MeshMessage::Ping { ts: 1 });
        bytes.push(0);
        assert!(decode_message(&bytes).is_err());
    }

    #[test]
    fn an_unknown_message_variant_is_refused() {
        assert!(decode_message(&[200]).is_err());
    }

    #[test]
    fn a_truncated_message_is_refused() {
        let bytes = encode(&MeshMessage::ChatMessage { message: text("m1") });
        assert!(decode_message(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn a_json_message_from_an_older_node_is_refused() {
        assert!(decode_message(br#"{"type":"ping","ts":1}"#).is_err());
    }
}
