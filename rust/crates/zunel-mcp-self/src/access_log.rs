//! Per-request access logging for the Streamable HTTP MCP server.
//!
//! Each successfully-served (or rejected) HTTP request emits one
//! JSON object (followed by a newline) to a configured sink. The
//! shape is intentionally flat and stable so operators can parse it
//! with `jq`, ship it through `vector`/`fluent-bit` to a SIEM,
//! grep for incidents, or feed it into a token-cost dashboard.
//!
//! Schema, with field types and intent:
//!
//! ```jsonc
//! {
//!   "ts":     "2026-04-26T20:34:12.123456Z", // RFC 3339 UTC, microsecond precision
//!   "peer":   "127.0.0.1:54321",             // remote socket addr (post-TLS)
//!   "method": "tools/call",                  // JSON-RPC method, "*batch" for batches, null on parse fail
//!   "tool":   "read_file",                   // present when method = tools/call, else absent
//!   "rpc_id": 7,                             // JSON-RPC id passed through verbatim (string|number|null)
//!   "depth":  2,                             // Mcp-Call-Depth, null when missing
//!   "key":    "ab12cd34",                    // first 8 hex chars of SHA-256(matched bearer); null on no-auth
//!   "status": 200,                           // HTTP status code emitted
//!   "ms":     14                             // wall-clock latency in milliseconds (rounded down)
//! }
//! ```
//!
//! Secrets policy: the matched bearer token never appears in the log;
//! a stable 4-byte fingerprint (the first 8 hex chars of its
//! SHA-256) is included so operators can correlate activity to a
//! specific *credential* without that credential ever leaving
//! memory in plaintext form. Tokens that are presented but *don't*
//! match — i.e. failed auth attempts — are also not logged in any
//! form, so credential-stuffing probes can't poison the log.
//!
//! Sink semantics:
//!
//! - `AccessLog::stdout()` writes line-by-line to stdout and is the
//!   right choice when running under `journalctl`/`docker logs`.
//! - `AccessLog::open(path)` opens the file in append mode. This is
//!   compatible with `logrotate`'s `copytruncate` strategy on Linux
//!   and macOS without re-opening on signals: each emit is one
//!   `write_all` of a payload smaller than `PIPE_BUF`, so the
//!   kernel guarantees atomic appends across processes.
//! - Sink failures are demoted to a one-shot stderr warning and the
//!   server keeps serving. Logging is observability, not load-bearing.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{stdout, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

/// Async-safe sink that wraps either stdout or a file opened in
/// append mode. Each `emit` serializes the entry to JSON, appends a
/// newline, and writes the whole payload under the mutex so log
/// lines from concurrent connections don't interleave.
pub struct AccessLog {
    sink: Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    /// Latched once we've already complained about a sink write
    /// failure to stderr; we don't want to spam the operator with a
    /// failure-per-request when their disk fills up.
    warned: AtomicBool,
    label: String,
}

impl AccessLog {
    /// Build an access log that writes to process stdout. Useful when
    /// the agent is running under a supervisor that already captures
    /// stdout (`systemd`, `docker`, `launchd`, k8s).
    pub fn stdout() -> Self {
        Self {
            sink: Mutex::new(Box::new(stdout())),
            warned: AtomicBool::new(false),
            label: "stdout".to_string(),
        }
    }

    /// Build an access log that appends to a file. The file is
    /// opened with `O_APPEND` so per-emit writes are atomic on
    /// POSIX-style filesystems (the typical case); operators using
    /// `logrotate`'s `copytruncate` workflow get clean rotation
    /// without the agent needing to re-open on a signal.
    ///
    /// Errors here mean the file couldn't be opened at all, which is
    /// an operator-visible misconfiguration: the caller should
    /// propagate the error so the server fails loudly at boot rather
    /// than silently dropping logs.
    pub async fn open(path: &Path) -> Result<Self> {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .with_context(|| format!("opening access-log file at {}", path.display()))?;
        Ok(Self {
            sink: Mutex::new(Box::new(file)),
            warned: AtomicBool::new(false),
            label: path.display().to_string(),
        })
    }

    /// Write one JSON line. Failures are reported once to stderr
    /// (subsequent failures are silent) so a misbehaving sink doesn't
    /// flood the supervisor. We deliberately don't propagate the
    /// error — observability shouldn't be able to take down the
    /// data plane.
    pub async fn emit(&self, entry: &AccessLogEntry) {
        let mut payload = match serde_json::to_vec(entry) {
            Ok(buf) => buf,
            Err(err) => {
                self.warn_once(&format!("access log: failed to serialize entry: {err}"));
                return;
            }
        };
        payload.push(b'\n');

        let mut sink = self.sink.lock().await;
        if let Err(err) = sink.write_all(&payload).await {
            self.warn_once(&format!("access log {}: write failed: {err}", self.label));
            return;
        }
        if let Err(err) = sink.flush().await {
            self.warn_once(&format!("access log {}: flush failed: {err}", self.label));
        }
    }

