//! Stdin-backed approval handler. Prints the request to stderr, reads
//! one line from the configured reader, treats `y`/`yes` as approve.
//! Timeout defaults to 60 s; on timeout or EOF the decision is Deny.

use std::io::{self, Write};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;
use tokio::time::timeout;

use zunel_core::{ApprovalDecision, ApprovalHandler, ApprovalRequest};

/// Approval handler that reads from an `AsyncBufRead` (stdin by
/// default). Tests inject a `Cursor` to avoid touching the real
/// stdin, which in tokio keeps a background thread alive past the
/// test's exit point.
pub struct StdinApprovalHandler<R: AsyncBufRead + Unpin + Send> {
    pub timeout: Duration,
    reader: Mutex<R>,
}

impl StdinApprovalHandler<BufReader<tokio::io::Stdin>> {
    pub fn new() -> Self {
        Self::with_reader(BufReader::new(tokio::io::stdin()))
    }
}

impl Default for StdinApprovalHandler<BufReader<tokio::io::Stdin>> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: AsyncBufRead + Unpin + Send> StdinApprovalHandler<R> {
    pub fn with_reader(reader: R) -> Self {
        Self {
            timeout: Duration::from_secs(60),
            reader: Mutex::new(reader),
        }
    }

    #[allow(dead_code)] // exercised by the lib's integration tests
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl<R> ApprovalHandler for StdinApprovalHandler<R>
where
    R: AsyncBufRead + Unpin + Send + 'static,
{
    async fn request(&self, req: ApprovalRequest) -> ApprovalDecision {
        let mut stderr = io::stderr();
        let _ = writeln!(
            stderr,
            "\n[approval required] {}\n  {}\nApprove? [y/N]: ",
            req.tool_name, req.description
        );
        let _ = stderr.flush();
        let mut line = String::new();
        let read_fut = async {
            let mut guard = self.reader.lock().await;
            match guard.read_line(&mut line).await {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(line.trim().to_lowercase()),
            }
        };
        match timeout(self.timeout, read_fut).await {
            Ok(Some(s)) if matches!(s.as_str(), "y" | "yes") => ApprovalDecision::Approve,
            _ => ApprovalDecision::Deny,
        }
    }
}
