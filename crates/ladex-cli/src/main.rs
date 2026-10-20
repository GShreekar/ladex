use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use include_dir::{include_dir, Dir};
use ladex_core::control::server::{self as control, Endpoint};
use ladex_core::control::{Reply, Request};
use ladex_core::node::{self, Node, NodeConfig, NodeHandle, Notice, StartError, UiAssets};
use ladex_core::{auth, tls};

// The web UI built from `ui/` by build.rs.
static UI_DIR: Dir = include_dir!("$OUT_DIR/ui");

mod autostart;
mod commands;
mod doctor;
mod output;
mod pair;
mod passphrase;
mod receive;

#[derive(Parser)]
#[command(name = "ladex", version)]
#[command(about = "LADEX - Local Area Data Exchange", long_about = None)]
struct Cli {
    /// Without a command, starts a node; a command talks to the node already running on this machine.
    #[command(subcommand)]
    command: Option<commands::Command>,

    #[command(flatten)]
    node: Args,

    /// Where the node keeps its files and settings. Default: $LADEX_DATA_DIR, else ~/.ladex/files
    #[arg(long, global = true)]
    data_dir: Option<std::path::PathBuf>,

    /// Print the result as JSON, for scripts.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(clap::Args)]
struct Args {
    /// Passphrase for browser logins and for joining the mesh; without one, anyone on the network can do both.
    passphrase: Option<String>,

    /// Generate a random passphrase and print it.
    #[arg(short = 's', long = "secure", conflicts_with = "passphrase")]
    secure: bool,

    /// Local HTTP/WS port for this node's own browser tab.
    #[arg(long, default_value = "8080")]
    port: u16,

    /// UDP multicast discovery port.
    #[arg(long, default_value = "7878")]
    discovery_port: u16,

    /// UDP port other nodes connect to over QUIC (default: the same number as --port).
    #[arg(long)]
    quic_port: Option<u16>,

    /// Disable UDP multicast discovery (combine with --peer to connect manually).
    #[arg(long)]
    no_discovery: bool,

    /// Peer node to connect to directly, as <ip>:<quic_port>; repeatable.
    #[arg(long = "peer")]
    manual_peers: Vec<String>,

    /// Serve plain HTTP/WS instead of TLS; remote browsers then lose streamed downloads and integrity checks.
    #[arg(long)]
    no_tls: bool,

    /// Port for a plain-HTTP listener on 127.0.0.1, free of certificate warnings (default: main port + 1; unused with --no-tls).
    #[arg(long)]
    local_port: Option<u16>,

    /// How long a download may wait for the next piece of a file before giving up.
    #[arg(long, default_value = "60")]
    stall_timeout_secs: u64,

    /// Most disk space, in GiB, that shared files may use on this node. Default: the limit set in the app or with `ladex settings`, else 20.
    #[arg(long)]
    storage_limit_gb: Option<u64>,

    /// How long a browser guest stays signed in after joining.
    #[arg(long, default_value = "8")]
    guest_session_hours: u64,

