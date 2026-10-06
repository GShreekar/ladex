use std::fs::File;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context};
use quinn::{Connection, ConnectionError, RecvStream, SendStream};

use crate::manifest::{chunk_offset, Manifest, CHUNK_SIZE};
use crate::quinn_tls::{self, NodeCert};
use crate::report::{secs, Report};
use crate::wire::{Request, REQUEST_LEN};

struct SharedFile {
    file: File,
    manifest: Manifest,
    encoded_manifest: Vec<u8>,
}

pub async fn run(path: &Path, listen: SocketAddr, ticket_file: Option<&Path>) -> anyhow::Result<()> {
    let prepare_started = Instant::now();
    let manifest = tokio::task::spawn_blocking({
        let path = path.to_owned();
        move || Manifest::build(&path)
    })
    .await??;
    let prepare_secs = secs(prepare_started.elapsed());
    let shared = Arc::new(SharedFile { file: File::open(path)?, encoded_manifest: manifest.encode(), manifest });

    let cert = NodeCert::generate()?;
    let endpoint = quinn::Endpoint::server(quinn_tls::server_config(&cert)?, listen)?;
    let ticket = format!("{}/{}", endpoint.local_addr()?, cert.fingerprint());
    crate::ticket::publish(&ticket, ticket_file)?;

    let incoming = endpoint.accept().await.context("the endpoint closed before a receiver connected")?;
    let connection = incoming.await?;
    let started = Instant::now();
    serve_connection(&connection, &shared).await?;
    endpoint.wait_idle().await;

    Report {
        transport: "quinn",
        role: "send",
        bytes: shared.manifest.size,
        prepare_secs,
        connect_secs: 0.0,
        transfer_secs: secs(started.elapsed()),
        finalize_secs: 0.0,
    }
    .print();
    Ok(())
}

async fn serve_connection(connection: &Connection, shared: &Arc<SharedFile>) -> anyhow::Result<()> {
    let mut requests = tokio::task::JoinSet::new();
    loop {
        let (send, recv) = match connection.accept_bi().await {
            Ok(streams) => streams,
            Err(ConnectionError::ApplicationClosed(_)) => break,
            Err(error) => return Err(error.into()),
        };
        let shared = shared.clone();
        requests.spawn(async move { serve_request(send, recv, &shared).await });
    }
    while let Some(result) = requests.join_next().await {
        result??;
    }
    Ok(())
}

async fn serve_request(mut send: SendStream, mut recv: RecvStream, shared: &Arc<SharedFile>) -> anyhow::Result<()> {
    let mut bytes = [0u8; REQUEST_LEN];
    recv.read_exact(&mut bytes).await?;
    match Request::decode(bytes)? {
        Request::GetManifest => send.write_all(&shared.encoded_manifest).await?,
        Request::GetChunks { start, count } => send_chunks(&mut send, shared, start, count).await?,
    }
    // quinn keeps retransmitting a finished stream after it is dropped, so there is no need to wait for acks.
    send.finish()?;
    Ok(())
}

async fn send_chunks(send: &mut SendStream, shared: &Arc<SharedFile>, start: u32, count: u32) -> anyhow::Result<()> {
    let Some(end) = start.checked_add(count).filter(|end| *end <= shared.manifest.chunk_count()) else {
        bail!("chunks {start}+{count} are out of range");
    };
    let mut buffer = vec![0u8; CHUNK_SIZE as usize];
    for index in start..end {
        let length = shared.manifest.chunk_len(index);
        buffer = read_chunk(shared.clone(), index, length, buffer).await?;
        send.write_all(&buffer[..length]).await?;
    }
    Ok(())
}

async fn read_chunk(shared: Arc<SharedFile>, index: u32, length: usize, mut buffer: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        crate::positional::read_exact_at(&shared.file, &mut buffer[..length], chunk_offset(index))?;
        Ok(buffer)
    })
    .await?
}
