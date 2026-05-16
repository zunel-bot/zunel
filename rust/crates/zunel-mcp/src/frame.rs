//! JSON-RPC over stdio framing primitives shared by the built-in MCP
//! servers (`zunel-mcp-self`, `zunel-mcp-slack`).
//!
//! MCP uses the LSP-style framing convention:
//!
//! ```text
//! Content-Length: NNN\r\n
//! \r\n
//! {…N bytes of JSON…}
//! ```
//!
//! Each helper here is intentionally minimal — the goal is to centralize
//! the framing rules in one place so a tweak (extra header, length-limit
//! cap) lands in every binary at once. Per-server method dispatch stays
//! in the binary's own `main.rs`.
//!
//! Both helpers map their failure modes onto [`crate::Error`] so they
//! interop cleanly with `anyhow::Result` on the binary side via
//! `?` / `From<zunel_mcp::Error> for anyhow::Error`.

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{Error, Result};

/// Hard cap on a single MCP frame body. A misbehaving (or malicious) peer
/// could otherwise announce an arbitrarily large `Content-Length` and force
/// the host to allocate that many bytes before any I/O fails.
pub const MAX_FRAME_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Read one Content-Length-framed JSON-RPC message from `reader`.
///
/// Returns `Err(Error::Protocol("stdin closed"))` when the peer hangs up
/// mid-frame so the caller can break its read loop cleanly. Header
/// parsing is case-insensitive on the field name to match the LSP spec.
pub async fn read_frame<R>(reader: &mut R) -> Result<Value>
where
    R: AsyncReadExt + Unpin,
{
    let mut header = Vec::new();
    let mut byte = [0_u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return Err(Error::Protocol("stdin closed".into()));
        }
        header.push(byte[0]);
    }
    let header =
        String::from_utf8(header).map_err(|_| Error::Protocol("non-UTF-8 header".into()))?;
    let len = header
        .split("\r\n")
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| Error::Protocol("missing Content-Length".into()))?;
    if len > MAX_FRAME_BODY_BYTES {
        return Err(Error::Protocol(format!(
            "frame body too large: {len} bytes (cap {MAX_FRAME_BODY_BYTES})"
        )));
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Write one Content-Length-framed JSON-RPC message to `writer` and
/// flush. The body is `serde_json::to_vec(value)` so the byte-length on
/// the wire matches the header exactly.
pub async fn write_frame<W>(writer: &mut W, value: &Value) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let body = serde_json::to_vec(value)?;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let value = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &value).await.unwrap();
        let mut reader = BufReader::new(&buf[..]);
        let read = read_frame(&mut reader).await.unwrap();
        assert_eq!(read, value);
    }

    #[tokio::test]
    async fn read_frame_handles_case_insensitive_header() {
        // LSP spec is case-insensitive; some peers ship `content-length`.
        let body = br#"{"hi":1}"#;
        let mut wire: Vec<u8> = Vec::new();
        wire.extend_from_slice(b"content-length: 8\r\n\r\n");
        wire.extend_from_slice(body);
        let mut reader = BufReader::new(&wire[..]);
        let read = read_frame(&mut reader).await.unwrap();
        assert_eq!(read, json!({"hi": 1}));
    }

    #[tokio::test]
    async fn read_frame_returns_protocol_error_on_eof() {
        let mut reader: &[u8] = &[];
        let err = read_frame(&mut reader).await.unwrap_err();
        match err {
            Error::Protocol(msg) => assert!(msg.contains("stdin closed")),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_frame_rejects_missing_content_length() {
        let mut wire: Vec<u8> = Vec::new();
        wire.extend_from_slice(b"x-foo: bar\r\n\r\n");
        wire.extend_from_slice(b"{}");
        let mut reader = BufReader::new(&wire[..]);
        let err = read_frame(&mut reader).await.unwrap_err();
        match err {
            Error::Protocol(msg) => assert!(msg.contains("Content-Length")),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_frame_rejects_oversized_content_length() {
        // A malicious peer announcing a 10 GiB body must be rejected
        // BEFORE allocation; otherwise it OOMs the host.
        let over = MAX_FRAME_BODY_BYTES + 1;
        let wire = format!("Content-Length: {over}\r\n\r\n");
        let mut reader = BufReader::new(wire.as_bytes());
        let err = read_frame(&mut reader).await.unwrap_err();
        match err {
            Error::Protocol(msg) => {
                assert!(
                    msg.contains("too large"),
                    "expected body-too-large error, got {msg:?}"
                );
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
    }
}
