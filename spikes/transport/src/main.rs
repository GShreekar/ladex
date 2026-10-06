//! Transport spike (ADR 0001): a 1 → 1 file transfer over plain quinn and over iroh-blobs.

mod iroh_recv;
mod iroh_send;
mod manifest;
mod positional;
mod quinn_recv;
mod quinn_send;
mod quinn_tls;
mod report;
mod testfile;
mod ticket;
mod wire;

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "quinn vs iroh 1 → 1 transfer spike")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a file of incompressible pseudo-random bytes.
    Gen {
        path: PathBuf,
        #[arg(long, default_value = "4GiB", value_parser = testfile::parse_size)]
        size: u64,
    },
    /// Print the BLAKE3 hash of a whole file.
    Hash { path: PathBuf },
    /// Serve one file over quinn to the first receiver, then exit.
    QuinnSend {
        path: PathBuf,
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: SocketAddr,
        #[arg(long)]
        ticket_file: Option<PathBuf>,
    },
    /// Fetch a file over quinn from the sender named in the ticket.
    QuinnRecv {
        ticket: String,
        out: PathBuf,
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: SocketAddr,
        /// Chunk requests kept in flight, one QUIC stream each.
        #[arg(long, default_value_t = 16)]
        streams: u32,
        /// 1 MiB chunks asked for per request.
        #[arg(long, default_value_t = 4)]
        batch: u32,
    },
    /// Serve one file over iroh-blobs to the first receiver, then exit.
    IrohSend {
        path: PathBuf,
        store_dir: PathBuf,
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: SocketAddr,
        #[arg(long)]
        ticket_file: Option<PathBuf>,
    },
    /// Fetch a file over iroh-blobs from the sender named in the ticket.
    IrohRecv {
        ticket: String,
        out: PathBuf,
        store_dir: PathBuf,
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Gen { path, size } => testfile::generate(&path, size),
        Command::Hash { path } => {
            println!("{}", testfile::hash_file(&path)?);
            Ok(())
        }
        Command::QuinnSend { path, listen, ticket_file } => quinn_send::run(&path, listen, ticket_file.as_deref()).await,
        Command::QuinnRecv { ticket, out, listen, streams, batch } => {
            let options = quinn_recv::Options { listen, streams, batch };
            quinn_recv::run(&ticket, &out, options).await
        }
        Command::IrohSend { path, store_dir, listen, ticket_file } => {
            iroh_send::run(&path, &store_dir, listen, ticket_file.as_deref()).await
        }
        Command::IrohRecv { ticket, out, store_dir, listen } => iroh_recv::run(&ticket, &out, &store_dir, listen).await,
    }
}
