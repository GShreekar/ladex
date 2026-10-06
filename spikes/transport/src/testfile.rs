use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

use anyhow::{bail, Context};

const BLOCK_SIZE: usize = 4 << 20;

/// Parse sizes like `4GiB`, `512MiB` or a plain byte count.
pub fn parse_size(text: &str) -> anyhow::Result<u64> {
    let units = [("GiB", 1u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    for (suffix, multiplier) in units {
        if let Some(number) = text.strip_suffix(suffix) {
            return Ok(number.trim().parse::<u64>()? * multiplier);
        }
    }
    text.parse().with_context(|| format!("'{text}' is not a size like 4GiB or 1048576"))
}

/// Fill a file with BLAKE3 XOF output, which is fast and defeats any compression on the path.
pub fn generate(path: &Path, size: u64) -> anyhow::Result<()> {
    if size == 0 {
        bail!("the test file must not be empty");
    }
    let seed: [u8; 32] = rand::random();
    let mut stream = blake3::Hasher::new_keyed(&seed).finalize_xof();
    let mut writer = BufWriter::with_capacity(BLOCK_SIZE, File::create(path)?);
    let mut block = vec![0u8; BLOCK_SIZE];
    let mut remaining = size;
    while remaining > 0 {
        let length = remaining.min(BLOCK_SIZE as u64) as usize;
        stream.fill(&mut block[..length]);
        writer.write_all(&block[..length])?;
        remaining -= length as u64;
    }
    writer.into_inner()?.sync_all()?;
    Ok(())
}

pub fn hash_file(path: &Path) -> anyhow::Result<blake3::Hash> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut block = vec![0u8; BLOCK_SIZE];
    loop {
        let read = file.read(&mut block)?;
        if read == 0 {
            return Ok(hasher.finalize());
        }
        hasher.update(&block[..read]);
    }
}
