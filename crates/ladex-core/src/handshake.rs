// The mesh handshake, over any transport that carries whole frames both ways.
//
//   dialer -> Hello    { protocol, secured, name, public key, SPAKE2 message, intro }
//   server -> Reply    { name, public key, SPAKE2 message, intro, proof, signature }
//          or Rejected { reason }
//   dialer -> Confirm  { proof, signature }
//
// Both sides run SPAKE2 with the passphrase, so the passphrase never crosses the
// wire and a wrong guess costs one connection, with no offline attack. Each
// side then sends a proof, HMAC(K, "ladex-confirm" ‖ role ‖ transcript), and an
// Ed25519 signature over the same input with its identity key. The transcript
// covers every field both sides sent plus the channel binding (the server's TLS
// certificate fingerprint as each side saw it), so a man in the middle who
// terminates TLS twice, or changes any field, makes the proofs fail; and the
// signature means a node can only claim the id of a key it holds.
//
// Every incoming connection counts as a passphrase guess against its IP until it
// proves otherwise (ratelimit.rs locks a guesser out for exponentially longer).
// Nodes on a secured mesh are recorded in the trust store; a revoked key is
// turned away even when it knows the passphrase.

use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::time::Duration;

use ed25519_dalek::{Signature, VerifyingKey};
use ring::{digest, hmac};
use serde::{Deserialize, Serialize};
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};

use crate::identity::{self, Identity};
use crate::ratelimit::{AttemptLimiter, Ticket};
use crate::trust::{Standing, TrustStore, TrustedVia};
use crate::types::NodeId;
use crate::validate;

// How long to wait for each of the other side's frames.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_FRAME_BYTES: usize = 16 * 1024;
pub const MAX_INTRO_BYTES: usize = 4096;
// Ed25519 SPAKE2 messages are 33 bytes; anything much larger isn't one.
const MAX_PAKE_MESSAGE_BYTES: usize = 64;

// Fixed SPAKE2 identities: the dialer doesn't know the server's key up front,
// so the real keys are bound through the transcript instead.
const CLIENT_SPAKE_IDENTITY: &[u8] = b"ladex mesh client";
const SERVER_SPAKE_IDENTITY: &[u8] = b"ladex mesh server";
const TRANSCRIPT_LABEL: &[u8] = b"ladex mesh handshake v1";
const PROOF_LABEL: &[u8] = b"ladex-confirm";
const SIGNATURE_LABEL: &[u8] = b"ladex-sign";

/// Carries whole frames between two nodes, in order.
pub trait Transport: Send {
    fn send(&mut self, frame: Vec<u8>) -> impl Future<Output = io::Result<()>> + Send;
    /// `Ok(None)` once the other side has closed.
    fn recv(&mut self) -> impl Future<Output = io::Result<Option<Vec<u8>>>> + Send;
}

/// What this node brings to a handshake.
pub struct Local<'a> {
    pub identity: &'a Identity,
    pub name: &'a str,
    pub passphrase: Option<&'a str>,
    pub protocol: u32,
    /// Anything else the caller wants the peer to know, authenticated with the rest.
    pub intro: &'a str,
    /// The server's TLS certificate fingerprint as this side saw it; empty without TLS.
    pub channel_binding: &'a [u8],
    pub trust: &'a TrustStore,
}

/// The node on the other end, once it has proven itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub node_id: NodeId,
    pub public_key: VerifyingKey,
    pub name: String,
    pub intro: String,
}

/// Why the server turned a dialer away.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum Reason {
    RateLimited { retry_after_secs: u64 },
    ProtocolMismatch { expected: u32 },
    SecurityMismatch,
    Revoked,
    SelfConnection,
    Duplicate,
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reason::RateLimited { retry_after_secs } => write!(f, "too many failed attempts, locked out for another {retry_after_secs}s"),
            Reason::ProtocolMismatch { expected } => write!(f, "protocol version mismatch, expected {expected}"),
            Reason::SecurityMismatch => f.write_str("one side has a passphrase and the other does not"),
            Reason::Revoked => f.write_str("the node has been revoked"),
            Reason::SelfConnection => f.write_str("a node cannot connect to itself"),
            Reason::Duplicate => f.write_str("the nodes are already connected"),
        }
    }
}

