//! The loopback socket the commands connect to, and the file that says where it is and holds the token they must show.

use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use super::{version_mismatch_message, Event, Events, Reply, Request, Settings, VERSION};
use crate::{persist, NodeState};

const FILE_NAME: &str = "control.json";
const MAX_REQUEST_BYTES: u64 = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_QUEUE: usize = 32;

/// Where a running node listens for commands. The file holding it is readable only by the user who started the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
}

// The first and only line a command sends.
#[derive(Serialize, Deserialize)]
struct Hello {
    token: String,
    // Commands from before the version check do not send one.
    #[serde(default)]
    version: String,
    request: Request,
}

fn path_in(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE_NAME)
}

impl Endpoint {
    /// The endpoint of the node using this data directory, if one has run there; it may no longer be running.
    pub fn load(data_dir: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(path_in(data_dir)).ok()?).ok()
    }

    fn accepts(&self, token: &str) -> bool {
        self.token.as_bytes().ct_eq(token.as_bytes()).into()
    }
}

/// Opens the socket on a free loopback port and records it, with a fresh token, in the data directory.
pub async fn open(data_dir: &Path) -> io::Result<(TcpListener, Endpoint)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let endpoint = Endpoint { port: listener.local_addr()?.port(), token: hex::encode(rand::random::<[u8; 32]>()) };
    persist::write_private(&path_in(data_dir), &serde_json::to_vec(&endpoint).map_err(io::Error::other)?)?;
    Ok((listener, endpoint))
}

/// Forgets the endpoint, so commands say plainly that no node is running; the owner of the data directory calls this.
pub fn close(data_dir: &Path) {
    match std::fs::remove_file(path_in(data_dir)) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("Control: could not remove {}: {e}", path_in(data_dir).display()),
    }
}

/// Answers commands until the node stops.
pub async fn serve(state: NodeState, settings: Settings, listener: TcpListener, endpoint: Endpoint) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                tracing::warn!("Control: accept error: {e}");
                continue;
            }
        };
        let (state, settings, endpoint) = (state.clone(), settings.clone(), endpoint.clone());
        tokio::spawn(async move {
            if let Err(e) = serve_command(state, settings, stream, &endpoint).await {
                tracing::debug!("Control: a command ended early: {e:#}");
            }
        });
    }
}

async fn read_line(stream: &mut (impl tokio::io::AsyncRead + Unpin)) -> anyhow::Result<String> {
    let mut line = String::new();
    let mut limited = BufReader::new(stream.take(MAX_REQUEST_BYTES));
    tokio::time::timeout(REQUEST_TIMEOUT, limited.read_line(&mut line)).await.context("no request arrived in time")??;
    Ok(line)
}

async fn write_event(stream: &mut (impl tokio::io::AsyncWrite + Unpin), event: &Event) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    Ok(())
}

async fn serve_command(state: NodeState, settings: Settings, mut stream: TcpStream, endpoint: &Endpoint) -> anyhow::Result<()> {
    let (mut reading, mut writing) = stream.split();
    let hello = serde_json::from_str::<Hello>(&read_line(&mut reading).await?);
    let hello = match hello {
        Ok(hello) if endpoint.accepts(&hello.token) => hello,
        // The same answer either way, so a stranger learns nothing about what the token should look like.
        _ => return write_event(&mut writing, &Event::Failed { message: "this command may not control the node".into() }).await,
    };
    if hello.version != VERSION {
        return write_event(&mut writing, &Event::Failed { message: version_mismatch_message(VERSION, &hello.version) }).await;
    }
    write_event(&mut writing, &Event::Welcome { version: VERSION.to_string() }).await?;
    let request = hello.request;

    // The work runs in a task of its own: an offer to a device stands even after the command that made it has gone.
    let (events, mut outbox) = mpsc::channel(EVENT_QUEUE);
    tokio::spawn(async move { super::answer(&state, &settings, request, Events::new(events)).await });
    while let Some(event) = outbox.recv().await {
        write_event(&mut writing, &event).await?;
    }
    Ok(())
}

/// Sends one request to a running node, passing on each progress event; returns its reply.
pub async fn call(endpoint: &Endpoint, request: &Request, mut on_event: impl FnMut(&Event)) -> anyhow::Result<Reply> {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint.port)).await?;
    let mut hello = serde_json::to_vec(&Hello { token: endpoint.token.clone(), version: VERSION.to_string(), request: request.clone() })?;
    hello.push(b'\n');
    stream.write_all(&hello).await?;

    let mut lines = BufReader::new(stream).lines();
    ensure_node_matches(&mut lines).await?;
    while let Some(line) = lines.next_line().await? {
        match parse_event(&line)? {
            Event::Done { reply } => return Ok(*reply),
            Event::Failed { message } => bail!(message),
            Event::Welcome { .. } => bail!("the node sent something this command does not understand"),
            event => on_event(&event),
        }
    }
    bail!("the node stopped before it answered")
}

fn parse_event(line: &str) -> anyhow::Result<Event> {
    serde_json::from_str(line).context("the node sent something this command does not understand")
}

// A node from before the version check never says hello, so anything else first means it is older.
async fn ensure_node_matches(lines: &mut tokio::io::Lines<BufReader<TcpStream>>) -> anyhow::Result<()> {
    let first = lines.next_line().await?.context("the node stopped before it answered")?;
    match parse_event(&first) {
        Ok(Event::Welcome { version }) if version == VERSION => Ok(()),
        Ok(Event::Welcome { version }) => bail!(version_mismatch_message(&version, VERSION)),
        Ok(Event::Failed { message }) => bail!(message),
        _ => bail!(version_mismatch_message("", VERSION)),
    }
}
