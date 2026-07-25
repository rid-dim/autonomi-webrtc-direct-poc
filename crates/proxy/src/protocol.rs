//! The single RPC spoken over the WebRTC DataChannel.
//!
//! Deliberately minimal — one request per stream, no multiplexing of our own,
//! no versioning handshake. libp2p's stream abstraction already handles SCTP
//! fragmentation and backpressure, so this layer only has to frame bytes.
//!
//! ```text
//! Request   (browser -> proxy):  32 bytes            the XorName to fetch
//! Response  (proxy -> browser):  0x00 | u32be | data OK, `data` is `len` bytes
//!                                0x01                NOT_FOUND
//!                                0x02                REFUSED (not on the allowlist)
//! ```
//!
//! The proxy cannot lie about `data`: the browser verifies it against the
//! requested address (BLAKE3) before using it. `NOT_FOUND` is the only
//! meaningful thing a hostile proxy can fabricate — it can withhold, not forge.

use futures::{AsyncReadExt, AsyncWriteExt};
use libp2p::StreamProtocol;

/// Protocol id negotiated on each stream.
pub const PROTOCOL: StreamProtocol = StreamProtocol::new("/autonomi-fetch/0.1.0");

/// Length of a content address, in bytes.
pub const ADDRESS_LEN: usize = 32;

pub const STATUS_OK: u8 = 0x00;
pub const STATUS_NOT_FOUND: u8 = 0x01;
pub const STATUS_REFUSED: u8 = 0x02;

/// Payload bytes per transport message.
///
/// `libp2p-webrtc` caps a single message at 16 KiB including protobuf framing;
/// staying under it keeps one `write_all` to one message.
pub const MAX_MESSAGE_PAYLOAD: usize = 16 * 1024 - 64;

/// Largest response we are willing to send or receive.
///
/// A chunk is at most `MAX_CHUNK_SIZE` (~4 MiB) plus compression headroom; the
/// ceiling here is generous enough for that and tight enough to bound memory.
pub const MAX_RESPONSE_LEN: u32 = 8 * 1024 * 1024;

/// Read a request address off a stream.
pub async fn read_request<S>(stream: &mut S) -> std::io::Result<[u8; ADDRESS_LEN]>
where
    S: AsyncReadExt + Unpin,
{
    let mut address = [0u8; ADDRESS_LEN];
    stream.read_exact(&mut address).await?;
    Ok(address)
}

/// Write a successful response, then close the stream.
pub async fn write_found<S>(stream: &mut S, data: &[u8]) -> std::io::Result<()>
where
    S: AsyncWriteExt + Unpin,
{
    let len = u32::try_from(data.len()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "chunk exceeds u32 length")
    })?;
    if len > MAX_RESPONSE_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "chunk exceeds maximum response length",
        ));
    }
    stream.write_all(&[STATUS_OK]).await?;
    stream.write_all(&len.to_be_bytes()).await?;

    // Written and flushed one transport message at a time.
    //
    // A plain `write_all` of the whole payload stalls forever as soon as the
    // response needs more than one message: `libp2p-webrtc`'s `Stream` buffers
    // into a `Framed` sink, and `webrtc-rs`'s `PollDataChannel::poll_write`
    // stores the resulting write future *without polling it*, leaving it to be
    // driven by some later poll that never comes. Measured precisely: a 15,020
    // byte response arrives in 7 ms, a 20,000 byte one never arrives at all.
    //
    // Flushing after each message forces that future to completion, so the
    // pipeline drains message by message.
    for piece in data.chunks(MAX_MESSAGE_PAYLOAD) {
        stream.write_all(piece).await?;
        stream.flush().await?;
    }

    stream.close().await
}

/// Read whatever is left until the client closes its side.
///
/// The client sends its 32-byte request and immediately half-closes, so this
/// returns as soon as that EOF is seen. Errors are ignored: a client that
/// vanishes mid-request is not the proxy's problem, and the only purpose here
/// is to avoid dropping a stream that never closed cleanly.
pub async fn drain_to_eof<S>(stream: &mut S)
where
    S: AsyncReadExt + Unpin,
{
    let mut scratch = [0u8; 64];
    loop {
        match stream.read(&mut scratch).await {
            Ok(0) | Err(_) => return,
            Ok(_) => continue,
        }
    }
}

/// Write a bodyless status response, then close the stream.
pub async fn write_status<S>(stream: &mut S, status: u8) -> std::io::Result<()>
where
    S: AsyncWriteExt + Unpin,
{
    stream.write_all(&[status]).await?;
    stream.close().await
}