#[derive(Debug)]
pub enum Failure {
    /// This node turned the peer away.
    Refused(Reason),
    /// The peer turned this node away.
    RejectedBy(Reason),
    /// Wrong passphrase, a key the peer doesn't hold, or someone in the middle.
    ProofFailed,
    Malformed(&'static str),
    Closed,
    TimedOut,
    Transport(io::Error),
    TrustStore(anyhow::Error),
}

impl Failure {
    /// Whether dialing again soon would fail the same way.
    pub fn is_authentication(&self) -> bool {
        matches!(
            self,
            Failure::ProofFailed
                | Failure::Refused(Reason::Revoked)
                | Failure::RejectedBy(Reason::RateLimited { .. } | Reason::SecurityMismatch | Reason::Revoked)
        )
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Refused(reason) => write!(f, "refused the peer: {reason}"),
            Failure::RejectedBy(reason) => write!(f, "rejected by the peer: {reason}"),
            Failure::ProofFailed => f.write_str("the peer failed to prove the passphrase and its key: wrong passphrase, or the connection is being intercepted"),
            Failure::Malformed(what) => write!(f, "malformed handshake: {what}"),
            Failure::Closed => f.write_str("the peer closed the connection during the handshake"),
            Failure::TimedOut => f.write_str("the peer did not answer in time"),
            Failure::Transport(e) => write!(f, "connection error during the handshake: {e}"),
            Failure::TrustStore(e) => write!(f, "trust store error: {e:#}"),
        }
    }
}

impl std::error::Error for Failure {}

/// Runs the dialer's side of the handshake.
pub async fn connect<T: Transport>(transport: &mut T, local: &Local<'_>) -> Result<Peer, Failure> {
    let (pake, client) = Side::start(Role::Client, local);
    let secured = local.passphrase.is_some();
    let hello = Frame::Hello {
        protocol: local.protocol,
        secured,
        name: client.name.clone(),
        public_key: client.public_key.as_bytes().to_vec(),
        pake: client.pake.clone(),
        intro: client.intro.clone(),
    };
    send_frame(transport, &hello).await?;

    let (server, proof, signature) = match recv_frame(transport).await? {
        Frame::Reply { name, public_key, pake, intro, proof, signature } => (Side::remote(name, &public_key, pake, intro)?, proof, signature),
        Frame::Rejected { reason } => return Err(Failure::RejectedBy(reason)),
        _ => return Err(Failure::Malformed("expected a reply")),
    };
    if is_revoked(local.trust, &server.public_key)? {
        return Err(Failure::Refused(Reason::Revoked));
    }

    let transcript = Transcript::new(local.protocol, secured, local.channel_binding, &client, &server);
    let key = pake.finish(&server.pake).map_err(|_| Failure::Malformed("unusable SPAKE2 message"))?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, &key);
    if !is_authentic(&server.public_key, &key, Role::Server, &transcript, &proof, &signature) {
        return Err(Failure::ProofFailed);
    }
    let (proof, signature) = authenticate(local.identity, &key, Role::Client, &transcript);
    send_frame(transport, &Frame::Confirm { proof, signature }).await?;

    remember(local, &server)?;
    Ok(server.into_peer())
}

/// Starts the server's side of the handshake: reads the dialer's hello, unless
/// its IP is locked out. Finish with `Incoming::accept` or `Incoming::refuse`.
pub async fn receive_hello<'a, T: Transport>(transport: &mut T, limiter: &'a AttemptLimiter, remote_ip: IpAddr) -> Result<Incoming<'a>, Failure> {
    let ticket = match limiter.begin(remote_ip) {
        Ok(ticket) => ticket,
        Err(retry_after) => return Err(reject(transport, Reason::RateLimited { retry_after_secs: retry_after.as_secs().max(1) }).await),
    };
    let Frame::Hello { protocol, secured, name, public_key, pake, intro } = recv_frame(transport).await? else {
        return Err(Failure::Malformed("expected a hello"));
    };
    let client = Side::remote(name, &public_key, pake, intro)?;
    Ok(Incoming { limiter, remote_ip, ticket, protocol, secured, client })
}

/// A dialer that has said hello but not yet proven anything.
pub struct Incoming<'a> {
    limiter: &'a AttemptLimiter,
    remote_ip: IpAddr,
    // Dropped unrefunded unless the dialer proves the passphrase, so the attempt counts as a guess.
    ticket: Ticket,
    protocol: u32,
    secured: bool,
    client: Side,
}

