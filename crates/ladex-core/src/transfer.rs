//! Moves file chunks between nodes, verified against the manifest and fetched from every holder in parallel.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use self::peer::{Fault, Peer};
use self::uploads::UploadSlots;
use self::wire::{ErrorCode, Fetch, GetChunks, Reply};
use crate::availability::Have;
use crate::bitmap::Bitmap;
use crate::mesh::MeshMessage;
use crate::store::{chunk_count, Blob, ChunkHash, Origin, StoreError, WriteError, CHUNK_SIZE};
use crate::types::NodeId;
use crate::validate;
use crate::NodeState;

pub mod peer;
pub mod uploads;
pub mod wire;

pub const MAX_REQUEST_CHUNKS: usize = 16;
pub const UPLOAD_SLOTS: usize = 8;
// A requester that opens a stream and says nothing is cut off after this.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);
// A request waits this long for an upload slot before being told the node is busy.
const SLOT_WAIT: Duration = Duration::from_secs(5);
// A requester that stops reading for this long loses its stream and the upload slot it held.
const SERVE_STALL_TIMEOUT: Duration = Duration::from_secs(60);
// Chunks are 1 MiB each, so this bounds the memory held by chunks that arrived but are not yet written.
const EVENT_QUEUE: usize = 32;

const READAHEAD: u32 = 16;
// Only this many missing chunks are weighed against each other, to keep planning cheap for huge files.
const PLANNING_WINDOW: usize = 4096;
// The last 1% of a file, but never fewer than this many chunks...
const ENDGAME_MIN_CHUNKS: u32 = 4;
// ...nor more than this many, since every one of them is requested from every source.
const ENDGAME_MAX_CHUNKS: u32 = 64;
const MIN_DEPTH: usize = 4;
const MAX_DEPTH: usize = 32;
const TARGET_IN_FLIGHT_SECS: f64 = 0.5;

pub struct Source {
    // None: has the whole file.
    pub has: Option<Bitmap>,
    pub in_flight: usize,
    pub speed_bps: f64,
}

impl Source {
    fn has(&self, index: u32) -> bool {
        self.has.as_ref().is_none_or(|map| map.get(index))
    }

    pub fn depth(&self) -> usize {
        if self.speed_bps <= 0.0 {
            return MIN_DEPTH;
        }
        ((self.speed_bps * TARGET_IN_FLIGHT_SECS / CHUNK_SIZE as f64).ceil() as usize).clamp(MIN_DEPTH, MAX_DEPTH)
    }
}

/// How many chunks are left when the scheduler stops being economical and asks every source for each one.
pub fn endgame_threshold(total_chunks: u32) -> u32 {
    (total_chunks / 100).clamp(ENDGAME_MIN_CHUNKS, ENDGAME_MAX_CHUNKS).min(total_chunks)
}

// Missing chunks, most urgent first: those just ahead of a streaming reader, then rarest first.
pub fn priority_order(have: &Bitmap, readers: &[u32], sources: &[Source]) -> Vec<u32> {
    let mut order = Vec::new();
    let mut seen = HashSet::new();
    let mut positions = readers.to_vec();
    positions.sort_unstable();
    for position in positions {
        for index in position..position.saturating_add(READAHEAD).min(have.len()) {
            if !have.get(index) && seen.insert(index) {
                order.push(index);
            }
        }
    }
    let mut rest: Vec<(usize, u32)> = have
        .iter_missing()
        .filter(|i| !seen.contains(i))
        .take(PLANNING_WINDOW)
        .map(|i| (sources.iter().filter(|s| s.has(i)).count(), i))
        .filter(|(copies, _)| *copies > 0)
        .collect();
    rest.sort_unstable();
    order.extend(rest.into_iter().map(|(_, i)| i));
    order
}

// A chunk is requested from more than one source only in the endgame, when it goes to every source that can give it.
pub fn plan(want: &[u32], in_flight: &HashMap<u32, Vec<usize>>, sources: &[Source], endgame: bool) -> Vec<(usize, u32)> {
    let mut room: Vec<usize> = sources.iter().map(|s| s.depth().saturating_sub(s.in_flight)).collect();
    let mut assigned = vec![0usize; sources.len()];
    let mut requests = Vec::new();

    for &index in want {
        let already = in_flight.get(&index);
        if already.is_some() && !endgame {
            continue;
        }
        let score = |s: usize, assigned: &[usize]| sources[s].speed_bps.max(1.0) / (sources[s].in_flight + assigned[s] + 1) as f64;
        let mut candidates: Vec<usize> =
            (0..sources.len()).filter(|&s| room[s] > 0 && sources[s].has(index) && !already.is_some_and(|a| a.contains(&s))).collect();
        candidates.sort_by(|&a, &b| score(b, &assigned).total_cmp(&score(a, &assigned)).then(a.cmp(&b)));
        candidates.truncate(if endgame { usize::MAX } else { 1 });
        for s in candidates {
            room[s] -= 1;
            assigned[s] += 1;
            requests.push((s, index));
        }
    }
    requests
}

pub struct Tuning {
    pub manifest_retry: Duration,
    pub tick: Duration,
    pub idle_exit: Duration,
    pub map_interval: Duration,
    pub stall_timeout: Duration,
    /// How long a request's stream may deliver no bytes before its chunks are asked for elsewhere.
    pub stream_stall_timeout: Duration,
    /// Uploads this node runs at once, across all the peers asking.
    pub upload_slots: usize,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            manifest_retry: Duration::from_secs(10),
            tick: Duration::from_millis(250),
            idle_exit: Duration::from_secs(600),
            map_interval: Duration::from_millis(250),
            stall_timeout: Duration::from_secs(60),
            stream_stall_timeout: Duration::from_secs(20),
            upload_slots: UPLOAD_SLOTS,
        }
    }
}

enum Event {
    Chunk {
        from: NodeId,
        index: u32,
        data: Vec<u8>,
        arrived: Instant,
    },
    /// The node did not give these chunks, so they are free to be requested elsewhere.
    Released {
        from: NodeId,
        indices: Vec<u32>,
        fault: Fault,
    },
    Manifest {
        from: NodeId,
        hashes: Vec<ChunkHash>,
    },
    Wake,
}

pub struct Download {
    pub blob: Arc<Blob>,
    events: mpsc::Sender<Event>,
    readers: Mutex<HashMap<u64, u32>>,
    next_reader: AtomicU64,
    cancelled: AtomicBool,
    // Set for a file sent to one device: fetched from this node alone, and never announced to the mesh.
    only_source: Option<NodeId>,
}

pub struct Reader {
    download: Arc<Download>,
    id: u64,
}

impl Reader {
    pub fn at(&self, chunk: u32) {
        self.download.readers.lock().unwrap().insert(self.id, chunk);
        let _ = self.download.events.try_send(Event::Wake);
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.download.readers.lock().unwrap().remove(&self.id);
    }
}

pub struct Transfers {
    downloads: Mutex<HashMap<String, Arc<Download>>>,
    // Downloads a restart cut short, waiting for a node that has them to connect.
    interrupted: Mutex<HashSet<String>>,
    tuning: Tuning,
    uploads: UploadSlots,
}

impl Default for Transfers {
    fn default() -> Self {
        Self::new()
    }
}

impl Transfers {
    pub fn new() -> Self {
        Self::with_tuning(Tuning::default())
    }

    pub fn with_tuning(tuning: Tuning) -> Self {
        let uploads = UploadSlots::new(tuning.upload_slots);
        Self { downloads: Mutex::new(HashMap::new()), interrupted: Mutex::new(HashSet::new()), tuning, uploads }
    }

