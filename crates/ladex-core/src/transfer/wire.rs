//! The chunk data plane: one short-lived stream per request, so a slow request never holds up control messages or others.
//!
//! A frame is a 4-byte big-endian length and then the body. Requests are postcard; replies are framed by hand so a
//! 1 MiB chunk is read straight into its buffer and never re-encoded.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::mpsc;
use tokio::time::{Instant, Sleep};

use crate::store::CHUNK_SIZE;

const LENGTH_BYTES: usize = 4;
const MAX_REQUEST_BYTES: usize = 4 * 1024;
const KIND_CHUNK: u8 = 0;
const KIND_MISSING: u8 = 1;
const KIND_ERROR: u8 = 2;
const CHUNK_HEADER_BYTES: usize = 1 + 4;
const MISSING_BYTES: usize = 1 + 4;
const ERROR_BYTES: usize = 1 + 1;
const MAX_REPLY_BYTES: usize = CHUNK_HEADER_BYTES + CHUNK_SIZE as usize;

/// The most ranges one request may carry.
pub const MAX_RANGES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRange {
    pub start: u32,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetChunks {
    pub file_id: String,
    pub ranges: Vec<ChunkRange>,
}

impl GetChunks {
    /// Asks for these chunks, merging neighbours into one range.
    pub fn for_indices(file_id: &str, indices: &[u32]) -> Self {
        let mut sorted = indices.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let mut ranges: Vec<ChunkRange> = Vec::new();
        for index in sorted {
            match ranges.last_mut() {
                Some(range) if range.start + range.count == index => range.count += 1,
                _ => ranges.push(ChunkRange { start: index, count: 1 }),
            }
        }
        Self { file_id: file_id.to_string(), ranges }
    }

    /// The chunks asked for, in order; None if the request is empty, oversized or has a range that overflows.
    pub fn indices(&self, max_chunks: usize) -> Option<Vec<u32>> {
        if self.ranges.is_empty() || self.ranges.len() > MAX_RANGES {
            return None;
        }
        let mut indices = Vec::new();
        for range in &self.ranges {
            let end = range.start.checked_add(range.count)?;
            if range.count == 0 || indices.len() + range.count as usize > max_chunks {
                return None;
            }
            indices.extend(range.start..end);
        }
        Some(indices)
    }
}

/// Why a whole request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    NotFound,
    /// All upload slots stayed taken for too long; asking again later may work.
    Busy,
    /// The request could not be read or asks for too much.
    Malformed,
}

