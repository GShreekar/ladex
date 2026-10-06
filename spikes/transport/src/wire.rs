//! Requests for the quinn path, a cut-down version of the NEXT_PHASE §5.2 data streams.

use anyhow::bail;

pub const ALPN: &[u8] = b"ladex-spike/1";

/// Largest manifest we accept: 32 bytes per chunk of a 1 TiB file.
pub const MAX_MANIFEST_LEN: usize = 16 + 32 * (1 << 20);

const GET_MANIFEST: u8 = 0;
const GET_CHUNKS: u8 = 1;

/// One request per bidirectional stream; the sender answers with raw bytes and finishes the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    GetManifest,
    GetChunks { start: u32, count: u32 },
}

pub const REQUEST_LEN: usize = 1 + 4 + 4;

impl Request {
    pub fn encode(self) -> [u8; REQUEST_LEN] {
        let mut bytes = [0u8; REQUEST_LEN];
        match self {
            Request::GetManifest => bytes[0] = GET_MANIFEST,
            Request::GetChunks { start, count } => {
                bytes[0] = GET_CHUNKS;
                bytes[1..5].copy_from_slice(&start.to_le_bytes());
                bytes[5..9].copy_from_slice(&count.to_le_bytes());
            }
        }
        bytes
    }

    pub fn decode(bytes: [u8; REQUEST_LEN]) -> anyhow::Result<Self> {
        let start = u32::from_le_bytes(bytes[1..5].try_into()?);
        let count = u32::from_le_bytes(bytes[5..9].try_into()?);
        match bytes[0] {
            GET_MANIFEST => Ok(Request::GetManifest),
            GET_CHUNKS => Ok(Request::GetChunks { start, count }),
            kind => bail!("unknown request kind {kind}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_chunks_survives_an_encode_decode_round_trip() {
        let request = Request::GetChunks { start: 4090, count: 4 };
        assert_eq!(Request::decode(request.encode()).unwrap(), request);
    }

    #[test]
    fn decode_rejects_an_unknown_request_kind() {
        assert!(Request::decode([9; REQUEST_LEN]).is_err());
    }
}