    fn get(&self, id: &str) -> Option<Arc<Download>> {
        self.downloads.lock().unwrap().get(id).cloned()
    }

    pub fn stall_timeout(&self) -> Duration {
        self.tuning.stall_timeout
    }

    /// How long a request's stream may deliver nothing before it is abandoned.
    pub fn stream_stall_timeout(&self) -> Duration {
        self.tuning.stream_stall_timeout
    }

    /// The node-wide upload slots.
    pub fn uploads(&self) -> &UploadSlots {
        &self.uploads
    }

    pub fn is_downloading(&self, id: &str) -> bool {
        self.downloads.lock().unwrap().contains_key(id)
    }

    pub fn active(&self) -> usize {
        self.downloads.lock().unwrap().len()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    NotFound,
    NoOneHasIt,
    Store(StoreError),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::NotFound => f.write_str("no such file"),
            FetchError::NoOneHasIt => f.write_str("no device that has this file is online"),
            FetchError::Store(e) => write!(f, "{e}"),
        }
    }
}

pub async fn ensure_file(state: &NodeState, id: &str) -> Result<Arc<Blob>, FetchError> {
    if let Some(blob) = state.store.get(id) {
        if blob.is_complete() {
            return Ok(blob);
        }
        if state.transfers.get(id).is_some() {
            return Ok(blob);
        }
    }

    let entry = state.catalog.listed(id).ok_or(FetchError::NotFound)?.entry;
    if !someone_online_has(state, id).await {
        return Err(FetchError::NoOneHasIt);
    }

    crate::state::make_room(state, entry.size).await;
    let blob = state.store.create_with(id, entry.size, Origin::Cached).map_err(FetchError::Store)?;
    blob.expect_root(&entry.manifest_root);
    if blob.is_complete() {
        return Ok(blob);
    }

    if start(state, &blob, None) {
        // Maps told before this node knew the file were dropped, so every peer is asked again.
        crate::availability::ask_for_maps(state, &[id.to_string()], None).await;
    }
    Ok(blob)
}

/// Fetches a file from one node alone and tells nobody: how a file sent to one device arrives.
pub fn fetch_from(state: &NodeState, blob: &Arc<Blob>, source: NodeId) {
    if !blob.is_complete() {
        start(state, blob, Some(source));
    }
}

// False if the file is already being downloaded.
fn start(state: &NodeState, blob: &Arc<Blob>, only_source: Option<NodeId>) -> bool {
    let (events, receiver) = mpsc::channel(EVENT_QUEUE);
    let download = Arc::new(Download {
        blob: blob.clone(),
        events,
        readers: Mutex::new(HashMap::new()),
        next_reader: AtomicU64::new(1),
        cancelled: AtomicBool::new(false),
        only_source,
    });
    {
        let mut downloads = state.transfers.downloads.lock().unwrap();
        if downloads.contains_key(blob.id()) {
            return false;
        }
        downloads.insert(blob.id().to_string(), download.clone());
    }
    tokio::spawn(run(state.clone(), download, receiver));
    true
}

// A node with only some of the chunks counts, since a download can start from it and take the rest from others.
async fn someone_online_has(state: &NodeState, id: &str) -> bool {
    let peers = state.mesh_peers.read().await;
    state.availability.holders(id).iter().any(|node| peers.contains_key(node))
}

pub fn open_reader(state: &NodeState, blob: &Blob) -> Option<Reader> {
    let download = state.transfers.get(blob.id())?;
    let id = download.next_reader.fetch_add(1, Ordering::Relaxed);
    download.readers.lock().unwrap().insert(id, 0);
    Some(Reader { download, id })
}

pub fn cancel(state: &NodeState, id: &str) {
    if let Some(download) = state.transfers.get(id) {
        download.cancelled.store(true, Ordering::Relaxed);
        let _ = download.events.try_send(Event::Wake);
    }
}

pub fn sources_changed(state: &NodeState) {
    let downloads: Vec<Arc<Download>> = state.transfers.downloads.lock().unwrap().values().cloned().collect();
    for download in downloads {
        let _ = download.events.try_send(Event::Wake);
    }
    if !state.transfers.interrupted.lock().unwrap().is_empty() {
        tokio::spawn(resume_interrupted(state.clone()));
    }
}

/// Notes the downloads a restart cut short, so they continue once a node that has them connects.
pub fn remember_interrupted(state: &NodeState) {
    let cut_short = state.store.all().into_iter().filter(|blob| blob.origin() == Origin::Cached && !blob.is_complete());
    state.transfers.interrupted.lock().unwrap().extend(cut_short.map(|blob| blob.id().to_string()));
}

async fn resume_interrupted(state: NodeState) {
    let waiting: Vec<String> = state.transfers.interrupted.lock().unwrap().iter().cloned().collect();
    for id in waiting {
        // With no source online yet, the file stays on the list for the next change.
        if !matches!(ensure_file(&state, &id).await, Err(FetchError::NoOneHasIt)) {
            state.transfers.interrupted.lock().unwrap().remove(&id);
        }
    }
}

pub fn on_manifest(state: &NodeState, from: &NodeId, file_id: &str, hashes_hex: &str) {
    let Some(download) = state.transfers.get(file_id) else { return };
    let Ok(raw) = hex::decode(hashes_hex) else { return };
    let hashes = raw.as_chunks::<32>().0.to_vec();
    let _ = download.events.try_send(Event::Manifest { from: from.clone(), hashes });
}

// A file sent to one device is served to that device's node alone, and only after the device accepted it.
fn may_serve(state: &NodeState, blob: &Blob, to: &NodeId) -> bool {
    blob.origin() != Origin::Pushed || state.pushes.may_fetch(blob.id(), to)
}

pub async fn serve_manifest(state: &NodeState, to: &NodeId, file_id: &str) {
    let Some(blob) = state.store.get(file_id).filter(|blob| may_serve(state, blob, to)) else { return };
    let Some(hashes) = blob.hashes() else { return };
    let message = MeshMessage::Manifest {
        file_id: file_id.to_string(),
        size: blob.size(),
        hashes: hex::encode(hashes.iter().flatten().copied().collect::<Vec<u8>>()),
    };
    if let Some(peer) = state.mesh_peers.read().await.get(to) {
        let _ = peer.sender.send(message);
    }
}

/// Answers one request stream from a peer: every requested chunk that is held, then the end of the stream.
pub async fn serve_stream<R, W>(state: &NodeState, from: &NodeId, mut reader: R, mut writer: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let read = tokio::time::timeout(REQUEST_READ_TIMEOUT, wire::read_request(&mut reader)).await;
    let request = read.map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;

    let indices = request.indices(MAX_REQUEST_CHUNKS).filter(|_| validate::is_valid_id(&request.file_id));
    let Some(indices) = indices else {
        return refuse(&mut writer, ErrorCode::Malformed).await;
    };
    if !state.mesh_peers.read().await.contains_key(from) {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "not a connected peer"));
    }
    let Ok(_slot) = tokio::time::timeout(SLOT_WAIT, state.transfers.uploads.acquire(from)).await else {
        return refuse(&mut writer, ErrorCode::Busy).await;
    };
    let Some(blob) = state.store.get(&request.file_id).filter(|blob| may_serve(state, blob, from)) else {
        return refuse(&mut writer, ErrorCode::NotFound).await;
    };

    for index in indices {
        let held = if index < blob.chunk_count() { blob.read_chunk_verified(index).await.ok() } else { None };
        let reply = match held {
            Some(bytes) => Reply::ChunkData { index, bytes },
            None => Reply::Missing { index },
        };
        within_stall_limit(wire::write_reply(&mut writer, &reply)).await?;
    }
    within_stall_limit(writer.shutdown()).await
}

