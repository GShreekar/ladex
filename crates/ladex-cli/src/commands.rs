//! The commands that talk to the node already running on this machine.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context};
use clap::{Args, Subcommand};
use ladex_core::control::server::{self, Endpoint};
use ladex_core::control::{Device, Event, GuestInvite, Invite, Node, Reply, Request};
use ladex_core::guest::sessions::GuestSummary;
use ladex_core::guest::{Permission, WaitingUpload};
use ladex_core::preferences::{self, AutoAccept, AutoAcceptFrom, Preferences};
use ladex_core::trust::{TrustedNodeView, TrustedVia};
use ladex_core::types::{FileView, TextMessage};

use crate::output::{self, format_bytes, ProgressLine};
use crate::{autostart, doctor, pair, passphrase, receive};

const WATCH_INTERVAL: Duration = Duration::from_secs(2);
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;

#[derive(Subcommand)]
pub enum Command {
    /// Share files or folders with everyone, or send files to one device with --to
    Send {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Send to this device alone (its name or id, see `ladex devices`); it is asked to accept first.
        #[arg(long)]
        to: Option<String>,
    },
    /// Download a shared file or folder, named by its id or its name
    Get {
        file: String,
        /// The directory to save into. Default: the current directory
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Share a folder; with --watch, keep running and share it again whenever it changes
    Seed {
        dir: PathBuf,
        #[arg(long)]
        watch: bool,
    },
    /// List what is shared
    Ls {
        /// Also list the files inside shared folders.
        #[arg(long)]
        all: bool,
    },
    /// List the connected devices and nodes
    Devices,
    /// Invite a browser: a one-time link as text and as a QR code, with the certificate fingerprint to check
    Invite {
        /// What the guest may do: read-only, upload or upload-with-approval
        #[arg(long, default_value = "read-only")]
        permission: Permission,
    },
    /// Check that this node can be reached and has what it needs
    Doctor,
    /// Stop sharing a file or folder this node shared, named by its id or its name
    Unshare { file: String },
    /// Show the room's chat, or post a message to it
    Chat {
        /// The message to post.
        message: Option<String>,
    },
    /// List the browser guests and their uploads waiting for approval, or act on them
    Guests {
        #[command(subcommand)]
        action: Option<GuestsAction>,
    },
    /// Receive files sent to this device: stays running as a device others can send to
    Receive {
        /// The device name others see. Default: this node's name
        #[arg(long)]
        name: Option<String>,
        /// The directory to save into. Default: the current directory
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Accept every file without asking, whatever the incoming files rule says.
        #[arg(long)]
        yes: bool,
    },
    /// Pair with another node by comparing six words, so each trusts the other's key itself
    Pair {
        /// The other device's address, as `ladex pair` on it shows; without one, waits for it to dial this one.
        address: Option<String>,
    },
    /// List the nodes this one trusts, or revoke one so the whole mesh drops it
    Trust {
        #[command(subcommand)]
        action: Option<TrustAction>,
    },
    /// Show the mesh passphrase, or set a new one; works with or without a running node
    Passphrase {
        #[command(subcommand)]
        action: Option<passphrase::Action>,
        /// Keep the passphrase in a file in the data directory instead of the OS keychain.
        #[arg(long, global = true)]
        no_keychain: bool,
    },
    /// Start a node when you log in, or stop doing so: what the app's Launch at login does
    Autostart {
        /// on or off; without it, shows whether it is on.
        state: Option<autostart::Switch>,
    },
    /// Show this node's settings, or change them: what the app's Settings and Network pages do
    Settings(SettingsChange),
}

#[derive(Args, Default)]
pub struct SettingsChange {
    /// Where files saved from the app's window go; an empty value goes back to the system's downloads folder.
    #[arg(long)]
    downloads_dir: Option<PathBuf>,
    /// Most disk space, in GiB, that shared files may use.
    #[arg(long)]
    storage_limit_gb: Option<u64>,
    /// Whose files the app's window accepts without asking: nobody, paired (devices on nodes paired with this one) or mesh.
    #[arg(long)]
    auto_accept: Option<AutoAcceptFrom>,
    /// Ask before accepting a file larger than this many MiB, whoever sends it.
    #[arg(long, conflicts_with = "no_size_limit")]
    ask_above_mb: Option<u64>,
    /// Accept files of any size from those auto_accept allows.
    #[arg(long)]
    no_size_limit: bool,
    /// Keep the node at this address connected (IP, or IP:port), for networks where discovery cannot find it. Repeatable.
    #[arg(long, value_name = "ADDRESS")]
    add_peer: Vec<String>,
    /// Stop connecting to a node added with --add-peer. Repeatable.
    #[arg(long, value_name = "ADDRESS")]
    remove_peer: Vec<String>,
}

impl SettingsChange {
    fn asks_for_change(&self) -> bool {
        self.downloads_dir.is_some()
            || self.storage_limit_gb.is_some()
            || self.auto_accept.is_some()
            || self.ask_above_mb.is_some()
            || self.no_size_limit
            || !self.add_peer.is_empty()
            || !self.remove_peer.is_empty()
    }
}

#[derive(Subcommand)]
pub enum GuestsAction {
    /// Sign a guest out, throwing away its uploads nobody accepted yet
    SignOut { guest: String },
    /// Accept a guest's upload that waits for approval, so it is shared with everyone
    Approve { upload: String },
    /// Throw away a guest's upload that waits for approval
    Deny { upload: String },
}

#[derive(Subcommand)]
pub enum TrustAction {
    /// Revoke a node, named by its id or its name; every node in the mesh drops it
    Revoke { node: String },
}

/// A running node, and how this command should show what the node tells it.
pub struct Session {
    endpoint: Endpoint,
    is_json: bool,
}

pub fn not_running(data_dir: &Path) -> anyhow::Error {
    anyhow!("no LADEX node is running with {}; start one with `ladex`, or open the LADEX app", data_dir.display())
}

impl Session {
    pub fn open(data_dir: &Path, is_json: bool) -> anyhow::Result<Self> {
        let endpoint = Endpoint::load(data_dir).ok_or_else(|| not_running(data_dir))?;
        Ok(Self { endpoint, is_json })
    }

