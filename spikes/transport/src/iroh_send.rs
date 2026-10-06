use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::anyhow;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr};
use iroh_blobs::api::blobs::{AddPathOptions, ImportMode};
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::ticket::BlobTicket;
use iroh_blobs::{BlobFormat, BlobsProtocol};
use tokio::sync::Notify;

use crate::report::{secs, Report};

pub async fn run(path: &Path, store_dir: &Path, listen: SocketAddr, ticket_file: Option<&Path>) -> anyhow::Result<()> {
    let store = FsStore::load(store_dir).await.map_err(|error| anyhow!("loading the blob store: {error:#}"))?;
    let prepare_started = Instant::now();
    let import = AddPathOptions { path: std::path::absolute(path)?, format: BlobFormat::Raw, mode: ImportMode::TryReference };
    let tag = store.blobs().add_path_with_opts(import).await?;
    let prepare_secs = secs(prepare_started.elapsed());
    let size = std::fs::metadata(path)?.len();

    let endpoint = lan_only_endpoint(listen).await?;
    let addr = EndpointAddr::new(endpoint.id()).with_ip_addr(endpoint.bound_sockets()[0]);
    crate::ticket::publish(&BlobTicket::new(addr, tag.hash, tag.format).to_string(), ticket_file)?;

    let served = Arc::new(ServedOnce::default());
    let protocol = ServeOnce { blobs: BlobsProtocol::new(&store, None), served: served.clone() };
    let router = Router::builder(endpoint).accept(iroh_blobs::ALPN, protocol).spawn();
    served.connected.notified().await;
    let started = Instant::now();
    served.finished.notified().await;
    let transfer_secs = secs(started.elapsed());
    router.shutdown().await?;

    Report { transport: "iroh", role: "send", bytes: size, prepare_secs, connect_secs: 0.0, transfer_secs, finalize_secs: 0.0 }.print();
    Ok(())
}

/// Relay, address lookup (DNS/pkarr) and portmapping all off: direct UDP on the given address only.
pub async fn lan_only_endpoint(listen: SocketAddr) -> anyhow::Result<Endpoint> {
    let endpoint = Endpoint::builder(iroh::endpoint::presets::Minimal)
        .relay_mode(iroh::RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr(listen)?
        .bind()
        .await?;
    Ok(endpoint)
}

#[derive(Debug, Default)]
struct ServedOnce {
    connected: Notify,
    finished: Notify,
}

/// Wraps the blobs protocol so the sender can tell when its single receiver is done.
#[derive(Debug)]
struct ServeOnce {
    blobs: BlobsProtocol,
    served: Arc<ServedOnce>,
}

impl ProtocolHandler for ServeOnce {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.served.connected.notify_one();
        let result = self.blobs.accept(connection).await;
        self.served.finished.notify_one();
        result
    }

    async fn shutdown(&self) {
        self.blobs.shutdown().await;
    }
}
