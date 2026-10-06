use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, ensure};

/// Fixed chunk size from NEXT_PHASE §5.1.
pub const CHUNK_SIZE: u32 = 1 << 20;

const HASH_LEN: usize = 32;
const HEADER_LEN: usize = 8 + 4 + 4;

/// File size plus one BLAKE3 hash per 1 MiB chunk, as planned for the v3 transfer engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub size: u64,
    pub chunk_hashes: Vec<blake3::Hash>,
}

impl Manifest {
    pub fn build(path: &Path) -> anyhow::Result<Self> {
        let mut file = File::open(path)?;
        let size = file.metadata()?.len();
        let mut chunk_hashes = Vec::with_capacity(chunk_count(size) as usize);
        let mut chunk = vec![0u8; CHUNK_SIZE as usize];
        for index in 0..chunk_count(size) {
            let length = chunk_len(size, index);
            file.read_exact(&mut chunk[..length])?;
            chunk_hashes.push(blake3::hash(&chunk[..length]));
        }
        Ok(Self { size, chunk_hashes })
    }

    pub fn chunk_count(&self) -> u32 {
        self.chunk_hashes.len() as u32
    }

    pub fn chunk_len(&self, index: u32) -> usize {
        chunk_len(self.size, index)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_LEN + self.chunk_hashes.len() * HASH_LEN);
        bytes.extend_from_slice(&self.size.to_le_bytes());
        bytes.extend_from_slice(&CHUNK_SIZE.to_le_bytes());
        bytes.extend_from_slice(&self.chunk_count().to_le_bytes());
        for hash in &self.chunk_hashes {
            bytes.extend_from_slice(hash.as_bytes());
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        ensure!(bytes.len() >= HEADER_LEN, "the manifest is truncated");
        let size = u64::from_le_bytes(bytes[0..8].try_into()?);
        let chunk_size = u32::from_le_bytes(bytes[8..12].try_into()?);
        let count = u32::from_le_bytes(bytes[12..16].try_into()?);
        if chunk_size != CHUNK_SIZE {
            bail!("the sender uses {chunk_size}-byte chunks, expected {CHUNK_SIZE}");
        }
        ensure!(count == chunk_count(size), "the manifest lists {count} chunks for {size} bytes");
        let hash_bytes = &bytes[HEADER_LEN..];
        ensure!(hash_bytes.len() == count as usize * HASH_LEN, "the manifest's hash list is the wrong length");
        let chunk_hashes = hash_bytes.chunks_exact(HASH_LEN).map(blake3::Hash::from_slice).collect::<Result<_, _>>()?;
        Ok(Self { size, chunk_hashes })
    }
}

pub fn chunk_offset(index: u32) -> u64 {
    index as u64 * CHUNK_SIZE as u64
}

fn chunk_count(size: u64) -> u32 {
    size.div_ceil(CHUNK_SIZE as u64) as u32
}

fn chunk_len(size: u64, index: u32) -> usize {
    (size - chunk_offset(index)).min(CHUNK_SIZE as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_survives_an_encode_decode_round_trip() {
        let manifest = Manifest { size: CHUNK_SIZE as u64 + 7, chunk_hashes: vec![blake3::hash(b"a"), blake3::hash(b"b")] };
        assert_eq!(Manifest::decode(&manifest.encode()).unwrap(), manifest);
    }

    #[test]
    fn last_chunk_holds_only_the_remainder() {
        assert_eq!(chunk_len(CHUNK_SIZE as u64 + 7, 1), 7);
    }

    #[test]
    fn decode_rejects_a_chunk_count_that_does_not_match_the_size() {
        let manifest = Manifest { size: 1, chunk_hashes: vec![blake3::hash(b"a"), blake3::hash(b"b")] };
        assert!(Manifest::decode(&manifest.encode()).is_err());
    }
}