    pub fn is_json(&self) -> bool {
        self.is_json
    }

    pub async fn call(&self, request: &Request) -> anyhow::Result<Reply> {
        let mut progress = ProgressLine::new(!self.is_json);
        let reply = server::call(&self.endpoint, request, |event| self.show(event, &mut progress)).await;
        progress.clear();
        reply
    }

    /// Sends a request whose events the caller handles itself.
    pub async fn stream(&self, request: &Request, on_event: impl FnMut(&Event)) -> anyhow::Result<Reply> {
        server::call(&self.endpoint, request, on_event).await
    }

    fn show(&self, event: &Event, progress: &mut ProgressLine) {
        if self.is_json {
            return;
        }
        if let Event::Progress { name, done, total } = event {
            return progress.draw(name, *done, *total);
        }
        progress.clear();
        match event {
            Event::Offered { name, device } => {
                eprintln!("Offered {name} to {device}; waiting for an answer (Ctrl-C leaves the offer open)")
            }
            Event::Delivered { name } => eprintln!("{name} was delivered"),
            Event::Declined { name } => eprintln!("{name} was declined"),
            Event::Welcome { .. }
            | Event::Progress { .. }
            | Event::Joined { .. }
            | Event::Incoming { .. }
            | Event::Done { .. }
            | Event::Failed { .. } => {}
        }
    }
}

/// Runs a command and returns the process's exit code.
pub async fn run(command: Command, data_dir: &Path, is_json: bool) -> i32 {
    // These work whether or not a node is running.
    let outcome = match command {
        Command::Doctor => return doctor::run(data_dir, is_json).await,
        Command::Passphrase { action, no_keychain } => passphrase::run(action, data_dir, !no_keychain, is_json).await,
        Command::Autostart { state } => autostart::run(state, data_dir, is_json),
        command => execute(command, data_dir, is_json).await,
    };
    match outcome {
        Ok(code) => code,
        Err(e) => {
            let message = if is_connection_refused(&e) { format!("{:#}", not_running(data_dir)) } else { format!("{e:#}") };
            if is_json {
                println!("{}", serde_json::json!({ "error": message }));
            }
            eprintln!("Error: {message}");
            EXIT_FAILED
        }
    }
}

// A node that was killed leaves its endpoint file behind, with nothing listening.
pub fn is_connection_refused(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionRefused)
}

