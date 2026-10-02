// Moving file chunks between nodes.
//
// A node that is asked for a file it doesn't have starts a `Download`: it
// fetches the file's manifest (the chunk hashes) from a holder, then requests
// chunks from every connected node that has them, in parallel. Each chunk is
// verified against the manifest before it is written, so sources can be mixed
// freely and a node that sends bad data is dropped for that file. A node
// starts serving the chunks it has as soon as it has them, not only once the
// whole file is in, so a download speeds up the more nodes join in.
//
// Chunks travel as binary frames over the authenticated mesh connection. The
// serving side sends through a small bounded queue, so a slow link slows the
// reader down instead of piling chunks up in memory.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::bitmap::Bitmap;
use crate::mesh::MeshMessage;
use crate::store::{chunk_count, Blob, ChunkHash, StoreError, WriteError, CHUNK_SIZE};
use crate::types::{FileMetadata, NodeId};
use crate::validate;
use crate::NodeState;

// ── Wire format ──────────────────────────────────────────────────────────

const FRAME_CHUNK: u8 = 1;
// Most chunks one GetChunks may ask for.
pub const MAX_REQUEST_CHUNKS: usize = 16;
// Requests a node will be serving to one peer at a time.
pub const SERVE_SLOTS: usize = 8;

pub fn encode_chunk_frame(file_id: &str, index: u32, data: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(data.len() + file_id.len() + 6);
    frame.push(FRAME_CHUNK);
    frame.push(file_id.len() as u8);
    frame.extend_from_slice(file_id.as_bytes());
    frame.extend_from_slice(&index.to_be_bytes());
    frame.extend_from_slice(data);
    frame
}

pub fn decode_chunk_frame(frame: &[u8]) -> Option<(&str, u32, &[u8])> {
    if *frame.first()? != FRAME_CHUNK {
        return None;
    }
    let id_len = *frame.get(1)? as usize;
    let id = std::str::from_utf8(frame.get(2..2 + id_len)?).ok()?;
    let index = u32::from_be_bytes(frame.get(2 + id_len..6 + id_len)?.try_into().ok()?);
    Some((id, index, &frame[6 + id_len..]))
}

// ── Scheduling ───────────────────────────────────────────────────────────

// Chunks asked for ahead of a reader that is streaming the file.
const READAHEAD: u32 = 16;
// Only this many missing chunks are weighed against each other (rarest first),
// to keep planning cheap for very large files.
const PLANNING_WINDOW: usize = 4096;
const ENDGAME_CHUNKS: u32 = 4;
const MIN_DEPTH: usize = 4;
const MAX_DEPTH: usize = 32;
// Aim to keep about this much data in flight per source.
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

    // How many chunks to keep outstanding with this source: enough to cover
    // its latency at its measured speed, within sane bounds.
    pub fn depth(&self) -> usize {
        if self.speed_bps <= 0.0 {
            return MIN_DEPTH;
        }
        ((self.speed_bps * TARGET_IN_FLIGHT_SECS / CHUNK_SIZE as f64).ceil() as usize).clamp(MIN_DEPTH, MAX_DEPTH)
    }
}

// The missing chunks worth asking for, most urgent first: those just ahead of
// each reader streaming the file, then the rest rarest-first (so chunks only
// few nodes have get copied early), by position among equals.
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

// Decides which chunk to ask which source for. `in_flight` maps each chunk
// already requested to the sources it was requested from. A chunk is only
// requested twice in endgame, when the few chunks left should come from
// whichever source answers first rather than wait on a slow one.
pub fn plan(want: &[u32], in_flight: &HashMap<u32, Vec<usize>>, sources: &[Source], endgame: bool) -> Vec<(usize, u32)> {
    let mut room: Vec<usize> = sources.iter().map(|s| s.depth().saturating_sub(s.in_flight)).collect();
    let mut assigned = vec![0usize; sources.len()];
    let mut requests = Vec::new();

    for &index in want {
        let already = in_flight.get(&index);
        if already.is_some() && !endgame {
            continue;
        }
        let best = (0..sources.len())
            .filter(|&s| room[s] > 0 && sources[s].has(index) && !already.is_some_and(|a| a.contains(&s)))
            .max_by(|&a, &b| {
                let score = |s: usize| sources[s].speed_bps.max(1.0) / (sources[s].in_flight + assigned[s] + 1) as f64;
                score(a).total_cmp(&score(b)).then(b.cmp(&a))
            });
        if let Some(s) = best {
            room[s] -= 1;
            assigned[s] += 1;
            requests.push((s, index));
        }
    }
    requests
}

