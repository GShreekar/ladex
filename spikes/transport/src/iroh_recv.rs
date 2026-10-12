use std::net::SocketAddr;
use std::path::Path;
use std::time::Instant;

use anyhow::anyhow;
use iroh_blobs::api::blobs::{ExportMode, ExportOptions};
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::ticket::BlobTicket;

use crate::iroh_send::lan_only_endpoint;
use crate::report::{secs, Report};

pub async fn run(ticket: &str, out: &Path, store_dir: &Path, listen: SocketAddr) -> anyhow::Result<()> {
    let ticket: BlobTicket = ticket.parse()?;
    let store = FsStore::load(store_dir).await.map_err(|error| anyhow!("loading the blob store: {error:#}"))?;
    let endpoint = lan_only_endpoint(listen).await?;

    let connect_started = Instant::now();
    let connection = endpoint.connect(ticket.addr().clone(), iroh_blobs::ALPN).await?;
    let connect_secs = secs(connect_started.elapsed());

    let started = Instant::now();
    // iroh-blobs verifies every 16 KiB chunk group against the BLAKE3 root hash as it arrives.
    let stats = store.remote().fetch(connection.clone(), ticket.hash_and_format()).complete().await?;
    let transfer_secs = secs(started.elapsed());
    connection.close(0u32.into(), b"done");

    let finalize_started = Instant::now();
    let export = ExportOptions { hash: ticket.hash(), mode: ExportMode::TryReference, target: std::path::absolute(out)? };
    let bytes = store.blobs().export_with_opts(export).finish().await?;
    // Match the quinn receiver, which fsyncs before reporting.
    std::fs::File::open(out)?.sync_all()?;
    let finalize_secs = secs(finalize_started.elapsed());

    store.shutdown().await?;
    endpoint.close().await;
    eprintln!("iroh stats: {} payload bytes read", stats.payload_bytes_read);
    Report { transport: "iroh", role: "recv", bytes, prepare_secs: 0.0, connect_secs, transfer_secs, finalize_secs }.print();
    Ok(())
}
