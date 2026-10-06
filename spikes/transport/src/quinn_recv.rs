use std::fs::File;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context};
use quinn::Connection;

use crate::manifest::{chunk_offset, Manifest, CHUNK_SIZE};
use crate::quinn_tls::{self, NodeCert, SERVER_NAME};
use crate::report::{secs, Report};
use crate::wire::{Request, MAX_MANIFEST_LEN};

pub struct Options {
    pub listen: SocketAddr,
    pub streams: u32,
    pub batch: u32,
}

struct Download {
    connection: Connection,
    manifest: Manifest,
    file: File,
    next_chunk: AtomicU32,
    batch: u32,
}

pub async fn run(ticket: &str, out: &Path, options: Options) -> anyhow::Result<()> {
    let (sender, fingerprint) = parse_ticket(ticket)?;
    let cert = NodeCert::generate()?;
    let mut endpoint = quinn::Endpoint::client(options.listen)?;
    endpoint.set_default_client_config(quinn_tls::client_config(&cert, fingerprint)?);

    let connect_started = Instant::now();
    let connection = endpoint.connect(sender, SERVER_NAME)?.await?;
    let connect_secs = secs(connect_started.elapsed());

    let started = Instant::now();
    let manifest = fetch_manifest(&connection).await?;
    let file = File::create(out)?;
    file.set_len(manifest.size)?;
    let download = Arc::new(Download { connection, manifest, file, next_chunk: AtomicU32::new(0), batch: options.batch });
    fetch_all_chunks(&download, options.streams).await?;
    let transfer_secs = secs(started.elapsed());

    let finalize_started = Instant::now();
    download.file.sync_all()?;
    let finalize_secs = secs(finalize_started.elapsed());

    download.connection.close(0u32.into(), b"done");
    endpoint.wait_idle().await;
    Report {
        transport: "quinn",
        role: "recv",
        bytes: download.manifest.size,
        prepare_secs: 0.0,
        connect_secs,
        transfer_secs,
        finalize_secs,
    }
    .print();
    Ok(())
}

fn parse_ticket(ticket: &str) -> anyhow::Result<(SocketAddr, String)> {
    let (address, fingerprint) = ticket.rsplit_once('/').context("a quinn ticket looks like <ip:port>/<fingerprint>")?;
    Ok((address.parse()?, fingerprint.to_owned()))
}

async fn fetch_manifest(connection: &Connection) -> anyhow::Result<Manifest> {
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&Request::GetManifest.encode()).await?;
    send.finish()?;
    Manifest::decode(&recv.read_to_end(MAX_MANIFEST_LEN).await?)
}

/// Each worker keeps one request stream in flight, so `streams` is the pipelining depth.
async fn fetch_all_chunks(download: &Arc<Download>, streams: u32) -> anyhow::Result<()> {
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..streams {
        let download = download.clone();
        workers.spawn(async move { fetch_batches_until_done(&download).await });
    }
    while let Some(result) = workers.join_next().await {
        result??;
    }
    Ok(())
}

async fn fetch_batches_until_done(download: &Arc<Download>) -> anyhow::Result<()> {
    let total = download.manifest.chunk_count();
    let mut buffer = vec![0u8; CHUNK_SIZE as usize];
    loop {
        let start = download.next_chunk.fetch_add(download.batch, Ordering::Relaxed);
        if start >= total {
            return Ok(());
        }
        let count = download.batch.min(total - start);
        buffer = fetch_batch(download, start, count, buffer).await?;
    }
}

async fn fetch_batch(download: &Arc<Download>, start: u32, count: u32, mut buffer: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let (mut send, mut recv) = download.connection.open_bi().await?;
    send.write_all(&Request::GetChunks { start, count }.encode()).await?;
    send.finish()?;
    for index in start..start + count {
        let length = download.manifest.chunk_len(index);
        recv.read_exact(&mut buffer[..length]).await?;
        buffer = verify_and_write(download.clone(), index, buffer).await?;
    }
    Ok(buffer)
}

/// Hashing and disk writes run off the async threads so the network tasks never stall on them.
async fn verify_and_write(download: Arc<Download>, index: u32, buffer: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let chunk = &buffer[..download.manifest.chunk_len(index)];
        if blake3::hash(chunk) != download.manifest.chunk_hashes[index as usize] {
            bail!("chunk {index} failed BLAKE3 verification");
        }
        crate::positional::write_all_at(&download.file, chunk, chunk_offset(index))?;
        Ok(buffer)
    })
    .await?
}