async fn execute(command: Command, data_dir: &Path, is_json: bool) -> anyhow::Result<i32> {
    let session = Session::open(data_dir, is_json)?;
    let request = match command {
        Command::Send { paths, to } => Request::Send { paths: existing(&paths)?, to },
        Command::Get { file, output } => Request::Get { file, into: std::path::absolute(output.unwrap_or_else(|| PathBuf::from(".")))? },
        Command::Seed { dir, watch: false } => Request::Seed { dir: existing(&[dir])?.remove(0), replaces: None },
        Command::Seed { dir, watch: true } => return watch(&session, &existing(&[dir])?.remove(0)).await,
        Command::Ls { all } => return list(&session, all).await,
        Command::Devices => Request::Devices,
        Command::Invite { permission } => Request::Invite { permission },
        Command::Unshare { file } => Request::Unshare { file },
        Command::Chat { message } => Request::Chat { message },
        Command::Guests { action: None } => Request::Guests,
        Command::Guests { action: Some(GuestsAction::SignOut { guest }) } => Request::SignOutGuest { guest },
        Command::Guests { action: Some(GuestsAction::Approve { upload }) } => Request::AnswerUpload { upload, accepted: true },
        Command::Guests { action: Some(GuestsAction::Deny { upload }) } => Request::AnswerUpload { upload, accepted: false },
        Command::Pair { address } => return pair::run(&session, address).await,
        Command::Trust { action: None } => Request::Trust,
        Command::Trust { action: Some(TrustAction::Revoke { node }) } => Request::RevokeNode { node },
        Command::Receive { name, output, yes } => {
            let into = std::path::absolute(output.unwrap_or_else(|| PathBuf::from(".")))?;
            return receive::run(&session, name, into, yes).await;
        }
        Command::Doctor | Command::Passphrase { .. } | Command::Autostart { .. } => {
            unreachable!("these do not need a running node and are handled before this")
        }
        Command::Settings(change) => {
            let update = if change.asks_for_change() {
                let current = current_preferences(&session).await?;
                Some(settings_update(current, change)?)
            } else {
                None
            };
            Request::Preferences { update }
        }
    };
    let reply = session.call(&request).await?;
    Ok(report(&reply, is_json))
}

async fn current_preferences(session: &Session) -> anyhow::Result<Preferences> {
    match session.call(&Request::Preferences { update: None }).await? {
        Reply::Preferences(view) => Ok(view.preferences),
        _ => bail!("the node answered something else"),
    }
}

// Only what was asked for changes; the node reads the downloads folder as a full path, whatever directory this ran in.
fn settings_update(current: Preferences, change: SettingsChange) -> anyhow::Result<preferences::Update> {
    let wants_other_rule = change.auto_accept.is_some() || change.ask_above_mb.is_some() || change.no_size_limit;
    let rule = wants_other_rule.then(|| AutoAccept {
        from: change.auto_accept.unwrap_or(current.auto_accept.from),
        ask_above_mib: if change.no_size_limit { None } else { change.ask_above_mb.or(current.auto_accept.ask_above_mib) },
    });
    let wants_other_peers = !change.add_peer.is_empty() || !change.remove_peer.is_empty();
    let peer_addresses = if wants_other_peers { Some(changed_peer_addresses(&current, &change)?) } else { None };
    let downloads_dir = match change.downloads_dir {
        Some(path) if path.as_os_str().is_empty() => Some(path),
        Some(path) => Some(std::path::absolute(&path).with_context(|| format!("could not resolve {}", path.display()))?),
        None => None,
    };
    Ok(preferences::Update {
        is_setup_done: None,
        downloads_dir,
        storage_limit_gib: change.storage_limit_gb,
        auto_accept: rule,
        peer_addresses,
    })
}

