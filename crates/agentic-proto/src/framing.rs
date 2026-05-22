//! Length-prefixed JSON framing over an async byte stream.
//!
//! Wire format on the Unix socket:
//!   `<u32 BE length><json payload bytes>`
//!
//! The same routines are used by `agenticd` (server side) and by the CLI /
//! Python SDK (client side). Migrating to protobuf in v1.1 means replacing
//! just this module's payload encoding; the length prefix stays.

use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Hard cap so a malformed/malicious peer can't OOM the daemon. 16 MiB is
/// generous for MVP request shapes.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("frame too large: {0} bytes (max {MAX_FRAME_BYTES})")]
    TooLarge(u32),
}

/// Serialize `value` as JSON and write it length-prefixed to `w`.
pub async fn write_frame<W, T>(w: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let bytes = serde_json::to_vec(value)?;
    let len: u32 = bytes
        .len()
        .try_into()
        .map_err(|_| FrameError::TooLarge(u32::MAX))?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// Read a single length-prefixed JSON frame from `r` and decode into `T`.
/// Returns `Ok(None)` on a clean EOF before any bytes are read.
pub async fn read_frame<R, T>(r: &mut R) -> Result<Option<T>, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let Some(bytes) = read_frame_bytes(r).await? else {
        return Ok(None);
    };
    let value = serde_json::from_slice(&bytes)?;
    Ok(Some(value))
}

/// Read a single length-prefixed JSON frame and return its raw bytes
/// without attempting to deserialise. Used by the daemon's ADR-0010
/// v0/v1 coexistence shim: the daemon peeks at `protocol_version` in
/// the JSON before choosing which `Request` shape to deserialise into.
///
/// Returns `Ok(None)` on a clean EOF before any bytes are read.
pub async fn read_frame_bytes<R>(r: &mut R) -> Result<Option<Vec<u8>>, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(FrameError::Io(e)),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use tokio::io::duplex;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Msg {
        kind: String,
        n: u32,
    }

    #[tokio::test]
    async fn roundtrip_a_message() {
        let (mut a, mut b) = duplex(64);
        let sent = Msg {
            kind: "hello".into(),
            n: 7,
        };
        write_frame(&mut a, &sent).await.unwrap();
        let got: Msg = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(got, sent);
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let (a, mut b) = duplex(64);
        drop(a);
        let got: Option<Msg> = read_frame(&mut b).await.unwrap();
        assert!(got.is_none());
    }
}
