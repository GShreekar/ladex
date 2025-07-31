use actix::{Actor, ActorContext, Addr, AsyncContext, Context, Handler, Message};
use std::collections::{HashMap, HashSet};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::{Metadata, MessageType, Peer};

// WebSocket sessions to the coordinator
#[derive(Message)]
#[rtype(result = "()")]
pub struct CoordinatorMessage {
    pub session_id: String,
    pub msg: MessageType,
}

// To register a WebSocket session
#[derive(Message)]
#[rtype(result = "()")]
pub struct RegisterSession {
    pub session_id: String,
    pub addr: Addr<crate::websocket::WebSocketSession>,
    pub peer: Peer,
}

// To unregister a WebSocket session
#[derive(Message)]
#[rtype(result = "()")]
pub struct UnregisterSession {
    pub session_id: String,
}

pub struct Coordinator {
    sessions: HashMap<String, Addr<crate::websocket::WebSocketSession>>,
    peers: HashMap<String, Peer>,
    metadata: HashMap<String, Metadata>,
}

impl Coordinator {
    pub fn new() -> Self {
        Coordinator {
            sessions: HashMap::new(),
            peers: HashMap::new(),
            metadata: HashMap::new(),
        }
    }

    // Broadcast metadata updates to all sessions
    fn broadcast_metadata(&self, ctx: &mut Context<Self>) {
        let metadata_list: Vec<Metadata> = self.metadata.values().cloned().collect();
        for session in self.sessions.values() {
            for metadata in &metadata_list {
                session.do_send(CoordinatorMessage {
                    session_id: String::new(),
                    msg: MessageType::MetadataUpdate(metadata.clone()),
                });
            }
        }
    }

    // Forward WebRTC signaling messages to the appropriate peer
    fn forward_signaling(&self, target_peer_id: &str, msg: MessageType) {
        if let Some(session) = self.sessions.get(target_peer_id) {
            session.do_send(CoordinatorMessage {
                session_id: target_peer_id.to_string(),
                msg,
            });
        }
    }
}

impl Actor for Coordinator {
    type Context = Context<Self>;

    fn started(&mut self, _ctx: &mut Self::Context) {
        println!("Coordinator actor started");
    }
    fn stopped(&mut self, _ctx: &mut Self::Context) {
        println!("Coordinator actor stopped");
    }
}

impl Handler<RegisterSession> for Coordinator {
    type Result = ();

    fn handle(&mut self, msg: RegisterSession, ctx: &mut Self::Context) ->
    Self::Result {
        self.sessions.insert(msg.session_id.clone(), msg.addr);
        self.peers.insert(msg.session_id.clone(), msg.peer);
        println!("Registered session: {}", msg.session_id);
        self.broadcast_metadata(ctx);
    }
}

impl Handler<UnregisterSession> for Coordinator {
    type Result = ();

    fn handle(&mut self, msg: UnregisterSession, ctx: &mut Self::Context) ->
    Self::Result {
        self.sessions.remove(&msg.session_id);
        self.peers.remove(&msg.session_id);
        let mut to_remove = Vec::new();
        for metadata in self.metadata.values_mut() {
            metadata.hosts.remove(&msg.session_id);
            if metadata.hosts.is_empty() {
                to_remove.push(metadata.id.clone());
            }
        }
        for id in to_remove {
            self.metadata.remove(&id);
        }
        for session in self.sessions.values() {
            session.do_send(CoordinatorMessage {
                session_id: String::new(),
                msg: MessageType::PeerDisconnect(msg.session_id.clone()),
            });
        }
        self.broadcast_metadata(ctx);
        println!("Unregistered session: {}", msg.session_id);
    }
}

impl Handler<CoordinatorMessage> for Coordinator {
    type Result = ();

    fn handle(&mut self, msg: CoordinatorMessage, ctx: &mut Self::Context) ->
    Self::Result {
        match msg.msg {
            // Update/Insert metadata, adding the sender as a host
            MessageType::MetadataUpdate(mut metadata) => {
                metadata.hosts.insert(msg.session_id.clone());
                self.metadata.insert(metadata.id.clone(), metadata.clone());
                self.broadcast_metadata(ctx);
            }
            // Find a host for the requested metadata and initiate WebRTC signaling
            MessageType::RequestDownload { metadata_id, requester_id } => {
                if let Some(metadata) = self.metadata.get(&metadata_id) {
                    if let Some(host_id) = metadata.hosts.iter().next() {
                        self.forward_signaling(
                            host_id,
                            MessageType::RequestDownload {
                                metadata_id,
                                requester_id,
                            },
                        );
                    }
                }
            }
            // Forward WebRTC offer to the requester
            MessageType::Offer { metadata_id, from_peer, sdp } => {
                if let Some(metadata) = self.metadata.get(&metadata_id) {
                    if let Some(requester_id) = metadata.hosts.iter().find(|id| *id != &from_peer) {
                        self.forward_signaling(
                            requester_id,
                            MessageType::Offer {
                                metadata_id,
                                from_peer,
                                sdp,
                            },
                        );
                    }
                }
            }
            // Forward WebRTC answer to the original offerer
            MessageType::Answer { metadata_id, from_peer, sdp } => {
                self.forward_signaling(
                    &from_peer,
                    MessageType::Answer {
                        metadata_id,
                        from_peer: msg.session_id,
                        sdp,
                    },
                );
            }
            // Forward ICE candidate to the appropriate peer
            MessageType::IceCandidate { metadata_id, from_peer, candidate } => {
                self.forward_signaling(
                    &from_peer,
                    MessageType::IceCandidate {
                        metadata_id,
                        from_peer: msg.session_id,
                        candidate,
                    },
                );
            }
            MessageType::PeerDisconnect(_) => {
                //Handled by UnregisterSession
            }
        }
    }
}