impl Incoming<'_> {
    /// The node id the dialer claims; not proven until `accept` succeeds.
    pub fn claimed_node_id(&self) -> NodeId {
        identity::node_id_for(&self.client.public_key)
    }

    pub async fn refuse<T: Transport>(self, transport: &mut T, reason: Reason) -> Failure {
        reject(transport, reason).await
    }

    /// Runs the rest of the server's side of the handshake.
    pub async fn accept<T: Transport>(self, transport: &mut T, local: &Local<'_>) -> Result<Peer, Failure> {
        if let Some(reason) = self.reason_to_refuse(local)? {
            return Err(reject(transport, reason).await);
        }
        let (pake, server) = Side::start(Role::Server, local);
        let transcript = Transcript::new(self.protocol, self.secured, local.channel_binding, &self.client, &server);
        let key = pake.finish(&self.client.pake).map_err(|_| Failure::Malformed("unusable SPAKE2 message"))?;
        let key = hmac::Key::new(hmac::HMAC_SHA256, &key);

        let (proof, signature) = authenticate(local.identity, &key, Role::Server, &transcript);
        let reply = Frame::Reply {
            name: server.name,
            public_key: server.public_key.as_bytes().to_vec(),
            pake: server.pake,
            intro: server.intro,
            proof,
            signature,
        };
        send_frame(transport, &reply).await?;

        let Frame::Confirm { proof, signature } = recv_frame(transport).await? else {
            return Err(Failure::Malformed("expected a confirmation"));
        };
        if !is_authentic(&self.client.public_key, &key, Role::Client, &transcript, &proof, &signature) {
            return Err(Failure::ProofFailed);
        }
        self.limiter.succeed(self.remote_ip, self.ticket);

        remember(local, &self.client)?;
        Ok(self.client.into_peer())
    }

    fn reason_to_refuse(&self, local: &Local<'_>) -> Result<Option<Reason>, Failure> {
        let reason = if self.protocol != local.protocol {
            Some(Reason::ProtocolMismatch { expected: local.protocol })
        } else if self.secured != local.passphrase.is_some() {
            Some(Reason::SecurityMismatch)
        } else if self.client.public_key == local.identity.public_key() {
            Some(Reason::SelfConnection)
        } else if is_revoked(local.trust, &self.client.public_key)? {
            Some(Reason::Revoked)
        } else {
            None
        };
        Ok(reason)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Frame {
    Hello {
        protocol: u32,
        secured: bool,
        name: String,
        #[serde(with = "hex_bytes")]
        public_key: Vec<u8>,
        #[serde(with = "hex_bytes")]
        pake: Vec<u8>,
        intro: String,
    },
    Reply {
        name: String,
        #[serde(with = "hex_bytes")]
        public_key: Vec<u8>,
        #[serde(with = "hex_bytes")]
        pake: Vec<u8>,
        intro: String,
        #[serde(with = "hex_bytes")]
        proof: Vec<u8>,
        #[serde(with = "hex_bytes")]
        signature: Vec<u8>,
    },
    Confirm {
        #[serde(with = "hex_bytes")]
        proof: Vec<u8>,
        #[serde(with = "hex_bytes")]
        signature: Vec<u8>,
    },
    Rejected {
        reason: Reason,
    },
}

#[derive(Clone, Copy)]
enum Role {
    Client,
    Server,
}

impl Role {
    fn label(self) -> &'static [u8] {
        match self {
            Role::Client => b"client",
            Role::Server => b"server",
        }
    }
}

// What one side sent, exactly as sent: the transcript must match byte for byte.
struct Side {
    name: String,
    public_key: VerifyingKey,
    pake: Vec<u8>,
    intro: String,
}

impl Side {
    fn start(role: Role, local: &Local<'_>) -> (Spake2<Ed25519Group>, Self) {
        let password = Password::new(local.passphrase.unwrap_or("").as_bytes());
        let client = SpakeIdentity::new(CLIENT_SPAKE_IDENTITY);
        let server = SpakeIdentity::new(SERVER_SPAKE_IDENTITY);
        let (pake, message) = match role {
            Role::Client => Spake2::<Ed25519Group>::start_a(&password, &client, &server),
            Role::Server => Spake2::<Ed25519Group>::start_b(&password, &client, &server),
        };
        let side = Side {
            name: local.name.to_string(),
            public_key: local.identity.public_key(),
            pake: message,
            intro: local.intro.to_string(),
        };
        (pake, side)
    }

