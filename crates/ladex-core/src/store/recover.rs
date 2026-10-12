//! Finds chunks a crash left on disk without recording them, so a restart continues from what is really there.

use std::fs::File;
use std::io;

use rayon::prelude::*;

use super::{chunk_len, hash_chunk, read_exact_at, ChunkHash, State, CHUNK_SIZE};

const NEVER_WRITTEN: ChunkHash = [0; 32];

/// Marks present every missing chunk whose bytes on disk match its stored hash; returns how many there were.
pub fn recover_chunks(data: &File, size: u64, state: &mut State) -> io::Result<u32> {
    let hashes = &state.hashes;
    let candidates: Vec<u32> = state.have.iter_missing().filter(|index| hashes[*index as usize] != NEVER_WRITTEN).collect();
    let checked = candidates
        .into_par_iter()
        .map_init(
            || vec![0u8; CHUNK_SIZE as usize],
            |buffer, index| Ok((index, is_intact(data, &mut buffer[..chunk_len(size, index)], index, &hashes[index as usize])?)),
        )
        .collect::<io::Result<Vec<(u32, bool)>>>()?;

    let mut recovered = 0;
    for (index, _) in checked.into_iter().filter(|(_, is_intact)| *is_intact) {
        state.have.set(index);
        state.hash_known.set(index);
        recovered += 1;
    }
    Ok(recovered)
}

fn is_intact(data: &File, chunk: &mut [u8], index: u32, expected: &ChunkHash) -> io::Result<bool> {
    read_exact_at(data, chunk, index as u64 * CHUNK_SIZE)?;
    Ok(hash_chunk(chunk) == *expected)
}