impl ErrorCode {
    fn to_byte(self) -> u8 {
        match self {
            ErrorCode::NotFound => 1,
            ErrorCode::Busy => 2,
            ErrorCode::Malformed => 3,
        }
    }

    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(ErrorCode::NotFound),
            2 => Some(ErrorCode::Busy),
            3 => Some(ErrorCode::Malformed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    ChunkData {
        index: u32,
        bytes: Vec<u8>,
    },
    /// This chunk is not held here, or failed its check when read back.
    Missing {
        index: u32,
    },
    Error(ErrorCode),
}

/// A request for chunks, handed to the connection that owns the peer's QUIC streams; its answers come back on `replies`.
#[derive(Debug)]
pub struct Fetch {
    pub request: GetChunks,
    pub replies: mpsc::Sender<Reply>,
    /// How long the stream may deliver no bytes before it is given up.
    pub stall_timeout: Duration,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

async fn read_length<R: AsyncRead + Unpin>(reader: &mut R, max: usize) -> io::Result<Option<usize>> {
    let mut length = [0u8; LENGTH_BYTES];
    if reader.read(&mut length[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut length[1..]).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length > max {
        return Err(invalid("a frame is over the size limit"));
    }
    Ok(Some(length))
}

fn length_prefix(length: usize) -> [u8; LENGTH_BYTES] {
    u32::try_from(length).expect("frame sizes are bounded well below u32::MAX").to_be_bytes()
}

pub async fn write_request<W: AsyncWrite + Unpin>(writer: &mut W, request: &GetChunks) -> io::Result<()> {
    let body = postcard::to_stdvec(request).expect("chunk requests always serialize");
    if body.len() > MAX_REQUEST_BYTES {
        return Err(invalid("the request is over the size limit"));
    }
    writer.write_all(&length_prefix(body.len())).await?;
    writer.write_all(&body).await
}

pub async fn read_request<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<GetChunks> {
    let length = read_length(reader, MAX_REQUEST_BYTES).await?.ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    let (request, rest) = postcard::take_from_bytes(&body).map_err(|_| invalid("the request could not be read"))?;
    if !rest.is_empty() {
        return Err(invalid("the request has bytes after it"));
    }
    Ok(request)
}

pub async fn write_reply<W: AsyncWrite + Unpin>(writer: &mut W, reply: &Reply) -> io::Result<()> {
    match reply {
        Reply::ChunkData { index, bytes } => {
            let mut header = [0u8; LENGTH_BYTES + CHUNK_HEADER_BYTES];
            header[..LENGTH_BYTES].copy_from_slice(&length_prefix(CHUNK_HEADER_BYTES + bytes.len()));
            header[LENGTH_BYTES] = KIND_CHUNK;
            header[LENGTH_BYTES + 1..].copy_from_slice(&index.to_be_bytes());
            writer.write_all(&header).await?;
            writer.write_all(bytes).await
        }
        Reply::Missing { index } => {
            let mut frame = Vec::with_capacity(LENGTH_BYTES + MISSING_BYTES);
            frame.extend_from_slice(&length_prefix(MISSING_BYTES));
            frame.push(KIND_MISSING);
            frame.extend_from_slice(&index.to_be_bytes());
            writer.write_all(&frame).await
        }
        Reply::Error(code) => {
            let mut frame = Vec::with_capacity(LENGTH_BYTES + ERROR_BYTES);
            frame.extend_from_slice(&length_prefix(ERROR_BYTES));
            frame.extend_from_slice(&[KIND_ERROR, code.to_byte()]);
            writer.write_all(&frame).await
        }
    }
}

/// The next reply, or None once the serving node finished the stream between replies.
pub async fn read_reply<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Reply>> {
    let Some(length) = read_length(reader, MAX_REPLY_BYTES).await? else { return Ok(None) };
    let mut kind = [0u8; 1];
    if length == 0 {
        return Err(invalid("an empty reply"));
    }
    reader.read_exact(&mut kind).await?;
    match kind[0] {
        KIND_CHUNK if length >= CHUNK_HEADER_BYTES => {
            let index = read_index(reader).await?;
            let mut bytes = vec![0u8; length - CHUNK_HEADER_BYTES];
            reader.read_exact(&mut bytes).await?;
            Ok(Some(Reply::ChunkData { index, bytes }))
        }
        KIND_MISSING if length == MISSING_BYTES => Ok(Some(Reply::Missing { index: read_index(reader).await? })),
        KIND_ERROR if length == ERROR_BYTES => {
            let mut code = [0u8; 1];
            reader.read_exact(&mut code).await?;
            let code = ErrorCode::from_byte(code[0]).ok_or_else(|| invalid("an unknown error code"))?;
            Ok(Some(Reply::Error(code)))
        }
        _ => Err(invalid("an unreadable reply")),
    }
}

async fn read_index<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<u32> {
    let mut index = [0u8; 4];
    reader.read_exact(&mut index).await?;
    Ok(u32::from_be_bytes(index))
}

/// Fails a read with `TimedOut` once the stream has delivered no bytes for `timeout`.
///
/// A chunk is 1 MiB, so a slow link may take a long time to deliver one; what matters is that bytes keep arriving.
struct StallGuard<R> {
    inner: R,
    timeout: Duration,
    deadline: Pin<Box<Sleep>>,
    // The deadline runs only while a read is waiting, not while the reader is busy elsewhere.
    waiting: bool,
}

impl<R> StallGuard<R> {
    fn new(inner: R, timeout: Duration) -> Self {
        Self { inner, timeout, deadline: Box::pin(tokio::time::sleep(timeout)), waiting: false }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for StallGuard<R> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Poll::Ready(result) = Pin::new(&mut this.inner).poll_read(cx, buf) {
            this.waiting = false;
            return Poll::Ready(result);
        }
        if !this.waiting {
            this.deadline.as_mut().reset(Instant::now() + this.timeout);
            this.waiting = true;
        }
        match this.deadline.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "the stream stalled"))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Asks over one stream and passes each reply on until the serving node finishes, stalls or the receiver is gone.
pub async fn fetch_stream<R, W>(
    reader: R,
    mut writer: W,
    request: &GetChunks,
    replies: mpsc::Sender<Reply>,
    stall_timeout: Duration,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = StallGuard::new(reader, stall_timeout);
    write_request(&mut writer, request).await?;
    writer.shutdown().await?;
    // Once the receiver is gone, returning drops the stream, which tells the serving node to stop.
    loop {
        let reply = tokio::select! {
            reply = read_reply(&mut reader) => reply?,
            () = replies.closed() => return Ok(()),
        };
        let Some(reply) = reply else { return Ok(()) };
        if replies.send(reply).await.is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(file_id: &str, ranges: &[(u32, u32)]) -> GetChunks {
        GetChunks { file_id: file_id.into(), ranges: ranges.iter().map(|&(start, count)| ChunkRange { start, count }).collect() }
    }

    async fn reply_round_trip(reply: Reply) -> Option<Reply> {
        let mut wire = Vec::new();
        write_reply(&mut wire, &reply).await.unwrap();
        read_reply(&mut wire.as_slice()).await.unwrap()
    }

    #[test]
    fn neighbouring_chunks_are_merged_into_one_range() {
        assert_eq!(GetChunks::for_indices("f", &[5, 3, 4, 9, 4]), request("f", &[(3, 3), (9, 1)]));
    }

    #[test]
    fn a_request_lists_its_chunks_in_order() {
        assert_eq!(request("f", &[(3, 2), (9, 1)]).indices(16), Some(vec![3, 4, 9]));
    }

    #[test]
    fn a_request_for_nothing_is_malformed() {
        assert_eq!(request("f", &[]).indices(16), None);
        assert_eq!(request("f", &[(1, 0)]).indices(16), None);
    }

    #[test]
    fn a_request_for_more_chunks_than_allowed_is_malformed() {
        assert_eq!(request("f", &[(0, 17)]).indices(16), None);
        assert_eq!(request("f", &[(0, 10), (100, 10)]).indices(16), None);
    }

    #[test]
    fn a_range_running_past_the_last_index_is_malformed() {
        assert_eq!(request("f", &[(u32::MAX, 2)]).indices(16), None);
    }

    #[test]
    fn a_request_with_too_many_ranges_is_malformed() {
        let ranges: Vec<(u32, u32)> = (0..MAX_RANGES as u32 + 1).map(|i| (i * 10, 1)).collect();
        assert_eq!(request("f", &ranges).indices(100), None);
    }

    #[tokio::test]
    async fn a_request_survives_the_wire() {
        let asked = request("file_1", &[(0, 4), (40, 2)]);
        let mut wire = Vec::new();
        write_request(&mut wire, &asked).await.unwrap();
        assert_eq!(read_request(&mut wire.as_slice()).await.unwrap(), asked);
    }

    #[tokio::test]
    async fn a_request_with_bytes_after_it_is_refused() {
        let mut wire = Vec::new();
        write_request(&mut wire, &request("f", &[(0, 1)])).await.unwrap();
        wire.push(0);
        let longer = u32::from_be_bytes(wire[..4].try_into().unwrap()) + 1;
        wire[..4].copy_from_slice(&longer.to_be_bytes());
        assert!(read_request(&mut wire.as_slice()).await.is_err());
    }

    #[tokio::test]
    async fn an_oversized_request_is_refused_before_it_is_read() {
        let wire = (MAX_REQUEST_BYTES as u32 + 1).to_be_bytes();
        assert!(read_request(&mut wire.as_slice()).await.is_err());
    }

    #[tokio::test]
    async fn chunk_data_survives_the_wire() {
        let reply = Reply::ChunkData { index: 70_000, bytes: vec![7; 1000] };
        assert_eq!(reply_round_trip(reply.clone()).await, Some(reply));
    }

    #[tokio::test]
    async fn an_empty_chunk_survives_the_wire() {
        let reply = Reply::ChunkData { index: 0, bytes: Vec::new() };
        assert_eq!(reply_round_trip(reply.clone()).await, Some(reply));
    }

    #[tokio::test]
    async fn a_missing_chunk_survives_the_wire() {
        assert_eq!(reply_round_trip(Reply::Missing { index: 9 }).await, Some(Reply::Missing { index: 9 }));
    }

    #[tokio::test]
    async fn every_error_code_survives_the_wire() {
        for code in [ErrorCode::NotFound, ErrorCode::Busy, ErrorCode::Malformed] {
            assert_eq!(reply_round_trip(Reply::Error(code)).await, Some(Reply::Error(code)));
        }
    }

    #[tokio::test]
    async fn a_finished_stream_reads_as_no_more_replies() {
        assert_eq!(read_reply(&mut [].as_slice()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_reply_cut_off_mid_chunk_is_an_error() {
        let mut wire = Vec::new();
        write_reply(&mut wire, &Reply::ChunkData { index: 1, bytes: vec![1; 100] }).await.unwrap();
        wire.truncate(wire.len() - 1);
        assert!(read_reply(&mut wire.as_slice()).await.is_err());
    }

    #[tokio::test]
    async fn a_reply_over_the_size_limit_is_refused_before_it_is_read() {
        let wire = (MAX_REPLY_BYTES as u32 + 1).to_be_bytes();
        assert!(read_reply(&mut wire.as_slice()).await.is_err());
    }

    #[tokio::test]
    async fn a_reply_of_an_unknown_kind_is_refused() {
        let wire = [0, 0, 0, 2, 9, 0];
        assert!(read_reply(&mut wire.as_slice()).await.is_err());
    }

    #[tokio::test]
    async fn an_error_reply_with_an_unknown_code_is_refused() {
        let wire = [0, 0, 0, 2, KIND_ERROR, 200];
        assert!(read_reply(&mut wire.as_slice()).await.is_err());
    }

    #[tokio::test]
    async fn a_missing_reply_of_the_wrong_length_is_refused() {
        let wire = [0, 0, 0, 3, KIND_MISSING, 0, 0];
        assert!(read_reply(&mut wire.as_slice()).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_that_delivers_nothing_is_cut_off() {
        let (_server_end, client_end) = tokio::io::duplex(1024);
        let (reader, writer) = tokio::io::split(client_end);
        let (tx, _rx) = mpsc::channel(1);

        let fetched = fetch_stream(reader, writer, &request("f", &[(0, 1)]), tx, Duration::from_secs(5)).await;

        assert_eq!(fetched.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stream_that_keeps_delivering_slowly_is_not_cut_off() {
        let (server_end, client_end) = tokio::io::duplex(1024);
        let (reader, writer) = tokio::io::split(client_end);
        let (tx, mut rx) = mpsc::channel(4);
        let mut server = server_end;
        let serving = async {
            let mut frame = Vec::new();
            write_reply(&mut frame, &Reply::ChunkData { index: 0, bytes: vec![9; 100] }).await.unwrap();
            for piece in frame.chunks(30) {
                tokio::time::sleep(Duration::from_secs(4)).await;
                server.write_all(piece).await.unwrap();
            }
            server.shutdown().await.unwrap();
        };

        let asked = request("f", &[(0, 1)]);
        let (fetched, ()) = tokio::join!(fetch_stream(reader, writer, &asked, tx, Duration::from_secs(5)), serving);

        fetched.unwrap();
        assert_eq!(rx.recv().await, Some(Reply::ChunkData { index: 0, bytes: vec![9; 100] }));
    }

    #[tokio::test]
    async fn fetching_stops_as_soon_as_the_receiver_is_gone() {
        let (server_end, client_end) = tokio::io::duplex(1024);
        let (reader, writer) = tokio::io::split(client_end);
        let (tx, rx) = mpsc::channel(1);
        let fetching =
            tokio::spawn(async move { fetch_stream(reader, writer, &request("f", &[(0, 1)]), tx, Duration::from_secs(60)).await });

        drop(rx);

        assert!(fetching.await.unwrap().is_ok());
        drop(server_end);
    }

    #[tokio::test]
    async fn fetching_passes_every_reply_on_and_ends_with_the_stream() {
        let mut served = Vec::new();
        write_reply(&mut served, &Reply::ChunkData { index: 2, bytes: vec![1, 2, 3] }).await.unwrap();
        write_reply(&mut served, &Reply::Missing { index: 3 }).await.unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let mut sent = Vec::new();

        fetch_stream(served.as_slice(), &mut sent, &request("f", &[(2, 2)]), tx, Duration::from_secs(5)).await.unwrap();

        assert_eq!(read_request(&mut sent.as_slice()).await.unwrap(), request("f", &[(2, 2)]));
        assert_eq!(rx.recv().await, Some(Reply::ChunkData { index: 2, bytes: vec![1, 2, 3] }));
        assert_eq!(rx.recv().await, Some(Reply::Missing { index: 3 }));
        assert_eq!(rx.recv().await, None);
    }
}
