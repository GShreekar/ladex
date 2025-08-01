use actix::{Actor, ActorContext, Addr, AsyncContext, Handler, StreamHandler};
use actix_web_actors::ws::{self, Message, ProtocolError};
use std::net::SocketAddr;

use crate::coordinator::{Coordinator, CoordinatorMessage, RegisterSession, UnregisterSession};
use crate::models::{MessageType, Peer};

//WebSocket session actor
pub struct WebSocketSession {
    session_id: String,
    coordinator: Addr<Coordinator>,
    addr: Option<SocketAddr>,
}

impl WebSocketSession {
    pub fn new(coordinator: Addr<Coordinator>, addr: Option<SocketAddr>) -> Self {
        let session_id = uuid::Uuid::new_v4().to_string();
        WebSocketSession {
            session_id,
            coordinator,
            addr,
        }
    }

    fn handle_message(&mut self, msg: &str, ctx: &mut ws::WebsocketContext<Self>) {
        match serde_json::from_str::<MessageType>(msg) {
            Ok(message) => {
                self.coordinator.do_send(CoordinatorMessage {
                    session_id: self.session_id.clone(),
                    msg: message,
                });
            }
            Err(e) => {
                eprintln!("Failed to parse WebSocket message: {}", e);
                ctx.text(format!("{{\"error\": \"Invalid message format: {}\"}}", e));
            }
        }
    }
}

impl Actor for WebSocketSession {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        let peer = Peer {
            id: self.session_id.clone(),
            ip: self
                .addr
                .map(|addr| addr.ip().to_string())
                .unwrap_or_else(|| "unknown".to_string()),
        };
        self.coordinator.do_send(RegisterSession {
            session_id: self.session_id.clone(),
            addr: ctx.address(),
            peer,
        });
        println!("WebSocket session started: {}", self.session_id);
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        self.coordinator.do_send(UnregisterSession {
            session_id: self.session_id.clone(),
        });
        println!("WebSocket session stopped: {}", self.session_id);
    }
}

impl StreamHandler<Result<Message, ProtocolError>> for WebSocketSession {
    fn handle(&mut self, msg: Result<Message, ProtocolError>, ctx: &mut Self::Context) {
        match msg {
            Ok(Message::Text(text)) => {
                if text.len() > 1024 * 1024 { // 1MB limit for messages
                    eprintln!("Message too large from session: {}", self.session_id);
                    ctx.text(r#"{"error": "Message too large"}"#);
                    return;
                }
                self.handle_message(&text, ctx);
            }
            Ok(Message::Binary(_)) => {
                eprintln!("Unexpected binary message from session: {}", self.session_id);
                ctx.text(r#"{"error": "Binary messages not supported"}"#);
            }
            Ok(Message::Close(reason)) => {
                println!("WebSocket closing for session {}: {:?}", self.session_id, reason);
                ctx.stop();
            }
            Ok(Message::Ping(bytes)) => {
                ctx.pong(&bytes);
            }
            Ok(Message::Pong(_)) => {
                // Heartbeat response received
                println!("Pong received from session: {}", self.session_id);
            }
            Ok(_) => {
                // Handle other message types
            }
            Err(e) => {
                eprintln!("WebSocket protocol error for session {}: {}", self.session_id, e);
                ctx.stop();
            }
        }
    }
}

impl Handler<CoordinatorMessage> for WebSocketSession {
    type Result = ();

    fn handle(&mut self, msg: CoordinatorMessage, ctx: &mut Self::Context) ->
    Self::Result {
        if let Ok(json) = serde_json::to_string(&msg.msg) {
            ctx.text(json);
        } else {
            eprintln!("Failed to serialize message for session: {}", self.session_id);
        }
    }
}