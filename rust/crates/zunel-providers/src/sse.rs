//! Minimal SSE (Server-Sent Events) line buffer. Not a general-purpose
//! implementation — only the subset OpenAI-compatible chat.completions
//! streams emit: `data:` lines with optional multi-line continuations,
//! event boundaries on blank lines, `[DONE]` sentinel.

/// Hard ceiling on the internal accumulators. A hostile (or buggy)
/// provider that streams a `\n`-free body forever would otherwise grow
/// `line_buf` and `event_data` without bound. 1 MiB is well above any
/// legitimate SSE payload (a single chat-completions delta is a few KB).
pub const MAX_SSE_BUFFER_BYTES: usize = 1024 * 1024;

/// Error returned by [`SseBuffer::feed`] when accumulated bytes exceed
/// [`MAX_SSE_BUFFER_BYTES`]. The caller should drop the stream — at this
/// point we either have a misbehaving peer or a non-SSE response that
/// got handed to the SSE parser.
#[derive(Debug, Clone, thiserror::Error)]
#[error("SSE buffer exceeded cap of {cap} bytes")]
pub struct SseOverflow {
    pub cap: usize,
}

/// Accumulates partial chunks and emits `Vec<Option<String>>` where:
/// - `Some(data)` is a complete `data:` payload (joined across lines).
/// - `None` is the `[DONE]` sentinel indicating end-of-stream.
#[derive(Debug, Default)]
pub struct SseBuffer {
    /// Raw bytes pending UTF-8 decoding. When `reqwest::bytes_stream()`
    /// splits a multi-byte codepoint across chunks, the tail bytes live
    /// here until the continuation arrives.
    pending: Vec<u8>,
    line_buf: String,
    event_data: Vec<String>,
    event_data_bytes: usize,
}

impl SseBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes from the wire. Returns any events that completed in
    /// this chunk. Multiple events per chunk are possible; partial events
    /// stay buffered until the next call.
    ///
    /// Returns [`SseOverflow`] when any internal buffer would grow past
    /// [`MAX_SSE_BUFFER_BYTES`]; the caller should drop the stream.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Option<String>>, SseOverflow> {
        let mut events = Vec::new();
        self.pending.extend_from_slice(bytes);
        if self.pending.len() > MAX_SSE_BUFFER_BYTES {
            return Err(SseOverflow {
                cap: MAX_SSE_BUFFER_BYTES,
            });
        }
        let valid_up_to = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) => e.valid_up_to(),
        };
        if valid_up_to > 0 {
            let decoded = std::str::from_utf8(&self.pending[..valid_up_to])
                .expect("valid_up_to bytes are by definition valid UTF-8");
            self.line_buf.push_str(decoded);
            self.pending.drain(..valid_up_to);
        }
        if self.line_buf.len() > MAX_SSE_BUFFER_BYTES {
            return Err(SseOverflow {
                cap: MAX_SSE_BUFFER_BYTES,
            });
        }

        // Process all complete lines in the buffer.
        while let Some(idx) = self.line_buf.find('\n') {
            let mut line = self.line_buf[..idx].to_string();
            self.line_buf.drain(..=idx);
            if line.ends_with('\r') {
                line.pop();
            }

            if line.is_empty() {
                // Event boundary.
                if !self.event_data.is_empty() {
                    let payload = self.event_data.join("\n");
                    self.event_data.clear();
                    self.event_data_bytes = 0;
                    if payload == "[DONE]" {
                        events.push(None);
                    } else {
                        events.push(Some(payload));
                    }
                }
                continue;
            }

            if line.starts_with(':') {
                // Comment line — ignore.
                continue;
            }

            // "field: value" parse. Ignore fields other than "data".
            if let Some(rest) = line.strip_prefix("data:") {
                let value = rest.strip_prefix(' ').unwrap_or(rest);
                self.event_data_bytes = self.event_data_bytes.saturating_add(value.len());
                if self.event_data_bytes > MAX_SSE_BUFFER_BYTES {
                    return Err(SseOverflow {
                        cap: MAX_SSE_BUFFER_BYTES,
                    });
                }
                self.event_data.push(value.to_string());
            }
            // Other fields (event, id, retry) ignored — OpenAI does not use them.
        }

        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_handles_data_then_blank_then_done() {
        let mut sse = SseBuffer::new();
        let events = sse.feed(b"data: hello\n\ndata: [DONE]\n\n").unwrap();
        assert_eq!(events, vec![Some("hello".to_string()), None]);
    }

    #[test]
    fn feed_rejects_unbounded_line() {
        // A hostile provider streaming forever without a `\n` should
        // trip the cap rather than OOM.
        let mut sse = SseBuffer::new();
        let big = vec![b'a'; MAX_SSE_BUFFER_BYTES + 1];
        let err = sse.feed(&big).expect_err("oversized chunk must overflow");
        assert_eq!(err.cap, MAX_SSE_BUFFER_BYTES);
    }

    #[test]
    fn feed_rejects_unbounded_multi_chunk_line() {
        // Same overflow detection across many smaller chunks.
        let mut sse = SseBuffer::new();
        let chunk = vec![b'b'; 64 * 1024];
        let mut hit_overflow = false;
        for _ in 0..32 {
            if sse.feed(&chunk).is_err() {
                hit_overflow = true;
                break;
            }
        }
        assert!(
            hit_overflow,
            "expected overflow after enough \\n-less chunks"
        );
    }

    #[test]
    fn feed_accumulates_event_data_across_chunks() {
        let mut sse = SseBuffer::new();
        assert!(sse.feed(b"data: part1\n").unwrap().is_empty());
        let events = sse.feed(b"data: part2\n\n").unwrap();
        assert_eq!(events, vec![Some("part1\npart2".to_string())]);
    }
}