    fn remote(name: String, public_key: &[u8], pake: Vec<u8>, intro: String) -> Result<Self, Failure> {
        let public_key: [u8; 32] = public_key.try_into().map_err(|_| Failure::Malformed("a public key is 32 bytes"))?;
        let public_key = VerifyingKey::from_bytes(&public_key).map_err(|_| Failure::Malformed("invalid public key"))?;
        if pake.len() > MAX_PAKE_MESSAGE_BYTES {
            return Err(Failure::Malformed("SPAKE2 message too long"));
        }
        if intro.len() > MAX_INTRO_BYTES {
            return Err(Failure::Malformed("intro too long"));
        }
        Ok(Side { name, public_key, pake, intro })
    }

    fn into_peer(self) -> Peer {
        Peer {
            node_id: identity::node_id_for(&self.public_key),
            public_key: self.public_key,
            name: validate::clean_label(&self.name, validate::MAX_NODE_NAME_CHARS),
            intro: self.intro,
        }
    }
}

struct Transcript(digest::Digest);

impl Transcript {
    fn new(protocol: u32, secured: bool, channel_binding: &[u8], client: &Side, server: &Side) -> Self {
        let mut context = digest::Context::new(&digest::SHA256);
        let fields: [&[u8]; 12] = [
            TRANSCRIPT_LABEL,
            &protocol.to_be_bytes(),
            &[u8::from(secured)],
            channel_binding,
            client.name.as_bytes(),
            client.public_key.as_bytes(),
            &client.pake,
            client.intro.as_bytes(),
            server.name.as_bytes(),
            server.public_key.as_bytes(),
            &server.pake,
            server.intro.as_bytes(),
        ];
        // Length prefixes stop bytes being shifted from one field to the next.
        for field in fields {
            context.update(&(field.len() as u32).to_be_bytes());
            context.update(field);
        }
        Self(context.finish())
    }

    fn input(&self, label: &[u8], role: Role) -> Vec<u8> {
        [label, b":", role.label(), b":", self.0.as_ref()].concat()
    }
}

fn authenticate(identity: &Identity, key: &hmac::Key, role: Role, transcript: &Transcript) -> (Vec<u8>, Vec<u8>) {
    let proof = hmac::sign(key, &transcript.input(PROOF_LABEL, role)).as_ref().to_vec();
    let signature = identity.sign(&transcript.input(SIGNATURE_LABEL, role)).to_bytes().to_vec();
    (proof, signature)
}

fn is_authentic(public_key: &VerifyingKey, key: &hmac::Key, role: Role, transcript: &Transcript, proof: &[u8], signature: &[u8]) -> bool {
    let knows_passphrase = hmac::verify(key, &transcript.input(PROOF_LABEL, role), proof).is_ok();
    let holds_key = Signature::from_slice(signature)
        .is_ok_and(|signature| identity::verify(public_key, &transcript.input(SIGNATURE_LABEL, role), &signature));
    knows_passphrase && holds_key
}

fn is_revoked(trust: &TrustStore, public_key: &VerifyingKey) -> Result<bool, Failure> {
    let standing = trust.standing(public_key).map_err(Failure::TrustStore)?;
    Ok(matches!(standing, Standing::Revoked))
}

// An open mesh proves nothing about who joined it, so only a secured one vouches for a node.
fn remember(local: &Local<'_>, peer: &Side) -> Result<(), Failure> {
    if local.passphrase.is_none() {
        return Ok(());
    }
    local
        .trust
        .trust(&peer.public_key, &peer.name, TrustedVia::Passphrase, crate::hlc::wall_clock_ms())
        .map_err(Failure::TrustStore)?;
    Ok(())
}

async fn reject<T: Transport>(transport: &mut T, reason: Reason) -> Failure {
    // The dialer may already be gone; the refusal stands whether or not it hears why.
    if let Err(e) = send_frame(transport, &Frame::Rejected { reason: reason.clone() }).await {
        tracing::debug!("Handshake: could not tell the dialer it was refused ({reason}): {e}");
    }
    Failure::Refused(reason)
}

async fn send_frame<T: Transport>(transport: &mut T, frame: &Frame) -> Result<(), Failure> {
    let bytes = serde_json::to_vec(frame).expect("handshake frames always serialize");
    transport.send(bytes).await.map_err(Failure::Transport)
}

async fn recv_frame<T: Transport>(transport: &mut T) -> Result<Frame, Failure> {
    let received = tokio::time::timeout(STEP_TIMEOUT, transport.recv()).await.map_err(|_| Failure::TimedOut)?;
    let bytes = received.map_err(Failure::Transport)?.ok_or(Failure::Closed)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(Failure::Malformed("frame too large"));
    }
    serde_json::from_slice(&bytes).map_err(|_| Failure::Malformed("unreadable frame"))
}