// The saved list with the additions and without the removals, compared as addresses so "10.0.0.2" removes "10.0.0.2:8080".
fn changed_peer_addresses(current: &Preferences, change: &SettingsChange) -> anyhow::Result<Vec<String>> {
    let removed = preferences::checked_peer_addresses(&change.remove_peer)?;
    let added = preferences::checked_peer_addresses(&change.add_peer)?;
    let kept = current.peer_addresses.iter().chain(&added).filter(|address| !removed.contains(address));
    Ok(kept.map(ToString::to_string).collect())
}

// The node reads the paths itself, so they must not depend on the directory this command was run in.
fn existing(paths: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    paths.iter().map(|path| path.canonicalize().with_context(|| format!("could not find {}", path.display()))).collect()
}

pub fn report(reply: &Reply, is_json: bool) -> i32 {
    if is_json {
        println!("{}", serde_json::to_string(reply).expect("a reply is plain data"));
    } else {
        print_reply(reply);
    }
    match reply {
        Reply::Pushed { declined, .. } if !declined.is_empty() => EXIT_FAILED,
        _ => EXIT_OK,
    }
}

fn print_reply(reply: &Reply) {
    match reply {
        Reply::Shared { files } => files.iter().for_each(print_shared),
        Reply::Pushed { delivered, declined } => println!("{} delivered, {} not delivered", delivered.len(), declined.len()),
        Reply::Saved { path, .. } => println!("Saved {}", path.display()),
        Reply::Files { files } => print_files(files),
        Reply::Devices { devices, nodes } => print_devices(devices, nodes),
        Reply::Invite(invite) => print_invite(invite),
        Reply::Status(status) => println!("{} ({}) is running", status.node_name, status.node_id),
        Reply::Preferences(view) => print_preferences(view),
        Reply::Messages { messages } => print_messages(messages),
        Reply::Guests { guests, uploads } => print_guests(guests, uploads),
        Reply::Passphrase { passphrase } => passphrase::print(passphrase.as_deref()),
        Reply::Received { path, .. } => println!("Saved {}", path.display()),
        Reply::Acknowledged { message } => println!("{message}"),
        Reply::Pairing(view) => println!("This device's address for pairing: {}", view.address.as_deref().unwrap_or("none")),
        Reply::Trust { nodes } => print_trust(nodes),
    }
}

fn print_trust(nodes: &[TrustedNodeView]) {
    if nodes.is_empty() {
        return println!("This node trusts no other node yet. Nodes are trusted when they join with the passphrase or pair.");
    }
    let how = |node: &TrustedNodeView| match node.node.trusted_via {
        TrustedVia::Passphrase => "passphrase",
        TrustedVia::Pairing => "paired",
    };
    let standing = |node: &TrustedNodeView| match (&node.revoked_by, node.connected) {
        (Some(by), _) => format!("revoked by {by}"),
        (None, true) => "connected".to_string(),
        (None, false) => "not connected".to_string(),
    };
    let row = |node: &TrustedNodeView| vec![node.node.name.clone(), how(node).to_string(), standing(node), node.node.node_id.clone()];
    output::print_table(&["NODE", "TRUSTED BY", "NOW", "ID"], &nodes.iter().map(row).collect::<Vec<_>>());
}

fn print_messages(messages: &[TextMessage]) {
    if messages.is_empty() {
        return println!("No messages yet. Post one with: ladex chat \"hello\"");
    }
    for message in messages {
        let time = message.timestamp.with_timezone(&chrono::Local).format("%H:%M");
        let sender = message.sender_name.as_deref().unwrap_or("someone");
        println!("{time}  {sender}: {}", message.content);
    }
}