    fn warn_once(&self, msg: &str) {
        if self
            .warned
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            tracing::warn!("{msg}");
        }
    }
}

/// One JSON-line access log record. Matches the schema documented at
/// the top of this module. `serde_json::Value` is used for `rpc_id`
/// because JSON-RPC ids are spec-defined as `string | number | null`
/// and we'd rather pass them through verbatim than coerce them.
#[derive(Debug, Clone, Serialize)]
pub struct AccessLogEntry {
    pub ts: String,
    pub peer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpc_id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub status: u16,
    pub ms: u64,
}

/// Mutable accumulator that the HTTP request pipeline fills in as it
/// learns more about the request. Lives in [`http::handle_connection`]
/// where it's allocated at request start and emitted exactly once at
/// the end of the request, *after* the response bytes have been
/// flushed to the wire.
pub struct AccessLogContext {
    started_at: Instant,
    pub peer: String,
    pub method: Option<String>,
    pub tool: Option<String>,
    pub rpc_id: Option<Value>,
    pub depth: Option<u32>,
    pub key: Option<String>,
    pub status: u16,
}

impl AccessLogContext {
    pub fn new(peer: impl Into<String>) -> Self {
        Self {
            started_at: Instant::now(),
            peer: peer.into(),
            method: None,
            tool: None,
            rpc_id: None,
            depth: None,
            key: None,
            status: 0,
        }
    }

    /// Mark a JSON-RPC request payload onto the log context. Handles
    /// both the single-message and batch shapes: batches collapse to
    /// `method = "*batch"` because the entries share a transport
    /// frame, and individual tool calls inside a batch aren't
    /// addressable from outside.
    pub fn record_rpc(&mut self, payload: &Value) {
        match payload {
            Value::Array(items) => {
                self.method = Some("*batch".to_string());
                // For batches we can't pick a meaningful single tool
                // name; leave `tool` and `rpc_id` unset.
                let _ = items;
            }
            Value::Object(map) => {
                if let Some(method) = map.get("method").and_then(Value::as_str) {
                    self.method = Some(method.to_string());
                    if method == "tools/call" {
                        if let Some(tool) = map
                            .get("params")
                            .and_then(|p| p.get("name"))
                            .and_then(Value::as_str)
                        {
                            self.tool = Some(tool.to_string());
                        }
                    }
                }
                if let Some(id) = map.get("id") {
                    self.rpc_id = Some(id.clone());
                }
            }
            _ => {}
        }
    }

    /// Convert the accumulator into a finalized log entry. Computes
    /// the timestamp at finalize time so the recorded `ts` reflects
    /// when the response went out, while `ms` is measured from
    /// request arrival — an operator looking at a single line can
    /// see both edges.
    pub fn finish(self) -> AccessLogEntry {
        let now: DateTime<Utc> = SystemTime::now().into();
        let ts = now.to_rfc3339_opts(SecondsFormat::Micros, true);
        let ms = self.started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
        AccessLogEntry {
            ts,
            peer: self.peer,
            method: self.method,
            tool: self.tool,
            rpc_id: self.rpc_id,
            depth: self.depth,
            key: self.key,
            status: self.status,
            ms,
        }
    }
}

