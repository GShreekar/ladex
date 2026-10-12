//! A file's content address: BLAKE3 hashes of its 1 MiB chunks, and the root derived from them.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use anyhow::{bail, ensure, Context};
use rayon::prelude::*;

pub const CHUNK_SIZE: u32 = 1 << 20;

const HASH_LEN: usize = 32;
const HEADER_LEN: usize = 8 + 4 + 4;
const ROOT_CONTEXT: &str = "ladex manifest root v1";
// Chunks read ahead of the hashers; bounds memory to a few batches whatever the file size.
const BATCH_CHUNKS: usize = 16;
const BATCHES_IN_FLIGHT: usize = 2;

pub type ChunkHash = [u8; HASH_LEN];

/// A file's size and chunk hashes; names and other metadata are left out so equal content gets an equal id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub size: u64,
    pub chunk_hashes: Vec<ChunkHash>,
}

impl Manifest {
    /// Streams the file once, hashing its chunks in parallel while the next ones are read.
    pub fn build(path: &Path) -> anyhow::Result<Self> {
        let mut file = File::open(path).with_context(|| format!("could not open {}", path.display()))?;
        let size = file.metadata()?.len();
        let total_chunks = chunk_count(size)?;

        let (sender, receiver) = mpsc::sync_channel(BATCHES_IN_FLIGHT);
        thread::scope(|scope| {
            scope.spawn(move || read_batches(&mut file, size, total_chunks, sender));
            let mut chunk_hashes = Vec::with_capacity(total_chunks as usize);
            for batch in receiver {
                let chunks = batch.context("could not read the file")?;
                chunk_hashes.par_extend(chunks.par_iter().map(|chunk| hash_chunk(chunk)));
            }
            ensure!(chunk_hashes.len() == total_chunks as usize, "the file changed while it was being hashed");
            Ok(Self { size, chunk_hashes })
        })
    }

    /// Identifies this exact content; the same bytes always give the same root.
    pub fn root(&self) -> String {
        root_of(self.size, &self.chunk_hashes)
    }

    pub fn chunk_count(&self) -> u32 {
        self.chunk_hashes.len() as u32
    }

    pub fn encode(&self) -> Vec<u8> {
        encode_parts(self.size, &self.chunk_hashes)
    }

    /// Parses a manifest received from another node, rejecting anything inconsistent.
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        ensure!(bytes.len() >= HEADER_LEN, "the manifest is truncated");
        let size = u64::from_le_bytes(bytes[0..8].try_into()?);
        let chunk_size = u32::from_le_bytes(bytes[8..12].try_into()?);
        let count = u32::from_le_bytes(bytes[12..16].try_into()?);
        if chunk_size != CHUNK_SIZE {
            bail!("the manifest uses {chunk_size}-byte chunks, expected {CHUNK_SIZE}");
        }
        ensure!(count == chunk_count(size)?, "the manifest lists {count} chunks for {size} bytes");
        let hash_bytes = &bytes[HEADER_LEN..];
        ensure!(hash_bytes.len() as u64 == count as u64 * HASH_LEN as u64, "the manifest's hash list is the wrong length");
        let chunk_hashes = hash_bytes.chunks_exact(HASH_LEN).map(|hash| hash.try_into().expect("chunks are HASH_LEN long")).collect();
        Ok(Self { size, chunk_hashes })
    }
}

/// The root of a file of `size` bytes whose chunks hash to `chunk_hashes`.
pub fn root_of(size: u64, chunk_hashes: &[ChunkHash]) -> String {
    let mut hasher = blake3::Hasher::new_derive_key(ROOT_CONTEXT);
    hasher.update(&encode_parts(size, chunk_hashes));
    hasher.finalize().to_hex().to_string()
}

pub fn hash_chunk(data: &[u8]) -> ChunkHash {
    *blake3::hash(data).as_bytes()
}

fn encode_parts(size: u64, chunk_hashes: &[ChunkHash]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_LEN + chunk_hashes.len() * HASH_LEN);
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&CHUNK_SIZE.to_le_bytes());
    bytes.extend_from_slice(&(chunk_hashes.len() as u32).to_le_bytes());
    for hash in chunk_hashes {
        bytes.extend_from_slice(hash);
    }
    bytes
}

fn chunk_count(size: u64) -> anyhow::Result<u32> {
    u32::try_from(size.div_ceil(CHUNK_SIZE as u64)).context("the file is too large to be split into chunks")
}

fn read_batches(file: &mut File, size: u64, total_chunks: u32, sender: mpsc::SyncSender<io::Result<Vec<Vec<u8>>>>) {
    let mut remaining = size;
    for _ in 0..total_chunks.div_ceil(BATCH_CHUNKS as u32) {
        let batch = read_batch(file, &mut remaining);
        let failed = batch.is_err();
        // A closed channel means the hashing side already gave up.
        if sender.send(batch).is_err() || failed {
            return;
        }
    }
}