    /// Keep this node's private key in a file in the data directory instead of the OS keychain.
    #[arg(long)]
    no_keychain: bool,
}

struct EmbeddedUi;

impl UiAssets for EmbeddedUi {
    fn file(&self, path: &str) -> Option<&[u8]> {
        UI_DIR.get_file(path).map(|file| file.contents())
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let data_dir = cli.data_dir.unwrap_or_else(node::default_data_dir);
    match cli.command {
        Some(command) => std::process::exit(commands::run(command, &data_dir, cli.json).await),
        None => run_node(cli.node, data_dir).await,
    }
}

async fn run_node(args: Args, data_dir: PathBuf) {
    tracing_subscriber::fmt::init();

    let passphrase = resolve_passphrase(&args);
    let mut config = node_config(&args, data_dir, passphrase);
    apply_saved(&mut config, &args);
    let has_passphrase = config.passphrase.is_some();
    let mut handle = match Node::start(config).await {
        Ok(handle) => handle,
        Err(error) => exit_with_start_error(&error).await,
    };
    print_startup(&handle, has_passphrase);

    tokio::select! {
        result = handle.wait() => {
            if let Err(e) = result {
                tracing::error!("Server exited: {e}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Shutdown: SIGINT received — sending Goodbye to mesh peers");
            handle.shutdown().await;
        }
    }
}

/// What the app or `ladex settings` saved in the data folder fills in what the command line leaves out.
fn apply_saved(config: &mut NodeConfig, args: &Args) {
    match config.use_saved_passphrase() {
        Ok(true) => println!("Using the passphrase saved in {}", config.data_dir.display()),
        Ok(false) => {}
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
    }
    if args.storage_limit_gb.is_none() {
        config.use_saved_storage_limit();
    }
}

/// The passphrase to run with: generated for --secure, otherwise the one given, with a warning if it is weak.
fn resolve_passphrase(args: &Args) -> Option<String> {
    let passphrase = if args.secure {
        let generated = auth::generate_passphrase();
        println!("Generated passphrase: {generated}");
        Some(generated)
    } else {
        args.passphrase.clone()
    };
    match passphrase.as_deref() {
        Some("") => {
            eprintln!("Error: the passphrase must not be empty.");
            std::process::exit(2);
        }
        Some(p) => {
            if let Some(reason) = auth::weakness(p) {
                eprintln!("Warning: this passphrase is weak ({reason}). Prefer `ladex -s` for a generated one.");
            }
        }
        None => {}
    }
    passphrase
}

fn node_config(args: &Args, data_dir: PathBuf, passphrase: Option<String>) -> NodeConfig {
    let mut config = NodeConfig::new(data_dir, Arc::new(EmbeddedUi));
    config.passphrase = passphrase;
    config.http_port = args.port;
    config.quic_port = args.quic_port;
    config.local_http_port = args.local_port;
    config.discovery_port = (!args.no_discovery).then_some(args.discovery_port);
    config.manual_peers = parse_manual_peers(&args.manual_peers);
    config.use_tls = !args.no_tls;
    config.use_keychain = !args.no_keychain;
    if let Some(gib) = args.storage_limit_gb {
        config.storage_limit_bytes = gib.saturating_mul(1 << 30);
    }
    config.stall_timeout = std::time::Duration::from_secs(args.stall_timeout_secs);
    config.guest_session_ttl = std::time::Duration::from_secs(args.guest_session_hours * 60 * 60);
    config
}

fn parse_manual_peers(peers: &[String]) -> Vec<SocketAddr> {
    peers
        .iter()
        .filter_map(|peer| match peer.parse::<SocketAddr>() {
            Ok(addr) => Some(addr),
            Err(_) => {
                eprintln!("Warning: invalid --peer address '{peer}' — expected format: <ip>:<port>");
                None
            }
        })
        .collect()
}

async fn exit_with_start_error(error: &StartError) -> ! {
    match error {
        StartError::DataDirInUse { running, .. } => {
            eprintln!("Error: {error}.");
            if let Some(description) = describe_running_node(running.as_ref()).await {
                eprintln!("{description}");
            }
            eprintln!("Commands such as `ladex send` and `ladex ls` work with that node; stop it first to start another one here.");
        }
        StartError::EmptyPassphrase => {
            eprintln!("Error: the passphrase must not be empty.");
            std::process::exit(2);
        }
        StartError::MeshEndpoint(_) => eprintln!("Error: {error}. Pick another UDP port with --quic-port."),
        _ => eprintln!("Error: {error}"),
    }
    std::process::exit(1);
}

async fn describe_running_node(endpoint: Option<&Endpoint>) -> Option<String> {
    let reply = control::call(endpoint?, &Request::Status, |_| {}).await.ok()?;
    let Reply::Status(status) = reply else { return None };
    Some(format!("It is {} (version {}), serving its web page on port {}.", status.node_name, status.version, status.http_port))
}

fn describe_notice(notice: &Notice) -> String {
    match notice {
        Notice::CertificateUnavailable(reason) => format!("TLS: could not set up a certificate ({reason}) — falling back to plain HTTP"),
        Notice::ControlUnavailable(reason) => format!("Note: commands such as `ladex send` will not reach this node ({reason})."),
        Notice::LocalListenerUnavailable { port, reason } => format!(
            "Note: could not open the local HTTP listener on 127.0.0.1:{port} ({reason}); use the https address below, \
             or pick another port with --local-port."
        ),
    }
}

fn print_startup(handle: &NodeHandle, has_passphrase: bool) {
    let info = handle.info();
    let storage_limit_gib = handle.state().store.quota() >> 30;
    if !has_passphrase {
        eprintln!(
            "Warning: no passphrase set — any device on this network can open the web UI and join the mesh. \
             Use `ladex -s` to generate one, or set one in the app."
        );
    }
    for notice in &info.notices {
        eprintln!("{}", describe_notice(notice));
    }
    if !info.is_tls_enabled && has_passphrase {
        eprintln!(
            "Warning: TLS is off, so browser traffic is unencrypted. Nodes still talk to each other over \
             encrypted QUIC, and the passphrase itself is never sent."
        );
    }
    println!("Shared files are kept in {} (up to {} GiB)", info.data_dir.display(), storage_limit_gib);
    if info.imported_legacy_files > 0 {
        println!("Listed {} file(s) shared by an earlier version in the signed catalog", info.imported_legacy_files);
    }

    let scheme = if info.is_tls_enabled { "https" } else { "http" };
    let local_ip = info.primary_ip.map(|ip| ip.to_string()).unwrap_or_else(|| "YOUR_IP".to_string());
    match info.local_http_port {
        Some(port) => println!("Access on this machine: http://localhost:{port}  (no certificate warning)"),
        None => println!("Access locally: {scheme}://localhost:{}", info.http_port),
    }
    println!("Access from other devices: {scheme}://{local_ip}:{}", info.http_port);
    if info.is_tls_enabled {
        println!("Certificate fingerprint (SHA-256): {}", tls::format_fingerprint(&info.tls_fingerprint));
        println!(
            "Other devices' browsers will warn that this certificate is self-signed. Before accepting it, \
             check that the fingerprint in the browser's certificate details matches the one above."
        );
    }
    if info.primary_ip.is_some() {
        output::print_qr_code(&format!("{scheme}://{local_ip}:{}", info.http_port));
    }
}