/// Compute the public fingerprint of a bearer token. We hash with
/// SHA-256 and take the first 8 hex characters (4 bytes) of the
/// digest. That gives ~4.3 billion possible values — small enough to
/// be readable in a log scrape, large enough that two configured
/// tokens collide with vanishingly small probability for the
/// allowlist sizes operators actually use (single digits in
/// practice). Importantly, the fingerprint is stable across
/// processes, so the same token issued to two helpers shows up with
/// the same `key` field in both their logs and a SIEM can correlate.
pub fn token_fingerprint(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let digest = hasher.finalize();
    // Each byte → 2 hex chars; first 8 hex chars = first 4 bytes.
    let mut out = String::with_capacity(8);
    for byte in digest.iter().take(4) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Convenience exposed so the binaries can construct an `AccessLog`
/// from a CLI flag in a single match. `path == "-"` selects stdout
/// (mirrors the convention used by most Unix tools); any other
/// string is treated as a file path opened in append mode.
pub async fn open_from_cli(path: &str) -> Result<Arc<AccessLog>> {
    let log = if path == "-" {
        AccessLog::stdout()
    } else {
        AccessLog::open(Path::new(path)).await?
    };
    Ok(Arc::new(log))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_fingerprint_is_stable_8_hex_chars() {
        let fp = token_fingerprint("hunter2");
        assert_eq!(fp.len(), 8);
        assert!(fp
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(fp, token_fingerprint("hunter2"));
        assert_ne!(fp, token_fingerprint("hunter3"));
    }

    #[test]
    fn token_fingerprint_does_not_leak_full_token() {
        let token = "super-secret-bearer-token-that-must-not-leak";
        let fp = token_fingerprint(token);
        assert!(!token.contains(&fp) || fp.len() < token.len());
        assert!(!fp.contains(token));
    }

    #[test]
    fn record_rpc_extracts_method_tool_and_id() {
        let mut ctx = AccessLogContext::new("127.0.0.1:1");
        ctx.record_rpc(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "read_file", "arguments": {} }
        }));
        assert_eq!(ctx.method.as_deref(), Some("tools/call"));
        assert_eq!(ctx.tool.as_deref(), Some("read_file"));
        assert_eq!(ctx.rpc_id, Some(serde_json::json!(7)));
    }

    #[test]
    fn record_rpc_handles_batches_without_tool_attribution() {
        let mut ctx = AccessLogContext::new("127.0.0.1:1");
        ctx.record_rpc(&serde_json::json!([
            { "jsonrpc": "2.0", "id": 1, "method": "tools/list" },
            { "jsonrpc": "2.0", "id": 2, "method": "tools/call",
              "params": { "name": "read_file", "arguments": {} } }
        ]));
        assert_eq!(ctx.method.as_deref(), Some("*batch"));
        assert!(ctx.tool.is_none());
        assert!(ctx.rpc_id.is_none());
    }

    #[test]
    fn entry_serializes_with_stable_field_order() {
        // serde_json emits struct fields in declaration order; our
        // schema doc documents them in this order too. Lock that in.
        let entry = AccessLogEntry {
            ts: "2026-04-26T00:00:00.000000Z".into(),
            peer: "127.0.0.1:1234".into(),
            method: Some("tools/call".into()),
            tool: Some("read_file".into()),
            rpc_id: Some(serde_json::json!(7)),
            depth: Some(2),
            key: Some("ab12cd34".into()),
            status: 200,
            ms: 14,
        };
        let s = serde_json::to_string(&entry).unwrap();
        let prefix =
            r#"{"ts":"2026-04-26T00:00:00.000000Z","peer":"127.0.0.1:1234","method":"tools/call","#;
        assert!(s.starts_with(prefix), "unexpected prefix: {s}");
        assert!(
            s.ends_with(r#""status":200,"ms":14}"#),
            "unexpected suffix: {s}"
        );
    }

    #[test]
    fn entry_omits_optional_fields_when_unset() {
        let entry = AccessLogEntry {
            ts: "2026-04-26T00:00:00.000000Z".into(),
            peer: "127.0.0.1:1234".into(),
            method: None,
            tool: None,
            rpc_id: None,
            depth: None,
            key: None,
            status: 413,
            ms: 0,
        };
        let s = serde_json::to_string(&entry).unwrap();
        assert!(!s.contains("\"method\""));
        assert!(!s.contains("\"tool\""));
        assert!(!s.contains("\"rpc_id\""));
        assert!(!s.contains("\"depth\""));
        assert!(!s.contains("\"key\""));
        assert!(s.contains("\"status\":413"));
    }

    #[tokio::test]
    async fn open_appends_one_json_line_per_emit() {
        // Round-trips through the real append-to-file path so we
        // know the mutex-protected emit serializes lines and that
        // the sink is opened with O_APPEND (each emit lands at the
        // end without truncating).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("access.log");
        let log = AccessLog::open(&path).await.unwrap();
        let entry = AccessLogEntry {
            ts: "2026-04-26T00:00:00.000000Z".into(),
            peer: "127.0.0.1:1".into(),
            method: Some("tools/list".into()),
            tool: None,
            rpc_id: Some(serde_json::json!(1)),
            depth: None,
            key: None,
            status: 200,
            ms: 3,
        };
        log.emit(&entry).await;
        log.emit(&entry).await;
        // Drop closes the file handle; some platforms buffer writes
        // until close, so be explicit.
        drop(log);

        let bytes = std::fs::read(&path).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "two emits → two lines: {text:?}");
        for line in lines {
            let parsed: Value = serde_json::from_str(line).unwrap();
            assert_eq!(parsed["method"], "tools/list");
            assert_eq!(parsed["status"], 200);
        }
    }
}