fn read_batch(file: &mut File, remaining: &mut u64) -> io::Result<Vec<Vec<u8>>> {
    let mut chunks = Vec::with_capacity(BATCH_CHUNKS);
    while chunks.len() < BATCH_CHUNKS && *remaining > 0 {
        let mut chunk = vec![0u8; (*remaining).min(CHUNK_SIZE as u64) as usize];
        file.read_exact(&mut chunk)?;
        *remaining -= chunk.len() as u64;
        chunks.push(chunk);
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CHUNK: usize = CHUNK_SIZE as usize;

    fn file_with(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        file
    }

    fn patterned(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn hash_of(bytes: &[u8]) -> ChunkHash {
        *blake3::hash(bytes).as_bytes()
    }

    #[test]
    fn empty_file_has_no_chunks() {
        let manifest = Manifest::build(file_with(b"").path()).unwrap();
        assert_eq!(manifest, Manifest { size: 0, chunk_hashes: vec![] });
    }

    #[test]
    fn small_file_is_one_chunk() {
        let manifest = Manifest::build(file_with(b"hello").path()).unwrap();
        assert_eq!(manifest.chunk_hashes, vec![hash_of(b"hello")]);
    }

    #[test]
    fn last_chunk_holds_only_the_remainder() {
        let bytes = patterned(CHUNK + 7);
        let manifest = Manifest::build(file_with(&bytes).path()).unwrap();
        assert_eq!(manifest.chunk_hashes, vec![hash_of(&bytes[..CHUNK]), hash_of(&bytes[CHUNK..])]);
    }

    #[test]
    fn file_of_exact_chunk_multiple_has_no_empty_tail_chunk() {
        let manifest = Manifest::build(file_with(&patterned(2 * CHUNK)).path()).unwrap();
        assert_eq!(manifest.chunk_count(), 2);
    }

    #[test]
    fn hashes_stay_in_order_across_many_batches() {
        let bytes = patterned(BATCH_CHUNKS * 2 * CHUNK + 5);
        let manifest = Manifest::build(file_with(&bytes).path()).unwrap();
        let expected: Vec<ChunkHash> = bytes.chunks(CHUNK).map(hash_of).collect();
        assert_eq!(manifest.chunk_hashes, expected);
    }

    #[test]
    fn missing_file_is_an_error() {
        assert!(Manifest::build(Path::new("/nonexistent/ladex-test-file")).is_err());
    }

    #[test]
    fn same_content_gets_the_same_root() {
        let first = Manifest::build(file_with(&patterned(CHUNK + 1)).path()).unwrap();
        let second = Manifest::build(file_with(&patterned(CHUNK + 1)).path()).unwrap();
        assert_eq!(first.root(), second.root());
    }

    #[test]
    fn changing_one_byte_changes_the_root() {
        let mut bytes = patterned(CHUNK + 1);
        let before = Manifest::build(file_with(&bytes).path()).unwrap();
        bytes[CHUNK] ^= 1;
        let after = Manifest::build(file_with(&bytes).path()).unwrap();
        assert_ne!(before.root(), after.root());
    }

    #[test]
    fn the_root_is_64_lowercase_hex_characters() {
        let root = Manifest::build(file_with(b"hello").path()).unwrap().root();
        assert!(root.len() == 64 && root.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
    }

    #[test]
    fn the_root_depends_on_size_and_every_chunk() {
        let (a, b) = (hash_chunk(b"a"), hash_chunk(b"b"));
        let root = root_of(10, &[a, b]);
        assert_ne!(root, root_of(11, &[a, b]));
        assert_ne!(root, root_of(10, &[b, a]));
        assert_ne!(root, root_of(10, &[a]));
    }

    #[test]
    fn manifest_survives_an_encode_decode_round_trip() {
        let manifest = Manifest { size: CHUNK as u64 + 7, chunk_hashes: vec![hash_of(b"a"), hash_of(b"b")] };
        assert_eq!(Manifest::decode(&manifest.encode()).unwrap(), manifest);
    }

    #[test]
    fn decode_rejects_a_chunk_count_that_does_not_match_the_size() {
        let manifest = Manifest { size: 1, chunk_hashes: vec![hash_of(b"a"), hash_of(b"b")] };
        assert!(Manifest::decode(&manifest.encode()).is_err());
    }

    #[test]
    fn decode_rejects_a_truncated_hash_list() {
        let manifest = Manifest { size: 1, chunk_hashes: vec![hash_of(b"a")] };
        let encoded = manifest.encode();
        assert!(Manifest::decode(&encoded[..encoded.len() - 1]).is_err());
    }

    #[test]
    fn decode_rejects_a_different_chunk_size() {
        let mut encoded = Manifest { size: 0, chunk_hashes: vec![] }.encode();
        encoded[8..12].copy_from_slice(&(CHUNK_SIZE / 2).to_le_bytes());
        assert!(Manifest::decode(&encoded).is_err());
    }

    #[test]
    fn decode_rejects_a_size_too_large_for_the_chunk_count() {
        let mut encoded = Manifest { size: 0, chunk_hashes: vec![] }.encode();
        encoded[0..8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(Manifest::decode(&encoded).is_err());
    }
}