fn print_guests(guests: &[GuestSummary], uploads: &[WaitingUpload]) {
    if guests.is_empty() {
        println!("No guests are signed in. Invite one with: ladex invite");
    } else {
        let row =
            |guest: &GuestSummary| vec![guest.label.clone(), guest.ip.clone(), describe(guest.permission).to_string(), guest.id.clone()];
        output::print_table(&["GUEST", "ADDRESS", "MAY", "ID"], &guests.iter().map(row).collect::<Vec<_>>());
    }
    if uploads.is_empty() {
        return;
    }
    println!("\nWaiting for approval (ladex guests approve <id>, or deny <id>):");
    let row = |upload: &WaitingUpload| {
        let guest = upload.guest.clone().unwrap_or_else(|| "a guest who left".to_string());
        vec![upload.name.clone(), format_bytes(upload.size), guest, upload.id.clone()]
    };
    output::print_table(&["NAME", "SIZE", "FROM", "ID"], &uploads.iter().map(row).collect::<Vec<_>>());
}

fn print_preferences(view: &preferences::View) {
    let preferences = &view.preferences;
    let downloads =
        preferences.downloads_dir.as_ref().map_or("the system's downloads folder".to_string(), |path| path.display().to_string());
    let auto_accept = match preferences.auto_accept.from {
        AutoAcceptFrom::Nobody => "always ask",
        AutoAcceptFrom::PairedDevices => "accept from devices on paired nodes",
        AutoAcceptFrom::Mesh => "accept from anyone in this mesh",
    };
    let size_limit = preferences.auto_accept.ask_above_mib.map_or("no limit".to_string(), |mib| format!("ask above {mib} MiB"));
    let rows = vec![
        vec!["Data folder".to_string(), view.data_dir.display().to_string()],
        vec!["Downloads folder".to_string(), downloads],
        vec!["Storage limit".to_string(), format!("{} GiB ({} used)", preferences.storage_limit_gib, format_bytes(view.store_used_bytes))],
        vec!["Incoming files".to_string(), format!("{auto_accept}; {size_limit}")],
        vec!["Passphrase".to_string(), if view.has_passphrase { "set" } else { "none" }.to_string()],
        vec!["Devices added by address".to_string(), peer_addresses_text(&preferences.peer_addresses)],
        vec!["This device's address".to_string(), view.this_address.clone().unwrap_or_else(|| "none (no network)".to_string())],
    ];
    output::print_table(&["SETTING", "VALUE"], &rows);
}

fn peer_addresses_text(addresses: &[std::net::SocketAddr]) -> String {
    if addresses.is_empty() {
        return "none".to_string();
    }
    addresses.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
}

fn size_of(file: &FileView) -> String {
    if file.is_folder {
        return format!("{} in {} files", format_bytes(file.folder_bytes), file.folder_files);
    }
    format_bytes(file.size)
}

fn print_shared(file: &FileView) {
    println!("Shared {} ({})  id {}", file.name, size_of(file), file.id);
}

fn print_files(files: &[FileView]) {
    if files.is_empty() {
        return println!("Nothing is shared.");
    }
    let kind = |file: &FileView| if file.is_folder { "folder" } else { "file" };
    let availability = |file: &FileView| match file.hosts.len() {
        0 if file.available => "in parts, on several nodes".to_string(),
        0 => "nobody online has it".to_string(),
        1 => "1 node".to_string(),
        count => format!("{count} nodes"),
    };
    let row = |file: &FileView| vec![file.name.clone(), kind(file).to_string(), size_of(file), availability(file), file.id.clone()];
    output::print_table(&["NAME", "KIND", "SIZE", "ON", "ID"], &files.iter().map(row).collect::<Vec<_>>());
}

async fn list(session: &Session, includes_folder_contents: bool) -> anyhow::Result<i32> {
    let Reply::Files { mut files } = session.call(&Request::List).await? else { bail!("the node answered something else") };
    if !includes_folder_contents {
        files.retain(|file| file.parent.is_none());
    }
    files.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
    Ok(report(&Reply::Files { files }, session.is_json))
}