// ── Downloads ────────────────────────────────────────────────────────────

pub struct Tuning {
    pub request_timeout: Duration,
    pub tick: Duration,
    // A download nobody is waiting for stops after this long without progress.
    pub idle_exit: Duration,
    pub map_interval: Duration,
    // A download being streamed to a client gives up if no chunk arrives for this long.
    pub stall_timeout: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(20),
            tick: Duration::from_millis(250),
            idle_exit: Duration::from_secs(600),
            map_interval: Duration::from_secs(2),
            stall_timeout: Duration::from_secs(60),
        }
    }
}

enum Event {
    Chunk { from: NodeId, index: u32, data: Vec<u8> },
    ChunkError { from: NodeId, index: u32 },
    Manifest { from: NodeId, hashes: Vec<ChunkHash> },
    Wake,
}

pub struct Download {
    pub blob: Arc<Blob>,
    events: mpsc::Sender<Event>,
    // Where each stream that is reading this file currently is (chunk index).
    readers: Mutex<HashMap<u64, u32>>,
    next_reader: AtomicU64,
    // What partial holders (nodes still fetching the file themselves) have.
    partial: Mutex<HashMap<NodeId, Bitmap>>,
    cancelled: AtomicBool,
}

// While held, the download prioritises chunks at the reader's position.
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
    tuning: Tuning,
}

impl Transfers {
    pub fn new() -> Self {
        Self::with_tuning(Tuning::default())
    }

    pub fn with_tuning(tuning: Tuning) -> Self {
        Self { downloads: Mutex::new(HashMap::new()), tuning }
    }

    fn get(&self, id: &str) -> Option<Arc<Download>> {
        self.downloads.lock().unwrap().get(id).cloned()
    }

