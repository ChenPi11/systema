use std::os::unix::io::AsRawFd;

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
            let envelope = Envelope::decode(bytes).context("Failed to decode Envelope")?;
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
    payload
        .encode(&mut buf)
        .context("Failed to encode payload")?;
    Ok(Envelope {
        request_id,
        source: source.into(),
        target: target.into(),
        method: method.into(),
        payload: buf.freeze().to_vec(),
    })
}

// ---------------------------------------------------------------------------
// SCM_RIGHTS fd-passing helpers (dedicated connections only — not with Framed)
// ---------------------------------------------------------------------------

/// Send a single file descriptor over a Unix stream via SCM_RIGHTS.
/// The stream must be a dedicated raw connection (not wrapped in `Framed`).
pub async fn send_fd(stream: &UnixStream, fd: std::os::unix::io::RawFd) -> Result<()> {
    let raw = stream.as_raw_fd();
    tokio::task::spawn_blocking(move || {
        send_fd_sync(raw, fd)
    })
    .await
    .context("SCM_RIGHTS send task panicked")?
}

/// Receive a single file descriptor from a Unix stream via SCM_RIGHTS.
/// The stream must be a dedicated raw connection (not wrapped in `Framed`).
pub async fn recv_fd(stream: &UnixStream) -> Result<std::os::unix::io::RawFd> {
    let raw = stream.as_raw_fd();
    tokio::task::spawn_blocking(move || {
        recv_fd_sync(raw)
    })
    .await
    .context("SCM_RIGHTS recv task panicked")?
}

/// Synchronous SCM_RIGHTS send using libc::sendmsg.
fn send_fd_sync(sock_fd: std::os::unix::io::RawFd, fd: std::os::unix::io::RawFd) -> Result<()> {
    unsafe {
        let mut iov = libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        let mut cmsg_buf = [0u8; 24];
        let mut msghdr: libc::msghdr = std::mem::zeroed();

        msghdr.msg_iov = &mut iov;
        msghdr.msg_iovlen = 1;
        msghdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msghdr.msg_controllen = cmsg_buf.len();

        let cmsg = libc::CMSG_FIRSTHDR(&msghdr);
        if cmsg.is_null() {
            anyhow::bail!("CMSG_FIRSTHDR returned null");
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        let fd_size = std::mem::size_of::<libc::c_int>();
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_size as u32) as libc::size_t;
        std::ptr::write(libc::CMSG_DATA(cmsg) as *mut libc::c_int, fd);
        msghdr.msg_controllen = libc::CMSG_SPACE(fd_size as u32) as usize;

        let ret = libc::sendmsg(sock_fd, &msghdr, 0);
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            anyhow::bail!("sendmsg (SCM_RIGHTS) failed: {}", e);
        }
    }
    Ok(())
}

/// Synchronous SCM_RIGHTS receive using libc::recvmsg.
fn recv_fd_sync(sock_fd: std::os::unix::io::RawFd) -> Result<std::os::unix::io::RawFd> {
    unsafe {
        let mut data: u8 = 0;
        let mut iov = libc::iovec {
            iov_base: &mut data as *mut u8 as *mut libc::c_void,
            iov_len: 1,
        };
        let mut cmsg_buf = [0u8; 24];
        let mut msghdr: libc::msghdr = std::mem::zeroed();

        msghdr.msg_iov = &mut iov;
        msghdr.msg_iovlen = 1;
        msghdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msghdr.msg_controllen = cmsg_buf.len();

        let ret = libc::recvmsg(sock_fd, &mut msghdr, 0);
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            anyhow::bail!("recvmsg (SCM_RIGHTS) failed: {}", e);
        }

        let mut received_fd: Option<std::os::unix::io::RawFd> = None;
        let mut cmsg = libc::CMSG_FIRSTHDR(&msghdr);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let fd_ptr = libc::CMSG_DATA(cmsg) as *const libc::c_int;
                received_fd = Some(std::ptr::read(fd_ptr));
                break;
            }
            cmsg = libc::CMSG_NXTHDR(&msghdr, cmsg);
        }

        match received_fd {
            Some(fd) => Ok(fd),
            None => anyhow::bail!("recvmsg did not contain SCM_RIGHTS fd"),
        }
    }
}