fn print_devices(devices: &[Device], nodes: &[Node]) {
    if devices.is_empty() {
        println!("No devices are connected.");
    } else {
        let place = |device: &Device| if device.is_here { "this node".to_string() } else { device.node_name.clone() };
        let row = |device: &Device| vec![device.name.clone(), place(device), device.id.clone()];
        output::print_table(&["DEVICE", "CONNECTED TO", "ID"], &devices.iter().map(row).collect::<Vec<_>>());
    }
    println!();
    if nodes.is_empty() {
        return println!("No other nodes are connected.");
    }
    let delay = |node: &Node| node.rtt_ms.map_or("-".to_string(), |ms| format!("{ms} ms"));
    let row = |node: &Node| vec![node.name.clone(), node.address.to_string(), delay(node), node.id.clone()];
    output::print_table(&["NODE", "ADDRESS", "DELAY", "ID"], &nodes.iter().map(row).collect::<Vec<_>>());
}

fn print_invite(invite: &Invite) {
    let Some(url) = &invite.url else {
        return println!("This machine has no address on a local network, so other devices cannot reach it yet.");
    };
    if let Some(guest) = &invite.guest {
        print_guest_invite(guest, invite.certificate_fingerprint.as_deref());
    }
    println!("\nMembers of this mesh sign in at {url}");
    if invite.needs_passphrase {
        println!("They are asked for the passphrase this node was started with.");
    }
    if let Some(address) = &invite.peer_address {
        println!("To connect another node directly: ladex --peer {address}");
    }
}

fn print_guest_invite(guest: &GuestInvite, fingerprint: Option<&str>) {
    let minutes = guest.expires_in_secs / 60;
    println!("Invite a browser (one device, within {minutes} minutes; it may {}):", describe(guest.permission));
    println!("  Open: {}", guest.join_url);
    println!("  Or go to this address's /join page and type: {}", guest.code);
    match fingerprint {
        Some(fingerprint) => {
            println!("  The browser will warn about the certificate; accept it only if the fingerprint shown is:\n    {fingerprint}")
        }
        None => println!("  This node is not using encryption, so this link can be read by anyone on the network."),
    }
    output::print_qr_code(&guest.join_url);
}

fn describe(permission: Permission) -> &'static str {
    match permission {
        Permission::ReadOnly => "download only",
        Permission::Upload => "download and upload",
        Permission::UploadWithApproval => "download, and upload once you accept each file",
    }
}

// Everything under a folder that would change what is shared: each file's path, size and time of last change.
fn snapshot(dir: &Path) -> anyhow::Result<Vec<(PathBuf, u64, SystemTime)>> {
    let mut files = Vec::new();
    let mut to_visit = vec![dir.to_path_buf()];
    while let Some(visiting) = to_visit.pop() {
        for entry in std::fs::read_dir(&visiting).with_context(|| format!("could not read {}", visiting.display()))? {
            let path = entry?.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                to_visit.push(path);
            } else if metadata.is_file() {
                files.push((path, metadata.len(), metadata.modified()?));
            }
        }
    }
    files.sort();
    Ok(files)
}

// Shares the folder, then shares it afresh each time its contents have changed and then stayed the same for one interval.
async fn watch(session: &Session, dir: &Path) -> anyhow::Result<i32> {
    let mut shared_snapshot = snapshot(dir)?;
    let mut shared = seed(session, dir, None).await?;
    eprintln!("Watching {} (Ctrl-C stops watching; the folder stays shared)", dir.display());
    let mut last_seen = shared_snapshot.clone();
    loop {
        tokio::time::sleep(WATCH_INTERVAL).await;
        let now = snapshot(dir)?;
        let is_settled = now == last_seen;
        last_seen = now.clone();
        if now == shared_snapshot || !is_settled {
            continue;
        }
        match seed(session, dir, Some(shared.id.clone())).await {
            Ok(replacement) => shared = replacement,
            // The folder shared before stays up; the next change is another chance.
            Err(e) => eprintln!("Could not share the changed folder: {e:#}"),
        }
        shared_snapshot = now;
    }
}