mod hex_bytes {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        hex::decode(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ratelimit::Policy;
    use std::net::Ipv4Addr;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    const PROTOCOL: u32 = 7;
    const DIALER_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20));
    const CERT: &[u8] = b"server certificate fingerprint";

    struct Pipe {
        outgoing: mpsc::UnboundedSender<Vec<u8>>,
        incoming: mpsc::UnboundedReceiver<Vec<u8>>,
    }

    impl Transport for Pipe {
        async fn send(&mut self, frame: Vec<u8>) -> io::Result<()> {
            self.outgoing.send(frame).map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
        }

        async fn recv(&mut self) -> io::Result<Option<Vec<u8>>> {
            Ok(self.incoming.recv().await)
        }
    }

    fn pipe() -> (Pipe, Pipe) {
        let (a_tx, a_rx) = mpsc::unbounded_channel();
        let (b_tx, b_rx) = mpsc::unbounded_channel();
        (Pipe { outgoing: a_tx, incoming: b_rx }, Pipe { outgoing: b_tx, incoming: a_rx })
    }

    type Tamper = Box<dyn Fn(Vec<u8>) -> Vec<u8> + Send>;

    // A middlebox between dialer and server that passes frames on, changing the dialer's with `tamper`.
    fn relay(tamper: Tamper) -> (Pipe, Pipe) {
        let (dialer, mut dialer_side) = pipe();
        let (mut server_side, server) = pipe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    frame = dialer_side.incoming.recv() => match frame {
                        Some(frame) => { let _ = server_side.outgoing.send(tamper(frame)); }
                        None => break,
                    },
                    frame = server_side.incoming.recv() => match frame {
                        Some(frame) => { let _ = dialer_side.outgoing.send(frame); }
                        None => break,
                    },
                }
            }
        });
        (dialer, server)
    }

    // A pipe that also keeps a copy of every frame the dialer sends.
    fn recording_pipe() -> (Pipe, Pipe, Arc<Mutex<Vec<Vec<u8>>>>) {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let log = recorded.clone();
        let (dialer, server) = relay(Box::new(move |frame| {
            log.lock().unwrap().push(frame.clone());
            frame
        }));
        (dialer, server, recorded)
    }

    struct Node {
        identity: Identity,
        name: String,
        passphrase: Option<String>,
        channel_binding: Vec<u8>,
        trust: TrustStore,
        limiter: AttemptLimiter,
        _dir: tempfile::TempDir,
    }

    impl Node {
        fn new(name: &str, passphrase: Option<&str>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            Node {
                identity: Identity::generate(),
                name: name.to_string(),
                passphrase: passphrase.map(String::from),
                channel_binding: CERT.to_vec(),
                trust: TrustStore::open(dir.path()).unwrap(),
                limiter: AttemptLimiter::new(Policy {
                    free_attempts: 2,
                    base_lockout: Duration::from_secs(60),
                    max_lockout: Duration::from_secs(600),
                    global_cap: None,
                }),
                _dir: dir,
            }
        }

        fn binding(mut self, channel_binding: &[u8]) -> Self {
            self.channel_binding = channel_binding.to_vec();
            self
        }

        fn local(&self) -> Local<'_> {
            Local {
                identity: &self.identity,
                name: &self.name,
                passphrase: self.passphrase.as_deref(),
                protocol: PROTOCOL,
                intro: "intro from the node",
                channel_binding: &self.channel_binding,
                trust: &self.trust,
            }
        }

        fn node_id(&self) -> NodeId {
            self.identity.node_id().to_string()
        }

        async fn serve(&self, transport: &mut Pipe) -> Result<Peer, Failure> {
            receive_hello(transport, &self.limiter, DIALER_IP).await?.accept(transport, &self.local()).await
        }
    }

    async fn handshake_over(dialer: &Node, server: &Node, (mut a, mut b): (Pipe, Pipe)) -> (Result<Peer, Failure>, Result<Peer, Failure>) {
        let dialing = async {
            let outcome = connect(&mut a, &dialer.local()).await;
            drop(a);
            outcome
        };
        tokio::join!(dialing, server.serve(&mut b))
    }

    async fn handshake(dialer: &Node, server: &Node) -> (Result<Peer, Failure>, Result<Peer, Failure>) {
        handshake_over(dialer, server, pipe()).await
    }

    #[tokio::test]
    async fn the_same_passphrase_succeeds_and_each_side_learns_the_others_id() {
        let (dialer, server) = (Node::new("laptop", Some("pw")), Node::new("desktop", Some("pw")));
        let (dialed, served) = handshake(&dialer, &server).await;
        let (dialed, served) = (dialed.unwrap(), served.unwrap());
        assert_eq!((dialed.node_id, dialed.name), (server.node_id(), "desktop".to_string()));
        assert_eq!((served.node_id, served.name), (dialer.node_id(), "laptop".to_string()));
    }

    #[tokio::test]
    async fn each_side_receives_the_others_intro() {
        let (dialer, server) = (Node::new("a", Some("pw")), Node::new("b", Some("pw")));
        let (dialed, served) = handshake(&dialer, &server).await;
        assert_eq!(dialed.unwrap().intro, "intro from the node");
        assert_eq!(served.unwrap().intro, "intro from the node");
    }

    #[tokio::test]
    async fn a_wrong_passphrase_fails_on_both_sides() {
        let (dialer, server) = (Node::new("a", Some("right")), Node::new("b", Some("wrong")));
        let (dialed, served) = handshake(&dialer, &server).await;
        assert!(matches!(dialed, Err(Failure::ProofFailed)));
        assert!(served.is_err());
    }

    #[tokio::test]
    async fn an_open_mesh_without_a_passphrase_connects() {
        let (dialer, server) = (Node::new("a", None), Node::new("b", None));
        let (dialed, served) = handshake(&dialer, &server).await;
        assert!(dialed.is_ok() && served.is_ok());
    }

    #[tokio::test]
    async fn a_man_in_the_middle_who_terminates_tls_twice_fails() {
        let dialer = Node::new("a", Some("pw")).binding(b"attacker certificate");
        let server = Node::new("b", Some("pw"));
        let (dialed, served) = handshake(&dialer, &server).await;
        assert!(matches!(dialed, Err(Failure::ProofFailed)));
        assert!(served.is_err());
    }

    #[tokio::test]
    async fn a_relay_that_changes_the_dialers_intro_fails() {
        let (dialer, server) = (Node::new("a", Some("pw")), Node::new("b", Some("pw")));
        let rewrite = |frame: Vec<u8>| String::from_utf8(frame).unwrap().replace("intro from the node", "forged intro").into_bytes();
        let (dialed, served) = handshake_over(&dialer, &server, relay(Box::new(rewrite))).await;
        assert!(matches!(dialed, Err(Failure::ProofFailed)));
        assert!(served.is_err());
    }

    #[tokio::test]
    async fn a_replayed_dialer_transcript_fails() {
        let (dialer, server) = (Node::new("a", Some("pw")), Node::new("b", Some("pw")));
        let (a, b, recorded) = recording_pipe();
        let (dialed, _) = handshake_over(&dialer, &server, (a, b)).await;
        assert!(dialed.is_ok());

        let (mut replayer, mut server_end) = pipe();
        let frames: Vec<Vec<u8>> = recorded.lock().unwrap().drain(..).collect();
        for frame in frames {
            replayer.send(frame).await.unwrap();
        }
        assert!(matches!(server.serve(&mut server_end).await, Err(Failure::ProofFailed)));
    }

    #[tokio::test]
    async fn the_wire_never_carries_the_passphrase() {
        let passphrase = "correct horse battery staple";
        let (dialer, server) = (Node::new("a", Some(passphrase)), Node::new("b", Some(passphrase)));
        let (a, b, recorded) = recording_pipe();
        handshake_over(&dialer, &server, (a, b)).await.0.unwrap();
        let wire = recorded.lock().unwrap().concat();
        let hashed = hex::encode(digest::digest(&digest::SHA256, passphrase.as_bytes()));
        assert!(!String::from_utf8_lossy(&wire).contains(passphrase));
        assert!(!String::from_utf8_lossy(&wire).contains(&hashed));
    }

    #[tokio::test]
    async fn a_node_cannot_claim_a_key_it_does_not_hold() {
        // The impostor knows the passphrase and sends the victim's public key, but can only sign with its own.
        let (victim, server) = (Node::new("victim", Some("pw")), Node::new("b", Some("pw")));
        let impostor = Identity::generate();
        let (mut a, mut b) = pipe();
        let impersonating = async {
            let local = victim.local();
            let (pake, client) = Side::start(Role::Client, &local);
            let hello = Frame::Hello {
                protocol: PROTOCOL, secured: true, name: client.name.clone(),
                public_key: client.public_key.as_bytes().to_vec(), pake: client.pake.clone(), intro: client.intro.clone(),
            };
            send_frame(&mut a, &hello).await.unwrap();
            let Frame::Reply { name, public_key, pake: server_pake, intro, .. } = recv_frame(&mut a).await.unwrap() else { panic!("expected a reply") };
            let server_side = Side::remote(name, &public_key, server_pake, intro).unwrap();
            let transcript = Transcript::new(PROTOCOL, true, CERT, &client, &server_side);
            let key = hmac::Key::new(hmac::HMAC_SHA256, &pake.finish(&server_side.pake).unwrap());
            let (proof, signature) = authenticate(&impostor, &key, Role::Client, &transcript);
            send_frame(&mut a, &Frame::Confirm { proof, signature }).await.unwrap();
        };
        let (_, served) = tokio::join!(impersonating, server.serve(&mut b));
        assert!(matches!(served, Err(Failure::ProofFailed)));
    }

    #[tokio::test]
    async fn a_secured_handshake_trusts_both_nodes_by_passphrase() {
        let (dialer, server) = (Node::new("laptop", Some("pw")), Node::new("desktop", Some("pw")));
        handshake(&dialer, &server).await.0.unwrap();
        let Standing::Trusted(on_server) = server.trust.standing(&dialer.identity.public_key()).unwrap() else { panic!("not trusted") };
        assert_eq!((on_server.trusted_via, on_server.name.as_str()), (TrustedVia::Passphrase, "laptop"));
        assert!(matches!(dialer.trust.standing(&server.identity.public_key()).unwrap(), Standing::Trusted(_)));
    }

    #[tokio::test]
    async fn an_open_handshake_trusts_nobody() {
        let (dialer, server) = (Node::new("a", None), Node::new("b", None));
        handshake(&dialer, &server).await.0.unwrap();
        assert!(server.trust.list().unwrap().is_empty());
        assert!(dialer.trust.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_handshake_trusts_nobody() {
        let (dialer, server) = (Node::new("a", Some("right")), Node::new("b", Some("wrong")));
        let _ = handshake(&dialer, &server).await;
        assert!(server.trust.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_revoked_key_is_rejected_on_reconnect_even_with_the_passphrase() {
        let (dialer, server) = (Node::new("a", Some("pw")), Node::new("b", Some("pw")));
        handshake(&dialer, &server).await.0.unwrap();
        server.trust.revoke(&dialer.node_id()).unwrap();

        let (dialed, served) = handshake(&dialer, &server).await;
        assert!(matches!(dialed, Err(Failure::RejectedBy(Reason::Revoked))));
        assert!(matches!(served, Err(Failure::Refused(Reason::Revoked))));
    }

    #[tokio::test]
    async fn a_dialer_refuses_a_revoked_server() {
        let (dialer, server) = (Node::new("a", Some("pw")), Node::new("b", Some("pw")));
        handshake(&dialer, &server).await.0.unwrap();
        dialer.trust.revoke(&server.node_id()).unwrap();

        let (dialed, _) = handshake(&dialer, &server).await;
        assert!(matches!(dialed, Err(Failure::Refused(Reason::Revoked))));
    }

    #[tokio::test]
    async fn a_different_protocol_version_is_refused() {
        let (dialer, server) = (Node::new("a", Some("pw")), Node::new("b", Some("pw")));
        let (mut a, mut b) = pipe();
        let newer = Local { protocol: PROTOCOL + 1, ..dialer.local() };
        let (dialed, _) = tokio::join!(connect(&mut a, &newer), server.serve(&mut b));
        assert!(matches!(dialed, Err(Failure::RejectedBy(Reason::ProtocolMismatch { expected: PROTOCOL }))));
    }

    #[tokio::test]
    async fn a_secured_and_an_open_node_refuse_each_other() {
        let (dialer, server) = (Node::new("a", None), Node::new("b", Some("pw")));
        let (dialed, _) = handshake(&dialer, &server).await;
        let failure = dialed.unwrap_err();
        assert!(matches!(failure, Failure::RejectedBy(Reason::SecurityMismatch)));
        assert!(failure.is_authentication());
    }

    #[tokio::test]
    async fn a_node_dialing_itself_is_refused() {
        let node = Node::new("a", Some("pw"));
        let (dialed, _) = handshake(&node, &node).await;
        assert!(matches!(dialed, Err(Failure::RejectedBy(Reason::SelfConnection))));
    }

    #[tokio::test]
    async fn wrong_guesses_lock_the_dialers_ip_out() {
        let (guesser, server) = (Node::new("a", Some("guess")), Node::new("b", Some("pw")));
        for _ in 0..3 {
            let _ = handshake(&guesser, &server).await;
        }
        let (dialed, served) = handshake(&guesser, &server).await;
        let failure = dialed.unwrap_err();
        assert!(matches!(failure, Failure::RejectedBy(Reason::RateLimited { .. })));
        assert!(failure.is_authentication());
        assert!(matches!(served, Err(Failure::Refused(Reason::RateLimited { .. }))));
    }

    #[tokio::test]
    async fn a_locked_out_ip_is_refused_even_with_the_right_passphrase() {
        let (guesser, honest, server) = (Node::new("a", Some("guess")), Node::new("c", Some("pw")), Node::new("b", Some("pw")));
        for _ in 0..3 {
            let _ = handshake(&guesser, &server).await;
        }
        let (dialed, _) = handshake(&honest, &server).await;
        assert!(matches!(dialed, Err(Failure::RejectedBy(Reason::RateLimited { .. }))));
    }

    #[tokio::test]
    async fn successful_handshakes_never_lock_an_ip_out() {
        let (dialer, server) = (Node::new("a", Some("pw")), Node::new("b", Some("pw")));
        for _ in 0..10 {
            handshake(&dialer, &server).await.1.unwrap();
        }
    }

    #[tokio::test]
    async fn a_refused_duplicate_tells_the_dialer_why() {
        let (dialer, server) = (Node::new("a", Some("pw")), Node::new("b", Some("pw")));
        let (mut a, mut b) = pipe();
        let refusing = async {
            let incoming = receive_hello(&mut b, &server.limiter, DIALER_IP).await.unwrap();
            assert_eq!(incoming.claimed_node_id(), dialer.node_id());
            incoming.refuse(&mut b, Reason::Duplicate).await
        };
        let local = dialer.local();
        let (dialed, _) = tokio::join!(connect(&mut a, &local), refusing);
        let failure = dialed.unwrap_err();
        assert!(matches!(failure, Failure::RejectedBy(Reason::Duplicate)));
        assert!(!failure.is_authentication());
    }

    #[tokio::test]
    async fn garbage_instead_of_a_hello_is_malformed() {
        let server = Node::new("b", Some("pw"));
        let (mut a, mut b) = pipe();
        a.send(b"not json".to_vec()).await.unwrap();
        assert!(matches!(server.serve(&mut b).await, Err(Failure::Malformed(_))));
    }

    #[tokio::test]
    async fn an_oversized_frame_is_malformed() {
        let server = Node::new("b", Some("pw"));
        let (mut a, mut b) = pipe();
        a.send(vec![b' '; MAX_FRAME_BYTES + 1]).await.unwrap();
        assert!(matches!(server.serve(&mut b).await, Err(Failure::Malformed("frame too large"))));
    }

    #[tokio::test]
    async fn a_dialer_that_hangs_up_is_reported_as_closed() {
        let server = Node::new("b", Some("pw"));
        let (a, mut b) = pipe();
        drop(a);
        assert!(matches!(server.serve(&mut b).await, Err(Failure::Closed)));
    }

    #[test]
    fn transcript_fields_cannot_be_shifted_between_each_other() {
        let identity = Identity::generate();
        let side = |name: &str, intro: &str| Side { name: name.into(), public_key: identity.public_key(), pake: vec![], intro: intro.into() };
        let shifted_left = Transcript::new(1, true, b"", &side("ab", ""), &side("c", ""));
        let shifted_right = Transcript::new(1, true, b"", &side("a", "b"), &side("c", ""));
        assert_ne!(shifted_left.0.as_ref(), shifted_right.0.as_ref());
    }

    #[test]
    fn a_proof_for_one_role_does_not_pass_for_the_other() {
        let identity = Identity::generate();
        let side = Side { name: "a".into(), public_key: identity.public_key(), pake: vec![], intro: String::new() };
        let transcript = Transcript::new(1, true, b"", &side, &side);
        let key = hmac::Key::new(hmac::HMAC_SHA256, b"shared key");
        let (proof, signature) = authenticate(&identity, &key, Role::Client, &transcript);
        assert!(is_authentic(&identity.public_key(), &key, Role::Client, &transcript, &proof, &signature));
        assert!(!is_authentic(&identity.public_key(), &key, Role::Server, &transcript, &proof, &signature));
    }
}
