//! IPC framing utilities.
//!
//! Messages are sent as length-prefixed frames over a Unix SOCK_SEQPACKET or
//! SOCK_STREAM socket. Each frame is:
//!
//!   [ 4-byte big-endian length ][ protobuf-encoded Envelope bytes ]
//!
//! We use `tokio_util::codec::LengthDelimitedCodec` for framing and `prost`
//! for serialization.

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use prost::Message;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::proto::Envelope;

/// A framed Unix stream that sends/receives length-delimited protobuf envelopes.
pub type EnvelopeFramed = Framed<UnixStream, LengthDelimitedCodec>;

/// Wrap a `UnixStream` with the length-delimited codec.
pub fn frame_stream(stream: UnixStream) -> EnvelopeFramed {
    let codec = LengthDelimitedCodec::builder()
        .max_frame_length(16 * 1024 * 1024) // 16 MiB max frame
        .new_codec();
    Framed::new(stream, codec)
}

/// Encode and send an `Envelope` over a framed stream.
pub async fn send_envelope(framed: &mut EnvelopeFramed, envelope: &Envelope) -> Result<()> {
    let mut buf = BytesMut::new();
    envelope
        .encode(&mut buf)
        .context("Failed to encode Envelope")?;
    framed
        .send(buf.freeze())
        .await
        .context("Failed to send Envelope frame")?;
    Ok(())
}

/// Receive the next `Envelope` from a framed stream.
/// Returns `None` if the stream is closed.
pub async fn recv_envelope(framed: &mut EnvelopeFramed) -> Result<Option<Envelope>> {
    match framed.next().await {
        None => Ok(None),
        Some(result) => {
            let bytes: Bytes = result.context("Frame receive error")?.freeze();
            let envelope =
                Envelope::decode(bytes).context("Failed to decode Envelope")?;
            Ok(Some(envelope))
        }
    }
}

/// Build an `Envelope` for a method call or event.
pub fn make_envelope(
    request_id: u64,
    source: impl Into<String>,
    target: impl Into<String>,
    method: impl Into<String>,
    payload: impl Message,
) -> Result<Envelope> {
    let mut buf = BytesMut::new();
    payload.encode(&mut buf).context("Failed to encode payload")?;
    Ok(Envelope {
        request_id,
        source: source.into(),
        target: target.into(),
        method: method.into(),
        payload: buf.freeze().to_vec(),
    })
}