// A requester that stops reading would otherwise hold an upload slot forever.
async fn within_stall_limit<T>(write: impl Future<Output = std::io::Result<T>>) -> std::io::Result<T> {
    tokio::time::timeout(SERVE_STALL_TIMEOUT, write).await.map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))?
}

async fn refuse<W: AsyncWrite + Unpin>(writer: &mut W, code: ErrorCode) -> std::io::Result<()> {
    wire::write_reply(writer, &Reply::Error(code)).await?;
    writer.shutdown().await
}

/// Asks a peer for chunks over a stream of its own and turns what comes back into download events.
async fn request_chunks(fetches: mpsc::Sender<Fetch>, events: mpsc::Sender<Event>, from: NodeId, file_id: String, indices: Vec<u32>) {
    let (replies_tx, mut replies) = mpsc::channel(2);
    let request = GetChunks::for_indices(&file_id, &indices);
    let mut outstanding: HashSet<u32> = indices.into_iter().collect();
    let mut missing = Vec::new();
    let mut fault = Fault::Failed;
    if fetches.send(Fetch { request, replies: replies_tx }).await.is_ok() {
        while let Some(reply) = replies.recv().await {
            match reply {
                // Answers for chunks nobody asked for are ignored, so a peer can't push data at us.
                Reply::ChunkData { index, bytes } if outstanding.remove(&index) => {
                    let chunk = Event::Chunk { from: from.clone(), index, data: bytes, arrived: Instant::now() };
                    if events.send(chunk).await.is_err() {
                        return;
                    }
                }
                Reply::Missing { index } if outstanding.remove(&index) => missing.push(index),
                Reply::Error(code) => {
                    if code == ErrorCode::Busy {
                        fault = Fault::Busy;
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    // Whatever was not answered, whether the stream ended, failed or stalled, is free to be asked for elsewhere.
    let unanswered: Vec<u32> = outstanding.into_iter().collect();
    for (indices, fault) in [(missing, Fault::Missing), (unanswered, fault)] {
        if !indices.is_empty() && events.send(Event::Released { from: from.clone(), indices, fault }).await.is_err() {
            return;
        }
    }
}

// How the scheduler reaches a connected peer: control messages, and streams for chunk requests.
struct Link {
    control: mpsc::UnboundedSender<MeshMessage>,
    fetches: mpsc::Sender<Fetch>,
}

async fn run(state: NodeState, download: Arc<Download>, mut events: mpsc::Receiver<Event>) {
    let blob = download.blob.clone();
    let id = blob.id().to_string();
    let tuning = &state.transfers.tuning;
    let mut peers: HashMap<NodeId, Peer> = HashMap::new();
    let mut ticker = tokio::time::interval(tuning.tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut manifest_asked: Option<Instant> = None;
    let mut last_progress = Instant::now();
    // The chunks the mesh was last told this node has; None until the first announcement.
    let mut told: Option<(Instant, Bitmap)> = None;

    loop {
        let first = tokio::select! {
            event = events.recv() => event,
            _ = ticker.tick() => None,
        };
        let mut batch: Vec<Event> = first.into_iter().collect();
        while batch.len() < 64 {
            match events.try_recv() {
                Ok(event) => batch.push(event),
                Err(_) => break,
            }
        }
        for event in batch {
            match event {
                Event::Chunk { from, index, data, arrived } => {
                    let len = data.len();
                    let peer = peers.entry(from.clone()).or_insert_with(|| Peer::new(arrived));
                    peer.in_flight.remove(&index);
                    peer.cancellable.remove(&index);
                    match blob.write_chunk_verified(index, data).await {
                        Ok(()) => {
                            peer.delivered(arrived, len);
                            last_progress = Instant::now();
                            call_off_duplicates(&mut peers, &from, index);
                        }
                        Err(WriteError::HashMismatch) => {
                            tracing::warn!("Transfer: {from} sent a corrupted chunk {index} of {id}");
                            peer.sent_bad_chunk();
                            if peer.is_banned() {
                                tracing::warn!("Transfer: not fetching {id} from {from} any more");
                            }
                        }
                        Err(WriteError::Removed) => return finish(&state, &id, &download),
                        Err(e) => tracing::warn!("Transfer: could not store chunk {index} of {id}: {e:?}"),
                    }
                }
                Event::Released { from, indices, fault } => {
                    if let Some(peer) = peers.get_mut(&from) {
                        for index in &indices {
                            peer.in_flight.remove(index);
                            peer.cancellable.remove(index);
                        }
                        peer.released(fault);
                        if peer.is_banned() {
                            tracing::warn!("Transfer: not fetching {id} from {from} any more ({fault:?})");
                        }
                    }
                }
                Event::Manifest { from, hashes } => {
                    if !blob.has_manifest() {
                        let root = blob.expected_root().unwrap_or_default();
                        match blob.set_manifest(hashes, &root) {
                            // Saved at once, so a restart can check the chunks on disk against it.
                            Ok(()) => blob.persist().await,
                            Err(why) => {
                                tracing::warn!("Transfer: bad manifest for {id} from {from}: {why}");
                                peers.entry(from).or_insert_with(|| Peer::new(Instant::now())).ban();
                            }
                        }
                    }
                }
                Event::Wake => {}
            }
        }

        if download.cancelled.load(Ordering::Relaxed) || blob.is_removed() {
            return finish(&state, &id, &download);
        }

        if blob.bitmap().is_full() && blob.has_manifest() {
            match blob.seal().await {
                Ok(_) if download.only_source.is_some() => crate::push::on_fetched(&state, &id).await,
                Ok(_) => {
                    crate::state::broadcast_catalog(&state).await;
                    crate::availability::announce_complete(&state, std::slice::from_ref(&id)).await;
                    tracing::info!("Transfer: {id} complete");
                }
                Err(why) => tracing::error!("Transfer: {id} failed verification: {why}"),
            }
            return finish(&state, &id, &download);
        }

        let connected: HashMap<NodeId, Link> = state
            .mesh_peers
            .read()
            .await
            .iter()
            .map(|(n, h)| (n.clone(), Link { control: h.sender.clone(), fetches: h.fetches.clone() }))
            .collect();
        let holders: Vec<NodeId> = match &download.only_source {
            Some(source) => vec![source.clone()],
            None => state.availability.holders(&id),
        };
        peers.retain(|node, p| connected.contains_key(node) || p.is_banned());

        if !blob.has_manifest() {
            if manifest_asked.is_none_or(|t| t.elapsed() > tuning.manifest_retry) {
                let holder = holders.iter().find(|n| connected.contains_key(*n) && !peers.get(*n).is_some_and(Peer::is_banned));
                if let Some(node) = holder {
                    let _ = connected[node].control.send(MeshMessage::GetManifest { file_id: id.clone() });
                    manifest_asked = Some(Instant::now());
                }
            }
        } else {
            let have = blob.bitmap();
            let mut sources: Vec<Source> = Vec::new();
            let mut source_nodes: Vec<NodeId> = Vec::new();
            for node in connected.keys() {
                let peer = peers.entry(node.clone()).or_insert_with(|| Peer::new(Instant::now()));
                if peer.is_banned() {
                    continue;
                }
                let has = match (&download.only_source, state.availability.get(node, &id)) {
                    // The one source of a file sent to a device holds all of it.
                    (Some(source), _) if source == node => None,
                    (None, Some(Have::Complete)) => None,
                    (None, Some(Have::Partial(map))) if map.len() == have.len() => Some(map),
                    _ => continue,
                };
                sources.push(Source { has, in_flight: peer.in_flight.len(), speed_bps: peer.speed_bps });
                source_nodes.push(node.clone());
            }

            let mut in_flight: HashMap<u32, Vec<usize>> = HashMap::new();
            for (s, node) in source_nodes.iter().enumerate() {
                for index in &peers[node].in_flight {
                    in_flight.entry(*index).or_default().push(s);
                }
            }
            let readers: Vec<u32> = download.readers.lock().unwrap().values().copied().collect();
            let want = priority_order(&have, &readers, &sources);
            let endgame = have.len() - have.count() <= endgame_threshold(chunk_count(blob.size()));
            let mut per_source: HashMap<usize, Vec<u32>> = HashMap::new();
            for (s, index) in plan(&want, &in_flight, &sources, endgame) {
                per_source.entry(s).or_default().push(index);
            }
            for (s, indices) in per_source {
                let node = &source_nodes[s];
                // With several sources, each endgame request asks for one chunk so it can be called off alone.
                let group_size = if endgame && source_nodes.len() > 1 { 1 } else { MAX_REQUEST_CHUNKS };
                for group in indices.chunks(group_size) {
                    let peer = peers.get_mut(node).unwrap();
                    peer.request_started(Instant::now());
                    peer.in_flight.extend(group);
                    let request = tokio::spawn(request_chunks(
                        connected[node].fetches.clone(),
                        download.events.clone(),
                        node.clone(),
                        id.clone(),
                        group.to_vec(),
                    ));
                    if let [index] = group {
                        peer.cancellable.insert(*index, request.abort_handle());
                    }
                }
            }
        }

        let have = blob.bitmap();
        let due = download.only_source.is_none()
            && match &told {
                None => have.count() > 0,
                Some((at, map)) => have.count() > map.count() && at.elapsed() >= tuning.map_interval,
            };
        if due {
            crate::availability::announce_progress(&state, &id, &have, told.as_ref().map(|(_, map)| map)).await;
            told = Some((Instant::now(), have));
        }

        if download.readers.lock().unwrap().is_empty() && last_progress.elapsed() > tuning.idle_exit {
            tracing::info!("Transfer: giving up on {id} for now (no progress)");
            return finish(&state, &id, &download);
        }
    }
}

// A chunk arrived, so requests for it still running at other nodes are dropped, which stops those nodes sending it.
fn call_off_duplicates(peers: &mut HashMap<NodeId, Peer>, delivered_by: &NodeId, index: u32) {
    for (node, peer) in peers.iter_mut().filter(|(node, _)| *node != delivered_by) {
        if let Some(request) = peer.cancellable.remove(&index) {
            request.abort();
            peer.in_flight.remove(&index);
            tracing::trace!("Transfer: called off chunk {index} at {node}");
        }
    }
}

fn finish(state: &NodeState, id: &str, download: &Arc<Download>) {
    let mut downloads = state.transfers.downloads.lock().unwrap();
    if downloads.get(id).is_some_and(|d| Arc::ptr_eq(d, download)) {
        downloads.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::availability::Have;
    use crate::catalog::{Add, CatalogEntry, EntryKind, Op};
    use crate::hlc::Stamp;
    use crate::identity::Identity;
    use crate::manifest::root_of;
    use crate::mesh::{dispatch, MeshPeerHandle};
    use crate::transfer::wire::{ChunkRange, ErrorCode};

    #[test]
    fn a_download_gives_up_after_sixty_seconds_without_progress() {
        assert_eq!(Tuning::default().stall_timeout, Duration::from_secs(60));
    }

    fn source(node: &str, has: Option<&[u32]>, len: u32, in_flight: usize, speed: f64) -> Source {
        let has = has.map(|chunks| {
            let mut map = Bitmap::new(len);
            for c in chunks {
                map.set(*c);
            }
            map
        });
        let _ = node;
        Source { has, in_flight, speed_bps: speed }
    }

    fn have(len: u32, present: &[u32]) -> Bitmap {
        let mut map = Bitmap::new(len);
        for c in present {
            map.set(*c);
        }
        map
    }

    #[test]
    fn chunks_just_ahead_of_a_reader_come_first_then_rarest_first() {
        let sources = [source("a", None, 100, 0, 0.0), source("b", Some(&[50, 51, 52]), 100, 0, 0.0)];
        let order = priority_order(&have(100, &[]), &[40], &sources);
        assert_eq!(&order[..16], (40..56).collect::<Vec<u32>>().as_slice(), "the reader's window first");
        assert_eq!(order[16], 0);
        assert_eq!(order.len(), 100);
    }

    #[test]
    fn rare_chunks_beat_common_ones_when_nobody_is_reading() {
        let sources =
            [source("a", Some(&[0, 1, 2, 3]), 4, 0, 0.0), source("b", Some(&[0, 1, 2]), 4, 0, 0.0), source("c", Some(&[0, 1]), 4, 0, 0.0)];
        assert_eq!(priority_order(&have(4, &[]), &[], &sources), [3, 2, 0, 1]);
    }

    #[test]
    fn chunks_nobody_has_and_chunks_we_have_are_not_planned() {
        let sources = [source("a", Some(&[0, 1]), 5, 0, 0.0)];
        assert_eq!(priority_order(&have(5, &[1]), &[], &sources), [0]);
    }

    #[test]
    fn only_chunks_a_source_has_are_requested_from_it() {
        let sources = [source("a", Some(&[0, 2]), 4, 0, 0.0), source("b", Some(&[1, 3]), 4, 0, 0.0)];
        let mut got: Vec<(usize, u32)> = plan(&[0, 1, 2, 3], &HashMap::new(), &sources, false);
        got.sort();
        assert_eq!(got, [(0, 0), (0, 2), (1, 1), (1, 3)]);
    }

    #[test]
    fn a_source_is_never_given_more_than_its_depth() {
        let sources = [source("a", None, 100, 0, 0.0)];
        let want: Vec<u32> = (0..100).collect();
        assert_eq!(plan(&want, &HashMap::new(), &sources, false).len(), MIN_DEPTH);

        let busy = [source("a", None, 100, MIN_DEPTH, 0.0)];
        assert!(plan(&want, &HashMap::new(), &busy, false).is_empty());
    }

    #[test]
    fn faster_sources_get_a_deeper_pipeline_and_more_chunks() {
        let slow = source("slow", None, 200, 0, 2.0 * CHUNK_SIZE as f64);
        let fast = source("fast", None, 200, 0, 40.0 * CHUNK_SIZE as f64);
        assert!(fast.depth() > slow.depth());
        assert_eq!(fast.depth(), 20);
        assert_eq!(slow.depth(), MIN_DEPTH);
        assert_eq!(source("x", None, 1, 0, 1e15).depth(), MAX_DEPTH);

        let want: Vec<u32> = (0..60).collect();
        let sources = [slow, fast];
        let planned = plan(&want, &HashMap::new(), &sources, false);
        let to_fast = planned.iter().filter(|(s, _)| *s == 1).count();
        let to_slow = planned.iter().filter(|(s, _)| *s == 0).count();
        assert!(to_fast > to_slow, "fast {to_fast} slow {to_slow}");
    }

    #[test]
    fn a_chunk_already_requested_is_not_requested_again_except_in_the_endgame() {
        let sources = [source("a", None, 10, 1, 0.0), source("b", None, 10, 0, 0.0)];
        let in_flight: HashMap<u32, Vec<usize>> = [(3, vec![0])].into();

        let normal = plan(&[3], &in_flight, &sources, false);
        assert!(normal.is_empty());

        let endgame = plan(&[3], &in_flight, &sources, true);
        assert_eq!(endgame, [(1, 3)]);
    }

    #[test]
    fn work_is_spread_across_equally_good_sources() {
        let sources = [source("a", None, 20, 0, 0.0), source("b", None, 20, 0, 0.0)];
        let planned = plan(&(0..8).collect::<Vec<u32>>(), &HashMap::new(), &sources, false);
        let to_a = planned.iter().filter(|(s, _)| *s == 0).count();
        assert_eq!((to_a, planned.len() - to_a), (4, 4));
    }

    // Sees every reply a node serves; returning false withholds it and leaves the requester waiting in silence.
    type Tamper = Arc<dyn Fn(&mut Reply) -> bool + Send + Sync>;

    const STALL: Duration = Duration::from_secs(2);

    fn data_pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| ((i * 7) as u8).wrapping_add(seed).wrapping_add((i >> 16) as u8)).collect()
    }

    // Makes `client` reach `server` through in-process streams that run the real request and serving code.
    async fn connect_one_way(client: &NodeState, server: &NodeState, tamper: Option<Tamper>) {
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<MeshMessage>();
        let (fetches_tx, mut fetches_rx) = mpsc::channel::<Fetch>(16);
        client.mesh_peers.write().await.insert(
            server.node_id.clone(),
            MeshPeerHandle {
                node_id: server.node_id.clone(),
                node_name: server.node_name.clone(),
                addr: "127.0.0.1:1".parse().unwrap(),
                sender: control_tx,
                fetches: fetches_tx,
                last_seen: Instant::now(),
                rtt_ms: None,
                hang_up: Default::default(),
                dialer: client.node_id.clone(),
            },
        );
        tokio::spawn({
            let (server, client_id) = (server.clone(), client.node_id.clone());
            async move {
                while let Some(message) = control_rx.recv().await {
                    dispatch(&message, &client_id, &server).await;
                }
            }
        });
        let (server, client_id) = (server.clone(), client.node_id.clone());
        tokio::spawn(async move {
            while let Some(fetch) = fetches_rx.recv().await {
                tokio::spawn(stream_through(server.clone(), client_id.clone(), fetch, tamper.clone()));
            }
        });
    }

    async fn stream_through(server: NodeState, client_id: NodeId, fetch: Fetch, tamper: Option<Tamper>) {
        let (client_end, server_end) = tokio::io::duplex(256 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_end);
        let (client_reader, client_writer) = tokio::io::split(client_end);
        tokio::spawn(async move { serve_stream(&server, &client_id, server_reader, server_writer).await });
        let (served_tx, mut served) = mpsc::channel(2);
        tokio::spawn(async move { wire::fetch_stream(client_reader, client_writer, &fetch.request, served_tx, STALL).await });
        let mut withheld = false;
        while let Some(mut reply) = served.recv().await {
            if tamper.as_ref().is_some_and(|tamper| !tamper(&mut reply)) {
                withheld = true;
                continue;
            }
            if fetch.replies.send(reply).await.is_err() {
                return;
            }
        }
        if withheld {
            // The requester gives up on a stream that stays silent, which closes it from this end.
            tokio::time::sleep(STALL).await;
        }
    }

    // `a` serves `b`, and chunks `a` sends pass through `tamper_a_to_b`.
    async fn link(a: &NodeState, b: &NodeState, tamper_a_to_b: Option<Tamper>) {
        connect_one_way(b, a, tamper_a_to_b).await;
        connect_one_way(a, b, None).await;
    }

    // Lists the entry in each node's catalog, as the mesh would after a publisher's add reached them.
    fn list(nodes: &[&NodeState], entry: &CatalogEntry) {
        let author = Identity::generate();
        let stamp = Stamp { wall: crate::hlc::wall_clock_ms(), counter: 0, node: author.node_id().to_string() };
        let add = Add::sign(&author, stamp, 1, vec![], entry.clone());
        for node in nodes {
            let received = node.catalog.receive(vec![Op::Add(add.clone())]);
            assert!(received.refused.is_empty(), "{:?}", received.refused);
        }
    }

    async fn share(node: &NodeState, others: &[&NodeState], id: &str, content: &[u8]) -> CatalogEntry {
        let blob = node.store.create(id, content.len() as u64).unwrap();
        for index in 0..blob.chunk_count() {
            let start = index as usize * CHUNK_SIZE as usize;
            let end = (start + CHUNK_SIZE as usize).min(content.len());
            blob.write_chunk_hashing(index, content[start..end].to_vec()).await.unwrap();
        }
        let entry = CatalogEntry {
            file_id: id.into(),
            name: format!("{id}.bin"),
            size: content.len() as u64,
            mime_type: "application/octet-stream".into(),
            created_at_ms: crate::hlc::wall_clock_ms(),
            manifest_root: blob.seal().await.unwrap(),
            uploader_id: "peer_a".into(),
            kind: EntryKind::File { parent: None },
        };
        let everyone: Vec<&NodeState> = std::iter::once(node).chain(others.iter().copied()).collect();
        list(&everyone, &entry);
        for n in others {
            n.availability.set_complete(&node.node_id, &[id.to_string()]);
        }
        entry
    }

    async fn read_all(blob: &Blob) -> Vec<u8> {
        let mut out = Vec::new();
        for index in 0..blob.chunk_count() {
            out.extend(blob.read_chunk(index).await.unwrap());
        }
        out
    }

    async fn wait_complete(blob: &Arc<Blob>, seconds: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            if blob.is_complete() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    // Whether `observer` has been told that `host` has the whole file.
    async fn is_known_host(observer: &NodeState, host: &NodeState, id: &str) -> bool {
        for _ in 0..100 {
            if observer.availability.hosts(id).contains(&host.node_id) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    fn nodes<const N: usize>(names: [&str; N]) -> [NodeState; N] {
        names.map(|n| NodeState::for_tests_node(n, None))
    }

    // What `server` answers `client_id` when it asks over one stream, ending when the stream does.
    async fn ask(server: &NodeState, client_id: &str, request: GetChunks) -> Vec<Reply> {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_end);
        let (client_reader, client_writer) = tokio::io::split(client_end);
        let (replies_tx, mut replies) = mpsc::channel(64);
        let asking = wire::fetch_stream(client_reader, client_writer, &request, replies_tx, Duration::from_secs(60));
        let client_id = client_id.to_string();
        let serving = serve_stream(server, &client_id, server_reader, server_writer);
        let (asked, _) = tokio::join!(asking, serving);
        asked.unwrap();
        let mut got = Vec::new();
        while let Ok(reply) = replies.try_recv() {
            got.push(reply);
        }
        got
    }

    fn chunk_of(content: &[u8], index: u32) -> Vec<u8> {
        let start = index as usize * CHUNK_SIZE as usize;
        content[start..(start + CHUNK_SIZE as usize).min(content.len())].to_vec()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_is_answered_with_each_chunk_asked_for_in_order() {
        let [a, b] = nodes(["node_a", "node_b"]);
        link(&a, &b, None).await;
        let content = data_pattern(4 * CHUNK_SIZE as usize + 10, 9);
        share(&a, &[], "file_x", &content).await;

        let replies = ask(&a, "node_b", GetChunks::for_indices("file_x", &[4, 1, 2])).await;

        let chunk = |index| Reply::ChunkData { index, bytes: chunk_of(&content, index) };
        assert_eq!(replies, [chunk(1), chunk(2), chunk(4)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chunks_a_node_does_not_hold_yet_are_reported_missing_while_the_rest_are_served() {
        let [a, b] = nodes(["node_a", "node_b"]);
        link(&a, &b, None).await;
        let content = data_pattern(3 * CHUNK_SIZE as usize, 10);
        let partial = a.store.create_with("file_x", content.len() as u64, Origin::Cached).unwrap();
        partial.write_chunk_hashing(1, chunk_of(&content, 1)).await.unwrap();

        let replies = ask(&a, "node_b", GetChunks::for_indices("file_x", &[0, 1, 2])).await;

        assert_eq!(
            replies,
            [Reply::Missing { index: 0 }, Reply::ChunkData { index: 1, bytes: chunk_of(&content, 1) }, Reply::Missing { index: 2 }]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_chunk_past_the_end_of_the_file_is_reported_missing() {
        let [a, b] = nodes(["node_a", "node_b"]);
        link(&a, &b, None).await;
        share(&a, &[], "file_x", &data_pattern(100, 11)).await;

        let replies = ask(&a, "node_b", GetChunks::for_indices("file_x", &[5])).await;

        assert_eq!(replies, [Reply::Missing { index: 5 }]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_for_a_file_this_node_lacks_is_answered_not_found() {
        let [a, b] = nodes(["node_a", "node_b"]);
        link(&a, &b, None).await;

        let replies = ask(&a, "node_b", GetChunks::for_indices("file_nope", &[0])).await;

        assert_eq!(replies, [Reply::Error(ErrorCode::NotFound)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_for_too_many_chunks_is_answered_malformed() {
        let [a, b] = nodes(["node_a", "node_b"]);
        link(&a, &b, None).await;
        share(&a, &[], "file_x", &data_pattern(100, 12)).await;
        let too_many = GetChunks { file_id: "file_x".into(), ranges: vec![ChunkRange { start: 0, count: MAX_REQUEST_CHUNKS as u32 + 1 }] };

        assert_eq!(ask(&a, "node_b", too_many).await, [Reply::Error(ErrorCode::Malformed)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_naming_an_invalid_file_id_is_answered_malformed() {
        let [a, b] = nodes(["node_a", "node_b"]);
        link(&a, &b, None).await;

        let replies = ask(&a, "node_b", GetChunks::for_indices("../etc/passwd", &[0])).await;

        assert_eq!(replies, [Reply::Error(ErrorCode::Malformed)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_that_is_not_a_connected_peer_is_not_served() {
        let [a] = nodes(["node_a"]);
        share(&a, &[], "file_x", &data_pattern(100, 13)).await;
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_end);
        let (_client_reader, mut client_writer) = tokio::io::split(client_end);
        wire::write_request(&mut client_writer, &GetChunks::for_indices("file_x", &[0])).await.unwrap();

        let served = serve_stream(&a, &"node_stranger".to_string(), server_reader, server_writer).await;

        assert_eq!(served.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_that_finds_every_upload_slot_taken_is_answered_busy() {
        let [a, b] = nodes(["node_a", "node_b"]);
        link(&a, &b, None).await;
        share(&a, &[], "file_x", &data_pattern(100, 14)).await;
        let mut taken = Vec::new();
        for _ in 0..UPLOAD_SLOTS {
            taken.push(a.transfers.uploads().acquire(&"node_c".to_string()).await);
        }

        let replies = ask(&a, "node_b", GetChunks::for_indices("file_x", &[0])).await;

        assert_eq!(replies, [Reply::Error(ErrorCode::Busy)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_that_never_sends_its_request_is_cut_off() {
        let [a, b] = nodes(["node_a", "node_b"]);
        link(&a, &b, None).await;
        let (_client_end, server_end) = tokio::io::duplex(64 * 1024);
        let (server_reader, server_writer) = tokio::io::split(server_end);

        let served = serve_stream(&a, &"node_b".to_string(), server_reader, server_writer).await;

        assert_eq!(served.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chunks_that_were_not_asked_for_are_ignored() {
        let (fetches_tx, mut fetches) = mpsc::channel::<Fetch>(1);
        let (events_tx, mut events) = mpsc::channel(8);
        let asking = tokio::spawn(request_chunks(fetches_tx, events_tx, "node_a".into(), "file_x".into(), vec![1]));

        let fetch = fetches.recv().await.unwrap();
        fetch.replies.send(Reply::ChunkData { index: 9, bytes: vec![0; 4] }).await.unwrap();
        fetch.replies.send(Reply::ChunkData { index: 1, bytes: vec![1; 4] }).await.unwrap();
        drop(fetch);
        asking.await.unwrap();

        assert!(matches!(events.recv().await, Some(Event::Chunk { index: 1, .. })));
        assert!(events.recv().await.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chunks_a_peer_never_answered_are_released_when_its_stream_ends() {
        let (fetches_tx, mut fetches) = mpsc::channel::<Fetch>(1);
        let (events_tx, mut events) = mpsc::channel(8);
        let asking = tokio::spawn(request_chunks(fetches_tx, events_tx, "node_a".into(), "file_x".into(), vec![1, 2]));

        let fetch = fetches.recv().await.unwrap();
        fetch.replies.send(Reply::ChunkData { index: 1, bytes: vec![1; 4] }).await.unwrap();
        drop(fetch);
        asking.await.unwrap();

        assert!(matches!(events.recv().await, Some(Event::Chunk { index: 1, .. })));
        let Some(Event::Released { indices, fault, .. }) = events.recv().await else { panic!("no release") };
        assert_eq!((indices, fault), (vec![2], Fault::Failed));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chunks_a_peer_says_it_lacks_are_released_as_missing() {
        let (fetches_tx, mut fetches) = mpsc::channel::<Fetch>(1);
        let (events_tx, mut events) = mpsc::channel(8);
        let asking = tokio::spawn(request_chunks(fetches_tx, events_tx, "node_a".into(), "file_x".into(), vec![1]));

        let fetch = fetches.recv().await.unwrap();
        fetch.replies.send(Reply::Missing { index: 1 }).await.unwrap();
        drop(fetch);
        asking.await.unwrap();

        let Some(Event::Released { indices, fault, .. }) = events.recv().await else { panic!("no release") };
        assert_eq!((indices, fault), (vec![1], Fault::Missing));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_busy_peer_releases_its_chunks_as_busy_not_failed() {
        let (fetches_tx, mut fetches) = mpsc::channel::<Fetch>(1);
        let (events_tx, mut events) = mpsc::channel(8);
        let asking = tokio::spawn(request_chunks(fetches_tx, events_tx, "node_a".into(), "file_x".into(), vec![1, 2]));

        let fetch = fetches.recv().await.unwrap();
        fetch.replies.send(Reply::Error(ErrorCode::Busy)).await.unwrap();
        asking.await.unwrap();

        let Some(Event::Released { fault, .. }) = events.recv().await else { panic!("no release") };
        assert_eq!(fault, Fault::Busy);
    }

    // A node holding only the first `chunks` chunks of a file and telling its peers so; `seed.finish` adds the rest.
    struct PartialHolder {
        node: NodeState,
        blob: Arc<Blob>,
        content: Vec<u8>,
    }

    impl PartialHolder {
        async fn new(node: NodeState, entry: &CatalogEntry, hashes: Vec<ChunkHash>, content: Vec<u8>, chunks: u32) -> Self {
            let blob = node.store.create(&entry.file_id, entry.size).unwrap();
            blob.set_manifest(hashes, &entry.manifest_root).unwrap();
            let holder = Self { node, blob, content };
            holder.write(0..chunks).await;
            crate::availability::announce_progress(&holder.node, &entry.file_id, &holder.blob.bitmap(), None).await;
            holder
        }

        async fn write(&self, indices: std::ops::Range<u32>) {
            for index in indices {
                let start = index as usize * CHUNK_SIZE as usize;
                let end = (start + CHUNK_SIZE as usize).min(self.content.len());
                self.blob.write_chunk_verified(index, self.content[start..end].to_vec()).await.unwrap();
            }
        }

        async fn finish(&self) {
            self.write(self.blob.bitmap().iter_missing().collect::<Vec<u32>>().first().copied().unwrap_or(0)..self.blob.chunk_count())
                .await;
            self.blob.seal().await.unwrap();
            crate::availability::announce_complete(&self.node, &[self.blob.id().to_string()]).await;
        }
    }

    async fn wait_until(seconds: u64, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    // The whole file on a node of its own, which the nodes under test never talk to.
    async fn whole_file(content: &[u8]) -> (CatalogEntry, Vec<ChunkHash>) {
        let [origin] = nodes(["node_origin"]);
        let entry = share(&origin, &[], "file_x", content).await;
        (entry, origin.store.get("file_x").unwrap().hashes().unwrap().to_vec())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_still_downloading_serves_the_chunks_it_has_to_a_node_that_cannot_reach_the_source() {
        let [a, b, c] = nodes(["node_a", "node_b", "node_c"]);
        link(&a, &b, None).await;
        link(&b, &c, None).await;
        let content = data_pattern(12 * CHUNK_SIZE as usize, 5);
        let (entry, hashes) = whole_file(&content).await;
        list(&[&a, &b, &c], &entry);
        let seed = PartialHolder::new(a, &entry, hashes, content.clone(), 3).await;
        assert!(wait_until(10, || b.availability.get("node_a", "file_x").is_some()).await);

        let at_b = ensure_file(&b, "file_x").await.unwrap();
        assert!(wait_until(10, || at_b.bitmap().count() == 3).await);
        let at_c = ensure_file(&c, "file_x").await.unwrap();

        assert!(wait_until(10, || at_c.bitmap().count() == 3).await, "c has {} chunks", at_c.bitmap().count());
        assert!(!at_b.is_complete(), "nobody had the rest yet");
        seed.finish().await;
        assert!(wait_complete(&at_c, 20).await);
        assert_eq!(read_all(&at_c).await, content);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_is_told_new_chunks_as_they_are_verified_not_only_the_whole_file() {
        let [a, b, c] = nodes(["node_a", "node_b", "node_c"]);
        link(&a, &b, None).await;
        link(&b, &c, None).await;
        let content = data_pattern(8 * CHUNK_SIZE as usize, 6);
        let (entry, hashes) = whole_file(&content).await;
        list(&[&a, &b, &c], &entry);
        let seed = PartialHolder::new(a, &entry, hashes, content, 2).await;
        assert!(wait_until(10, || b.availability.get("node_a", "file_x").is_some()).await);
        let at_b = ensure_file(&b, "file_x").await.unwrap();
        let told = |count: u32| matches!(c.availability.get("node_b", "file_x"), Some(Have::Partial(map)) if map.count() == count);

        assert!(wait_until(10, || at_b.bitmap().count() == 2).await);
        assert!(wait_until(10, || told(2)).await);
        seed.finish().await;
        assert!(wait_complete(&at_b, 20).await);
        assert!(is_known_host(&c, &b, "file_x").await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chunks_announced_before_their_map_are_answered_by_asking_for_the_map() {
        let [a, b] = nodes(["node_a", "node_b"]);
        link(&a, &b, None).await;
        let content = data_pattern(4 * CHUNK_SIZE as usize, 7);
        let entry = share(&b, &[], "file_x", &content).await;
        list(&[&a], &entry);
        let partial = a.store.create("file_x", content.len() as u64).unwrap();
        for index in 0..2 {
            let start = index as usize * CHUNK_SIZE as usize;
            partial.write_chunk_hashing(index, content[start..start + CHUNK_SIZE as usize].to_vec()).await.unwrap();
        }

        crate::availability::on_have_chunks(&b, &"node_a".to_string(), "file_x", 4, &[(1, 1)]).await;

        let learned = wait_until(10, || matches!(b.availability.get("node_a", "file_x"), Some(Have::Partial(map)) if map.count() == 2));
        assert!(learned.await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_first_copy_of_a_chunk_calls_off_the_requests_for_it_at_other_nodes() {
        let pending = tokio::spawn(std::future::pending::<()>());
        let mut peers: HashMap<NodeId, Peer> = HashMap::new();
        let mut slow = Peer::new(Instant::now());
        slow.in_flight.insert(7);
        slow.cancellable.insert(7, pending.abort_handle());
        peers.insert("slow".to_string(), slow);
        peers.insert("fast".to_string(), Peer::new(Instant::now()));

        call_off_duplicates(&mut peers, &"fast".to_string(), 7);

        assert!(pending.await.unwrap_err().is_cancelled());
        assert!(peers["slow"].in_flight.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_node_that_delivered_a_chunk_is_not_called_off() {
        let pending = tokio::spawn(std::future::pending::<()>());
        let mut fast = Peer::new(Instant::now());
        fast.cancellable.insert(7, pending.abort_handle());
        let mut peers: HashMap<NodeId, Peer> = HashMap::from([("fast".to_string(), fast)]);

        call_off_duplicates(&mut peers, &"fast".to_string(), 7);

        assert!(!pending.is_finished());
        pending.abort();
    }

    #[test]
    fn the_endgame_is_the_last_percent_of_a_file_within_limits() {
        assert_eq!(endgame_threshold(2), 2);
        assert_eq!(endgame_threshold(100), 4);
        assert_eq!(endgame_threshold(1000), 10);
        assert_eq!(endgame_threshold(100_000), 64);
    }

    #[test]
    fn in_the_endgame_every_source_that_can_is_asked_for_each_chunk() {
        let sources = [source("a", None, 10, 0, 0.0), source("b", None, 10, 0, 0.0), source("c", Some(&[1]), 10, 0, 0.0)];

        let mut got = plan(&[0, 1], &HashMap::new(), &sources, true);
        got.sort();

        assert_eq!(got, [(0, 0), (0, 1), (1, 0), (1, 1), (2, 1)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_fetches_a_file_from_the_node_that_holds_it() {
        let [a, b] = nodes(["node_a", "node_b"]);
        link(&a, &b, None).await;
        let content = data_pattern(5 * CHUNK_SIZE as usize + 777, 1);
        share(&a, &[&b], "file_x", &content).await;

        let blob = ensure_file(&b, "file_x").await.unwrap();
        assert!(wait_complete(&blob, 10).await, "download did not finish");

        assert_eq!(read_all(&blob).await, content);
        assert_eq!(blob.manifest_root(), a.store.get("file_x").unwrap().manifest_root());
        assert!(is_known_host(&a, &b, "file_x").await, "a should be told that b now has the file");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(b.transfers.active(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_download_cut_short_by_a_restart_continues_once_a_source_connects() {
        let [a, b] = nodes(["node_a", "node_b"]);
        let content = data_pattern(4 * CHUNK_SIZE as usize, 12);
        let entry = share(&a, &[], "file_x", &content).await;
        list(&[&b], &entry);
        let partial = b.store.create_with("file_x", entry.size, Origin::Cached).unwrap();
        partial.set_manifest(a.store.get("file_x").unwrap().hashes().unwrap(), &entry.manifest_root).unwrap();
        partial.write_chunk_verified(2, chunk_of(&content, 2)).await.unwrap();

        remember_interrupted(&b);
        sources_changed(&b);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(b.transfers.active(), 0, "nothing starts while no node has the file");

        link(&a, &b, None).await;
        b.availability.set_complete(&a.node_id, &["file_x".to_string()]);
        sources_changed(&b);

        assert!(wait_complete(&partial, 10).await, "the download did not continue");
        assert_eq!(read_all(&partial).await, content);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_already_on_disk_is_served_without_any_transfer() {
        let [a] = nodes(["node_a"]);
        share(&a, &[], "file_x", &data_pattern(1000, 2)).await;
        let blob = ensure_file(&a, "file_x").await.unwrap();
        assert!(blob.is_complete());
        assert_eq!(a.transfers.active(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn asking_for_a_missing_or_unavailable_file_fails_cleanly() {
        let [a, b] = nodes(["node_a", "node_b"]);
        assert_eq!(ensure_file(&a, "file_nope").await.err(), Some(FetchError::NotFound));

        share(&a, &[&b], "file_x", &data_pattern(1000, 2)).await;
        assert_eq!(ensure_file(&b, "file_x").await.err(), Some(FetchError::NoOneHasIt));
        assert!(b.store.get("file_x").is_none(), "nothing is created for a file that can't be fetched");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn chunks_are_fetched_from_several_holders_at_once() {
        let [a, c, b] = nodes(["node_a", "node_c", "node_b"]);
        let from_a = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let from_c = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = |n: Arc<std::sync::atomic::AtomicUsize>| -> Tamper {
            Arc::new(move |reply| {
                if matches!(reply, Reply::ChunkData { .. }) {
                    n.fetch_add(1, Ordering::Relaxed);
                }
                true
            })
        };
        link(&a, &b, Some(counter(from_a.clone()))).await;
        link(&c, &b, Some(counter(from_c.clone()))).await;
        let content = data_pattern(24 * CHUNK_SIZE as usize, 3);
        share(&a, &[&b, &c], "file_x", &content).await;
        share(&c, &[&a, &b], "file_x", &content).await;
        b.availability.set_complete("node_a", &["file_x".to_string()]);
        b.availability.set_complete("node_c", &["file_x".to_string()]);

        let blob = ensure_file(&b, "file_x").await.unwrap();
        assert!(wait_complete(&blob, 15).await);
        assert_eq!(read_all(&blob).await, content);
        let (n_a, n_c) = (from_a.load(Ordering::Relaxed), from_c.load(Ordering::Relaxed));
        assert!(n_a > 0 && n_c > 0, "both sources should have served chunks (a: {n_a}, c: {n_c})");
        assert!((24..=24 + endgame_threshold(24) as usize).contains(&(n_a + n_c)), "frames: {}", n_a + n_c);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_that_sends_corrupted_data_is_dropped_and_the_file_still_completes() {
        let [a, evil, b] = nodes(["node_a", "node_evil", "node_b"]);
        link(&a, &b, None).await;
        let corrupted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let flip: Tamper = {
            let corrupted = corrupted.clone();
            Arc::new(move |reply| {
                if let Reply::ChunkData { bytes, .. } = reply {
                    let last = bytes.len() - 1;
                    bytes[last] ^= 0xff;
                    corrupted.fetch_add(1, Ordering::Relaxed);
                }
                true
            })
        };
        link(&evil, &b, Some(flip)).await;
        let content = data_pattern(12 * CHUNK_SIZE as usize, 4);
        share(&a, &[&b, &evil], "file_x", &content).await;
        share(&evil, &[&a, &b], "file_x", &content).await;
        b.availability.set_complete("node_a", &["file_x".to_string()]);
        b.availability.set_complete("node_evil", &["file_x".to_string()]);

        let blob = ensure_file(&b, "file_x").await.unwrap();
        assert!(wait_complete(&blob, 15).await);
        assert_eq!(read_all(&blob).await, content, "the file must be intact despite the bad source");
        let sent = corrupted.load(Ordering::Relaxed);
        assert!(sent >= 1, "the evil node was asked for something");
        assert!(sent <= 2 + MAX_DEPTH, "it was dropped soon after its second bad chunk (sent {sent})");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn when_a_source_stops_answering_its_chunks_are_fetched_elsewhere() {
        let [a, dead, b] = nodes(["node_a", "node_dead", "node_b"]);
        link(&a, &b, None).await;
        let black_hole: Tamper = Arc::new(|_| false);
        link(&dead, &b, Some(black_hole)).await;
        let content = data_pattern(10 * CHUNK_SIZE as usize, 5);
        share(&a, &[&b, &dead], "file_x", &content).await;
        share(&dead, &[&a, &b], "file_x", &content).await;
        b.availability.set_complete("node_a", &["file_x".to_string()]);
        b.availability.set_complete("node_dead", &["file_x".to_string()]);

        let blob = ensure_file(&b, "file_x").await.unwrap();
        assert!(wait_complete(&blob, 20).await, "the download should recover from a silent source");
        assert_eq!(read_all(&blob).await, content);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_manifest_that_does_not_match_the_catalog_is_refused() {
        let [a, b] = nodes(["node_a", "node_b"]);
        link(&a, &b, None).await;
        let content = data_pattern(3 * CHUNK_SIZE as usize, 6);
        let entry = share(&a, &[], "file_x", &content).await;
        let wrong = CatalogEntry { manifest_root: root_of(content.len() as u64, &[[7; 32]; 3]), ..entry };
        list(&[&b], &wrong);
        b.availability.set_complete("node_a", &["file_x".to_string()]);

        let blob = ensure_file(&b, "file_x").await.unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(!blob.is_complete());
        assert!(!blob.has_manifest(), "a manifest that doesn't match the root must not be accepted");
        assert_eq!(blob.bitmap().count(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsharing_stops_the_download_and_deletes_the_partial_file() {
        let [a, b] = nodes(["node_a", "node_b"]);
        let slow: Tamper = Arc::new(|_| false);
        link(&a, &b, Some(slow)).await;
        share(&a, &[&b], "file_x", &data_pattern(4 * CHUNK_SIZE as usize, 7)).await;
        let blob = ensure_file(&b, "file_x").await.unwrap();
        assert_eq!(b.transfers.active(), 1);

        crate::state::forget_file(&b, "file_x").await;

        assert!(blob.is_removed());
        assert!(b.store.get("file_x").is_none());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(b.transfers.active(), 0, "the download task ends");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_spreads_from_node_to_node_when_the_original_holder_is_out_of_reach() {
        let [a, b, c] = nodes(["node_a", "node_b", "node_c"]);
        link(&a, &b, None).await;
        link(&a, &c, Some(Arc::new(|_| false))).await;
        link(&b, &c, None).await;
        let content = data_pattern(10 * CHUNK_SIZE as usize, 8);
        share(&a, &[&b, &c], "file_x", &content).await;

        let at_b = ensure_file(&b, "file_x").await.unwrap();
        let at_c = ensure_file(&c, "file_x").await.unwrap();
        assert!(wait_complete(&at_b, 15).await);
        assert!(wait_complete(&at_c, 20).await, "c should get the file from b");
        assert_eq!(read_all(&at_c).await, content);
        assert!(is_known_host(&b, &c, "file_x").await, "b should be told that c now has the file");
    }
}