async fn seed(session: &Session, dir: &Path, replaces: Option<String>) -> anyhow::Result<FileView> {
    let reply = session.call(&Request::Seed { dir: dir.to_path_buf(), replaces }).await?;
    report(&reply, session.is_json);
    match reply {
        Reply::Shared { mut files } if files.len() == 1 => Ok(files.remove(0)),
        _ => bail!("the node answered something else"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current() -> Preferences {
        Preferences { auto_accept: AutoAccept { from: AutoAcceptFrom::Mesh, ask_above_mib: Some(200) }, ..Preferences::default() }
    }

    fn update(auto_accept: Option<AutoAcceptFrom>, ask_above_mb: Option<u64>, no_size_limit: bool) -> preferences::Update {
        settings_update(current(), SettingsChange { auto_accept, ask_above_mb, no_size_limit, ..Default::default() }).unwrap()
    }

    fn with_peer(address: &str) -> Preferences {
        Preferences { peer_addresses: vec![address.parse().unwrap()], ..current() }
    }

    fn peers_after(current: Preferences, add_peer: &[&str], remove_peer: &[&str]) -> Option<Vec<String>> {
        let to_strings = |addresses: &[&str]| addresses.iter().map(ToString::to_string).collect();
        let change = SettingsChange { add_peer: to_strings(add_peer), remove_peer: to_strings(remove_peer), ..Default::default() };
        settings_update(current, change).unwrap().peer_addresses
    }

    #[test]
    fn asking_for_nothing_changes_nothing() {
        assert_eq!(update(None, None, false), preferences::Update::default());
    }

    #[test]
    fn changing_who_is_accepted_keeps_the_size_limit() {
        let rule = update(Some(AutoAcceptFrom::PairedDevices), None, false).auto_accept.unwrap();
        assert_eq!((rule.from, rule.ask_above_mib), (AutoAcceptFrom::PairedDevices, Some(200)));
    }

    #[test]
    fn changing_the_size_limit_keeps_who_is_accepted() {
        let rule = update(None, Some(50), false).auto_accept.unwrap();
        assert_eq!((rule.from, rule.ask_above_mib), (AutoAcceptFrom::Mesh, Some(50)));
    }

    #[test]
    fn removing_the_size_limit_keeps_who_is_accepted() {
        let rule = update(None, None, true).auto_accept.unwrap();
        assert_eq!((rule.from, rule.ask_above_mib), (AutoAcceptFrom::Mesh, None));
    }

    #[test]
    fn an_empty_downloads_folder_is_passed_on_to_mean_the_default() {
        let update = settings_update(current(), SettingsChange { downloads_dir: Some(PathBuf::new()), ..Default::default() }).unwrap();
        assert_eq!(update.downloads_dir, Some(PathBuf::new()));
    }

    #[test]
    fn a_relative_downloads_folder_is_made_absolute() {
        let change = SettingsChange { downloads_dir: Some(PathBuf::from("received")), ..Default::default() };
        assert!(settings_update(current(), change).unwrap().downloads_dir.unwrap().is_absolute());
    }

    #[test]
    fn an_added_peer_joins_the_saved_ones() {
        let peers = peers_after(with_peer("10.0.0.2:8080"), &["10.0.0.3:9000"], &[]);
        assert_eq!(peers, Some(vec!["10.0.0.2:8080".to_string(), "10.0.0.3:9000".to_string()]));
    }

    #[test]
    fn a_peer_is_removed_by_its_ip_alone_when_it_uses_the_default_port() {
        assert_eq!(peers_after(with_peer("10.0.0.2:8080"), &[], &["10.0.0.2"]), Some(Vec::new()));
    }

    #[test]
    fn not_touching_peers_leaves_them_out_of_the_update() {
        assert_eq!(peers_after(with_peer("10.0.0.2:8080"), &[], &[]), None);
    }

    #[test]
    fn a_mistyped_peer_is_refused() {
        let change = SettingsChange { add_peer: vec!["my-laptop".to_string()], ..Default::default() };
        assert!(settings_update(current(), change).is_err());
    }
}
