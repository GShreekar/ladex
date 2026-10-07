//! Whole frames on a QUIC stream: a 4-byte big-endian length, then that many bytes.

use std::io;

use quinn::{ReadExactError, RecvStream, SendStream};

use crate::handshake;

const LENGTH_BYTES: usize = 4;

pub(super) struct FrameSender {
    stream: SendStream,
    max_frame: usize,
}

pub(super) struct FrameReceiver {
    stream: RecvStream,
    max_frame: usize,
}

/// Both halves of one stream, as the handshake needs them.
pub(super) struct FramedStream {
    pub sender: FrameSender,
    pub receiver: FrameReceiver,
}

impl FramedStream {
    pub fn new(send: SendStream, recv: RecvStream, max_frame: usize) -> Self {
        Self { sender: FrameSender { stream: send, max_frame }, receiver: FrameReceiver { stream: recv, max_frame } }
    }

    /// Raises the frame limit once the peer is authenticated.
    pub fn allow_frames_up_to(&mut self, max_frame: usize) {
        self.sender.max_frame = max_frame;
        self.receiver.max_frame = max_frame;
    }
}

impl FrameSender {
    pub async fn send(&mut self, payload: &[u8]) -> io::Result<()> {
        if payload.len() > self.max_frame {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("a {}-byte frame is over the limit", payload.len())));
        }
        let length = u32::try_from(payload.len()).expect("the frame limit fits in u32").to_be_bytes();
        self.stream.write_all(&length).await.map_err(io::Error::from)?;
        self.stream.write_all(payload).await.map_err(io::Error::from)
    }

    /// Tells the peer no more frames follow; frames already sent are still delivered.
    pub fn finish(&mut self) {
        let _ = self.stream.finish();
    }
}

impl FrameReceiver {
    /// `Ok(None)` when the peer finished the stream between frames.
    pub async fn recv(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut length = [0u8; LENGTH_BYTES];
        match self.stream.read_exact(&mut length).await {
            Ok(()) => {}
            Err(ReadExactError::FinishedEarly(0)) => return Ok(None),
            Err(e) => return Err(read_error(e)),
        }
        let length = u32::from_be_bytes(length) as usize;
        if length > self.max_frame {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("the peer sent a {length}-byte frame, over the limit")));
        }
        let mut payload = vec![0u8; length];
        self.stream.read_exact(&mut payload).await.map_err(read_error)?;
        Ok(Some(payload))
    }
}

fn read_error(error: ReadExactError) -> io::Error {
    match error {
        ReadExactError::FinishedEarly(_) => io::Error::new(io::ErrorKind::UnexpectedEof, "the stream ended inside a frame"),
        ReadExactError::ReadError(e) => e.into(),
    }
}

impl handshake::Transport for FramedStream {
    async fn send(&mut self, frame: Vec<u8>) -> io::Result<()> {
        self.sender.send(&frame).await
    }

    async fn recv(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.receiver.recv().await
    }
}