    pub fn stall_timeout(&self) -> Duration {
        self.tuning.stall_timeout
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

// What a node does when asked for a file: serve it from disk if complete,
// otherwise start (or join) fetching it from the other nodes.
pub async fn ensure_file(state: &NodeState, id: &str) -> Result<Arc<Blob>, FetchError> {
    if let Some(blob) = state.store.get(id) {
        if blob.is_complete() {
            return Ok(blob);
        }
        if state.transfers.get(id).is_some() {
            return Ok(blob);
        }
    }

    let entry: FileMetadata = {
        let files = state.files.read().await;
        files.get(id).filter(|f| !f.deleted).cloned().ok_or(FetchError::NotFound)?
    };
    let root = entry.manifest_root.clone().ok_or(FetchError::NotFound)?;
    if !someone_online_has(state, &entry).await {
        return Err(FetchError::NoOneHasIt);
    }

    let blob = state.store.create(id, entry.size).map_err(FetchError::Store)?;
    blob.expect_root(&root);
    blob.set_entry(entry);
    if blob.is_complete() {
        return Ok(blob);
    }

    let (events, receiver) = mpsc::channel(512);
    let download = Arc::new(Download {
        blob: blob.clone(),
        events,
        readers: Mutex::new(HashMap::new()),
        next_reader: AtomicU64::new(1),
        partial: Mutex::new(HashMap::new()),
        cancelled: AtomicBool::new(false),
    });
    {
        let mut downloads = state.transfers.downloads.lock().unwrap();
        if downloads.contains_key(id) {
            return Ok(blob); // another request started it meanwhile
        }
        downloads.insert(id.to_string(), download.clone());
    }
    tokio::spawn(run(state.clone(), download, receiver));
    Ok(blob)
}

async fn someone_online_has(state: &NodeState, entry: &FileMetadata) -> bool {
    let peers = state.mesh_peers.read().await;
    entry.holder_nodes().any(|node| peers.contains_key(node))
}

// Registers a stream reading `blob`, so its chunks are fetched first.
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

// Something about who can serve what changed (a node connected or left, a
// catalog update arrived): let downloads re-plan now instead of at the next tick.
pub fn sources_changed(state: &NodeState) {
    let downloads: Vec<Arc<Download>> = state.transfers.downloads.lock().unwrap().values().cloned().collect();
    for download in downloads {
        let _ = download.events.try_send(Event::Wake);
    }
}

// ── Input from the mesh ──────────────────────────────────────────────────

pub fn on_binary_frame(state: &NodeState, from: &NodeId, frame: &[u8]) {
    let Some((file_id, index, data)) = decode_chunk_frame(frame) else { return };
    if let Some(download) = state.transfers.get(file_id) {
        let _ = download.events.try_send(Event::Chunk { from: from.clone(), index, data: data.to_vec() });
    }
}

pub fn on_chunk_error(state: &NodeState, from: &NodeId, file_id: &str, index: u32) {
    if let Some(download) = state.transfers.get(file_id) {
        let _ = download.events.try_send(Event::ChunkError { from: from.clone(), index });
    }
}

pub fn on_manifest(state: &NodeState, from: &NodeId, file_id: &str, hashes_hex: &str) {
    let Some(download) = state.transfers.get(file_id) else { return };
    let Ok(raw) = hex::decode(hashes_hex) else { return };
    let hashes = raw.chunks_exact(32).map(|c| <ChunkHash>::try_from(c).unwrap()).collect();
    let _ = download.events.try_send(Event::Manifest { from: from.clone(), hashes });
}

pub fn on_chunk_map(state: &NodeState, from: &NodeId, file_id: &str, chunks: u32, bitmap_hex: &str) {
    let Some(download) = state.transfers.get(file_id) else { return };
    if chunks != download.blob.chunk_count() {
        return;
    }
    if let Some(map) = Bitmap::from_hex(chunks, bitmap_hex) {
        download.partial.lock().unwrap().insert(from.clone(), map);
        let _ = download.events.try_send(Event::Wake);
    }
}

// ── Serving other nodes ──────────────────────────────────────────────────

pub async fn serve_manifest(state: &NodeState, to: &NodeId, file_id: &str) {
    let Some(blob) = state.store.get(file_id) else { return };
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

// Sends the requested chunks we have. Each frame goes through the peer's
// bounded queue, so this waits whenever the link is behind.
pub async fn serve_chunks(state: &NodeState, to: &NodeId, file_id: &str, indices: Vec<u32>) {
    if !validate::is_valid_id(file_id) || indices.len() > MAX_REQUEST_CHUNKS {
        return;
    }
    let (control, data, slots) = match state.mesh_peers.read().await.get(to) {
        Some(peer) => (peer.sender.clone(), peer.data.clone(), peer.serve_slots.clone()),
        None => return,
    };
    let reject = |reason: &str, indices: &[u32]| {
        for &index in indices {
            let _ = control.send(MeshMessage::ChunkError { file_id: file_id.to_string(), index, reason: reason.to_string() });
        }
    };
    let Ok(permit) = slots.try_acquire_owned() else {
        return reject("busy", &indices);
    };
    let Some(blob) = state.store.get(file_id) else {
        return reject("not found", &indices);
    };

    let file_id = file_id.to_string();
    tokio::spawn(async move {
        let _permit = permit;
        for index in indices {
            match blob.read_chunk_verified(index).await {
                Ok(bytes) => {
                    if data.send(encode_chunk_frame(&file_id, index, &bytes)).await.is_err() {
                        return; // the peer went away
                    }
                }
                Err(_) => {
                    let _ = control.send(MeshMessage::ChunkError { file_id: file_id.clone(), index, reason: "not available".into() });
                }
            }
        }
    });
}

// ── The download task ────────────────────────────────────────────────────

struct Peer {
    in_flight: HashMap<u32, Instant>,
    speed_bps: f64,
    last_arrival: Instant,
    bad_chunks: u32,
    banned: bool,
}

impl Peer {
    fn new() -> Self {
        Self { in_flight: HashMap::new(), speed_bps: 0.0, last_arrival: Instant::now(), bad_chunks: 0, banned: false }
    }

    fn record_arrival(&mut self, bytes: usize) {
        let now = Instant::now();
        let seconds = now.duration_since(self.last_arrival).as_secs_f64().max(0.001);
        let sample = bytes as f64 / seconds;
        self.speed_bps = if self.speed_bps == 0.0 { sample } else { 0.7 * self.speed_bps + 0.3 * sample };
        self.last_arrival = now;
    }
}

const BAN_AFTER_BAD_CHUNKS: u32 = 2;
const MANIFEST_RETRY: Duration = Duration::from_secs(10);

async fn run(state: NodeState, download: Arc<Download>, mut events: mpsc::Receiver<Event>) {
    let blob = download.blob.clone();
    let id = blob.id().to_string();
    let tuning = &state.transfers.tuning;
    let mut peers: HashMap<NodeId, Peer> = HashMap::new();
    let mut ticker = tokio::time::interval(tuning.tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut manifest_asked: Option<Instant> = None;
    let mut last_progress = Instant::now();
    let mut last_map: (Instant, u32) = (Instant::now(), 0);

    loop {
        // Wait for something to happen, then handle everything that is ready.
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
                Event::Chunk { from, index, data } => {
                    let len = data.len();
                    let peer = peers.entry(from.clone()).or_insert_with(Peer::new);
                    if peer.in_flight.remove(&index).is_none() {
                        continue; // not something we asked this node for
                    }
                    match blob.write_chunk_verified(index, data).await {
                        Ok(()) => {
                            peer.record_arrival(len);
                            last_progress = Instant::now();
                        }
                        Err(WriteError::HashMismatch) => {
                            peer.bad_chunks += 1;
                            tracing::warn!("Transfer: {from} sent a corrupted chunk {index} of {id}");
                            if peer.bad_chunks >= BAN_AFTER_BAD_CHUNKS {
                                peer.banned = true;
                                tracing::warn!("Transfer: not fetching {id} from {from} any more");
                            }
                        }
                        Err(WriteError::Removed) => return finish(&state, &id, &download),
                        Err(e) => tracing::warn!("Transfer: could not store chunk {index} of {id}: {e:?}"),
                    }
                }
                Event::ChunkError { from, index } => {
                    if let Some(peer) = peers.get_mut(&from) {
                        peer.in_flight.remove(&index);
                        peer.speed_bps *= 0.5;
                    }
                }
                Event::Manifest { from, hashes } => {
                    if !blob.has_manifest() {
                        let root = blob.expected_root().unwrap_or_default();
                        if let Err(why) = blob.set_manifest(hashes, &root) {
                            tracing::warn!("Transfer: bad manifest for {id} from {from}: {why}");
                            peers.entry(from).or_insert_with(Peer::new).banned = true;
                        }
                    }
                }
                Event::Wake => {}
            }
        }

        if download.cancelled.load(Ordering::Relaxed) || blob.is_removed() {
            return finish(&state, &id, &download);
        }

        // The file is in: check it, keep it, and announce that we can serve it.
        if blob.bitmap().is_full() && blob.has_manifest() {
            match blob.seal() {
                Ok(_) => {
                    blob.persist().await;
                    crate::state::add_self_as_holder(&state, &id).await;
                    tracing::info!("Transfer: {id} complete");
                }
                Err(why) => tracing::error!("Transfer: {id} failed verification: {why}"),
            }
            return finish(&state, &id, &download);
        }

        // Who we could ask: connected nodes that hold the whole file, and ones still fetching it that have chunks.
        let connected: HashMap<NodeId, mpsc::UnboundedSender<MeshMessage>> =
            state.mesh_peers.read().await.iter().map(|(n, h)| (n.clone(), h.sender.clone())).collect();
        let holders: HashSet<NodeId> = {
            let files = state.files.read().await;
            files.get(&id).map(|f| f.holder_nodes().cloned().collect()).unwrap_or_default()
        };
        peers.retain(|node, p| connected.contains_key(node) || p.banned);

        if !blob.has_manifest() {
            if manifest_asked.is_none_or(|t| t.elapsed() > MANIFEST_RETRY) {
                let holder = holders.iter().find(|n| connected.contains_key(*n) && !peers.get(*n).is_some_and(|p| p.banned));
                if let Some(node) = holder {
                    let _ = connected[node].send(MeshMessage::GetManifest { file_id: id.clone() });
                    manifest_asked = Some(Instant::now());
                }
            }
        } else {
            // Requests that went unanswered: the chunk goes back to the pool.
            for peer in peers.values_mut() {
                let before = peer.in_flight.len();
                peer.in_flight.retain(|_, asked| asked.elapsed() < tuning.request_timeout);
                if peer.in_flight.len() < before {
                    peer.speed_bps *= 0.5;
                }
            }

            let have = blob.bitmap();
            let partial = download.partial.lock().unwrap().clone();
            let mut sources: Vec<Source> = Vec::new();
            let mut source_nodes: Vec<NodeId> = Vec::new();
            for node in connected.keys() {
                let peer = peers.entry(node.clone()).or_insert_with(Peer::new);
                if peer.banned {
                    continue;
                }
                let has = if holders.contains(node) {
                    None
                } else if let Some(map) = partial.get(node) {
                    Some(map.clone())
                } else {
                    continue;
                };
                sources.push(Source { has, in_flight: peer.in_flight.len(), speed_bps: peer.speed_bps });
                source_nodes.push(node.clone());
            }

            let mut in_flight: HashMap<u32, Vec<usize>> = HashMap::new();
            for (s, node) in source_nodes.iter().enumerate() {
                for index in peers[node].in_flight.keys() {
                    in_flight.entry(*index).or_default().push(s);
                }
            }
            let readers: Vec<u32> = download.readers.lock().unwrap().values().copied().collect();
            let want = priority_order(&have, &readers, &sources);
            let endgame = have.len() - have.count() <= ENDGAME_CHUNKS.min(chunk_count(blob.size()));
            let mut per_source: HashMap<usize, Vec<u32>> = HashMap::new();
            for (s, index) in plan(&want, &in_flight, &sources, endgame) {
                per_source.entry(s).or_default().push(index);
            }
            for (s, indices) in per_source {
                let node = &source_nodes[s];
                for group in indices.chunks(MAX_REQUEST_CHUNKS) {
                    let peer = peers.get_mut(node).unwrap();
                    for index in group {
                        peer.in_flight.insert(*index, Instant::now());
                    }
                    let _ = connected[node].send(MeshMessage::GetChunks { file_id: id.clone(), indices: group.to_vec() });
                }
            }
        }

        // Tell the other nodes what we have so far, so they can fetch from us too.
        let have = blob.bitmap();
        if have.count() != last_map.1 && last_map.0.elapsed() >= tuning.map_interval {
            last_map = (Instant::now(), have.count());
            for sender in connected.values() {
                let _ = sender.send(MeshMessage::ChunkMap { file_id: id.clone(), chunks: have.len(), bitmap: have.to_hex() });
            }
        }

        // Nobody is waiting for this and nothing has moved for a long time.
        if download.readers.lock().unwrap().is_empty() && last_progress.elapsed() > tuning.idle_exit {
            tracing::info!("Transfer: giving up on {id} for now (no progress)");
            return finish(&state, &id, &download);
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
    use crate::mesh::{dispatch, MeshPeerHandle};
    use crate::store::manifest_root;
    use crate::types::Holder;

    // ── Scheduling ───────────────────────────────────────────────────────

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
    fn chunk_frames_round_trip() {
        let frame = encode_chunk_frame("file_abc", 70_000, b"payload");
        assert_eq!(decode_chunk_frame(&frame), Some(("file_abc", 70_000, &b"payload"[..])));
        assert_eq!(decode_chunk_frame(&encode_chunk_frame("f", 0, b"")), Some(("f", 0, &b""[..])));
        for bad in [&[][..], &[2, 1, b'a', 0, 0, 0, 0][..], &[1, 9, b'a'][..], &[1, 1, 0xff, 0, 0, 0, 0][..], &[1][..]] {
            assert_eq!(decode_chunk_frame(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn chunks_just_ahead_of_a_reader_come_first_then_rarest_first() {
        let sources = [source("a", None, 100, 0, 0.0), source("b", Some(&[50, 51, 52]), 100, 0, 0.0)];
        let order = priority_order(&have(100, &[]), &[40], &sources);
        assert_eq!(&order[..16], (40..56).collect::<Vec<u32>>().as_slice(), "the reader's window first");
        // Of the rest, chunks only `a` has (1 copy) precede those both have (2 copies); 50..=52 are in the window.
        assert_eq!(order[16], 0);
        assert_eq!(order.len(), 100);
    }

    #[test]
    fn rare_chunks_beat_common_ones_when_nobody_is_reading() {
        let sources = [source("a", Some(&[0, 1, 2, 3]), 4, 0, 0.0), source("b", Some(&[0, 1, 2]), 4, 0, 0.0), source("c", Some(&[0, 1]), 4, 0, 0.0)];
        // chunk 3: one copy, chunk 2: two, chunks 0 and 1: three.
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

        // In the endgame a second source is asked too, but never the one already asked.
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

    // ── End to end between in-process nodes ──────────────────────────────

    type Tamper = Arc<dyn Fn(&mut Vec<u8>) -> bool + Send + Sync>;

    fn data_pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| ((i * 7) as u8).wrapping_add(seed).wrapping_add((i >> 16) as u8)).collect()
    }

    // One direction of a connection: what `from` sends to `to`. Control
    // messages go through the real `dispatch`, chunk frames through the real
    // frame handler. `tamper` may change a frame or drop it (by returning false).
    async fn connect_one_way(from: &NodeState, to: &NodeState, tamper: Option<Tamper>) {
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<MeshMessage>();
        let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(8);
        from.mesh_peers.write().await.insert(
            to.node_id.clone(),
            MeshPeerHandle {
                node_id: to.node_id.clone(),
                node_name: to.node_name.clone(),
                addr: "127.0.0.1:1".parse().unwrap(),
                http_port: 1,
                sender: control_tx,
                data: data_tx,
                serve_slots: Arc::new(tokio::sync::Semaphore::new(SERVE_SLOTS)),
                last_seen: Instant::now(),
                rtt_ms: None,
            },
        );
        let (receiver, from_id) = (to.clone(), from.node_id.clone());
        tokio::spawn({
            let (receiver, from_id) = (receiver.clone(), from_id.clone());
            async move {
                while let Some(message) = control_rx.recv().await {
                    dispatch(&message, &from_id, &receiver).await;
                }
            }
        });
        tokio::spawn(async move {
            while let Some(mut frame) = data_rx.recv().await {
                if tamper.as_ref().is_some_and(|t| !t(&mut frame)) {
                    continue;
                }
                on_binary_frame(&receiver, &from_id, &frame);
            }
        });
    }

    // Connects two nodes as mesh peers without sockets; `tamper_a_to_b` affects chunk frames a sends to b.
    async fn link(a: &NodeState, b: &NodeState, tamper_a_to_b: Option<Tamper>) {
        connect_one_way(a, b, tamper_a_to_b).await;
        connect_one_way(b, a, None).await;
    }

    // Puts a finished file on `node` and lists it in every given node's catalog.
    async fn share(node: &NodeState, others: &[&NodeState], id: &str, content: &[u8]) -> FileMetadata {
        let blob = node.store.create(id, content.len() as u64).unwrap();
        for index in 0..blob.chunk_count() {
            let start = index as usize * CHUNK_SIZE as usize;
            let end = (start + CHUNK_SIZE as usize).min(content.len());
            blob.write_chunk_hashing(index, content[start..end].to_vec()).await.unwrap();
        }
        let root = blob.seal().unwrap();
        let stamp = node.clock.now();
        let entry = FileMetadata {
            id: id.into(),
            name: format!("{id}.bin"),
            size: content.len() as u64,
            mime_type: "application/octet-stream".into(),
            uploader_id: "peer_a".into(),
            uploader_node: node.node_id.clone(),
            holders: [(node.node_id.clone(), Holder { since: stamp.clone(), present: true })].into(),
            uploaded_at: chrono::Utc::now(),
            created_at: stamp.wall,
            version: stamp,
            deleted: false,
            deleted_at: 0,
            manifest_root: Some(root),
            is_folder: false,
            parent: None,
            folder_bytes: 0,
            folder_files: 0,
        };
        blob.set_entry(entry.clone());
        for n in std::iter::once(node).chain(others.iter().copied()) {
            n.files.write().await.insert(id.into(), entry.clone());
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

    // A node lists itself as a holder just after the file completes, not at the same instant.
    async fn lists_itself_as_holder(node: &NodeState, id: &str) -> bool {
        for _ in 0..100 {
            if node.files.read().await.get(id).is_some_and(|f| f.is_held_by(&node.node_id)) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    fn nodes<const N: usize>(names: [&str; N]) -> [NodeState; N] {
        names.map(|n| NodeState::for_tests_node(n, None))
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
        // B now lists itself as a holder, so others can fetch from it.
        assert!(lists_itself_as_holder(&b, "file_x").await);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(b.transfers.active(), 0);
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

        // Listed, but the only holder isn't connected.
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
            Arc::new(move |_| {
                n.fetch_add(1, Ordering::Relaxed);
                true
            })
        };
        link(&a, &b, Some(counter(from_a.clone()))).await;
        link(&c, &b, Some(counter(from_c.clone()))).await;
        let content = data_pattern(24 * CHUNK_SIZE as usize, 3);
        share(&a, &[&b, &c], "file_x", &content).await;
        share(&c, &[&a, &b], "file_x", &content).await;
        // Both hold it: merge the two catalog entries' holders as the mesh would.
        for n in [&a, &b, &c] {
            let mut files = n.files.write().await;
            let entry = files.get_mut("file_x").unwrap();
            entry.set_holder("node_a", true, a.clock.now());
            entry.set_holder("node_c", true, c.clock.now());
        }

        let blob = ensure_file(&b, "file_x").await.unwrap();
        assert!(wait_complete(&blob, 15).await);
        assert_eq!(read_all(&blob).await, content);
        let (n_a, n_c) = (from_a.load(Ordering::Relaxed), from_c.load(Ordering::Relaxed));
        assert!(n_a > 0 && n_c > 0, "both sources should have served chunks (a: {n_a}, c: {n_c})");
        // Each chunk is fetched once, plus a few duplicates in the endgame.
        assert!((24..=24 + ENDGAME_CHUNKS as usize).contains(&(n_a + n_c)), "frames: {}", n_a + n_c);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_node_that_sends_corrupted_data_is_dropped_and_the_file_still_completes() {
        let [a, evil, b] = nodes(["node_a", "node_evil", "node_b"]);
        link(&a, &b, None).await;
        let corrupted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let flip: Tamper = {
            let corrupted = corrupted.clone();
            Arc::new(move |frame| {
                let last = frame.len() - 1;
                frame[last] ^= 0xff; // corrupt the payload, keep the header valid
                corrupted.fetch_add(1, Ordering::Relaxed);
                true
            })
        };
        link(&evil, &b, Some(flip)).await;
        let content = data_pattern(12 * CHUNK_SIZE as usize, 4);
        share(&a, &[&b, &evil], "file_x", &content).await;
        share(&evil, &[&a, &b], "file_x", &content).await;
        for n in [&a, &b, &evil] {
            let mut files = n.files.write().await;
            let entry = files.get_mut("file_x").unwrap();
            entry.set_holder("node_a", true, a.clock.now());
            entry.set_holder("node_evil", true, evil.clock.now());
        }

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
        for n in [&a, &b, &dead] {
            let mut files = n.files.write().await;
            let entry = files.get_mut("file_x").unwrap();
            entry.set_holder("node_a", true, a.clock.now());
            entry.set_holder("node_dead", true, dead.clock.now());
        }

        let blob = ensure_file(&b, "file_x").await.unwrap();
        assert!(wait_complete(&blob, 20).await, "the download should recover from a silent source");
        assert_eq!(read_all(&blob).await, content);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_manifest_that_does_not_match_the_catalog_is_refused() {
        let [a, b] = nodes(["node_a", "node_b"]);
        link(&a, &b, None).await;
        let content = data_pattern(3 * CHUNK_SIZE as usize, 6);
        share(&a, &[&b], "file_x", &content).await;
        // The catalog B holds promises a different file than A actually has.
        b.files.write().await.get_mut("file_x").unwrap().manifest_root = Some(manifest_root(content.len() as u64, &[[7; 32]; 3]));

        let blob = ensure_file(&b, "file_x").await.unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(!blob.is_complete());
        assert!(!blob.has_manifest(), "a manifest that doesn't match the root must not be accepted");
        assert_eq!(blob.bitmap().count(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsharing_stops_the_download_and_deletes_the_partial_file() {
        let [a, b] = nodes(["node_a", "node_b"]);
        let slow: Tamper = Arc::new(|_| false); // nothing arrives: the download just waits
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
        // c can see a, but nothing a sends to c ever arrives; b can reach a.
        // c can only complete by fetching from b, which becomes a holder (and
        // announces what it has) as it downloads.
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
        assert!(lists_itself_as_holder(&c, "file_x").await);
    }
}
