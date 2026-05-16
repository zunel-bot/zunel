//! Body-cap helpers for reading `reqwest::Response` payloads safely.
//!
//! Every place in the workspace that calls `response.text()` /
//! `.bytes()` / `.json()` on an attacker-influenced endpoint is a
//! potential OOM: a hostile peer can chunked-transfer arbitrary bytes
//! and we'll happily buffer them all before deciding what to do. This
//! module centralises the cap so a single tweak (raise the limit, swap
//! error type) lands in every caller at once.

use futures::StreamExt;

/// Error returned by [`read_text_capped`] / [`read_bytes_capped`].
#[derive(Debug, thiserror::Error)]
pub enum BodyReadError {
    #[error("response body exceeded cap of {cap} bytes")]
    TooLarge { cap: usize },
    #[error("body read failed: {0}")]
    Network(#[from] reqwest::Error),
    #[error("body was not valid UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
}

/// Stream the response body into a `Vec<u8>`, refusing once
/// accumulated bytes exceed `cap`. Unlike `response.bytes()`, the
/// peer can't OOM us by chunked-transferring beyond the cap — we
/// abort the read mid-stream the moment we cross the threshold.
pub async fn read_bytes_capped(
    response: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, BodyReadError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if buf.len().saturating_add(chunk.len()) > cap {
            return Err(BodyReadError::TooLarge { cap });
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Stream the response body into a UTF-8 `String`, capped at `cap`
/// bytes. Same OOM defence as [`read_bytes_capped`], plus the UTF-8
/// validation step.
pub async fn read_text_capped(
    response: reqwest::Response,
    cap: usize,
) -> Result<String, BodyReadError> {
    let bytes = read_bytes_capped(response, cap).await?;
    Ok(String::from_utf8(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_response(body: &'static [u8]) -> reqwest::Response {
        // Build a synthetic reqwest::Response without standing up an
        // HTTP server: http::Response → reqwest::Response::from.
        let http_resp = http::Response::builder()
            .status(200)
            .body(body.to_vec())
            .unwrap();
        reqwest::Response::from(http_resp)
    }

    #[tokio::test]
    async fn read_text_capped_passes_when_under_cap() {
        let resp = small_response(b"hello world");
        let got = read_text_capped(resp, 1024).await.unwrap();
        assert_eq!(got, "hello world");
    }

    #[tokio::test]
    async fn read_text_capped_rejects_when_over_cap() {
        let resp = small_response(b"hello world");
        let err = read_text_capped(resp, 5).await.unwrap_err();
        match err {
            BodyReadError::TooLarge { cap } => assert_eq!(cap, 5),
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_bytes_capped_returns_bytes_unchanged() {
        let resp = small_response(b"\xff\x00\x42");
        let got = read_bytes_capped(resp, 16).await.unwrap();
        assert_eq!(got, vec![0xff, 0x00, 0x42]);
    }
}
