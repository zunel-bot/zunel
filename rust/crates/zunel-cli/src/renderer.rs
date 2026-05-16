use std::io::{self, Write};

use tokio::sync::mpsc::Receiver;
use zunel_providers::{StreamEvent, ToolProgress};

use crate::spinner::ThinkingSpinner;

/// Consumes a stream event channel and writes assistant output to stdout
/// as it arrives. Currently renders plain text only — a richer renderer
/// can replace this module in place without changing call sites.
pub struct StreamingRenderer {
    spinner: Option<ThinkingSpinner>,
    header_printed: bool,
    wrote_anything: bool,
}

impl StreamingRenderer {
    pub fn start() -> Self {
        Self {
            spinner: Some(ThinkingSpinner::start()),
            header_printed: false,
            wrote_anything: false,
        }
    }

    pub async fn drive(mut self, mut rx: Receiver<StreamEvent>) -> io::Result<()> {
        let mut stdout = io::stdout();

        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::ContentDelta(text) => {
                    if text.is_empty() {
                        continue;
                    }
                    if !self.header_printed {
                        if let Some(spinner) = self.spinner.take() {
                            spinner.stop().await;
                        }
                        writeln!(stdout, "\nzunel:")?;
                        self.header_printed = true;
                    }
                    stdout.write_all(text.as_bytes())?;
                    stdout.flush()?;
                    self.wrote_anything = true;
                }
                StreamEvent::Done(_) => {
                    if self.wrote_anything {
                        writeln!(stdout)?;
                    }
                }
                // Per-token tool-call deltas don't render directly;
                // the runner consolidates them and emits ToolProgress
                // events instead.
                StreamEvent::ToolCallDelta { .. } => {}
                StreamEvent::ToolProgress(progress) => {
                    if let Some(spinner) = self.spinner.take() {
                        spinner.stop().await;
                    }
                    if self.wrote_anything {
                        writeln!(stdout)?;
                        self.wrote_anything = false;
                        // After a tool line we may resume streaming
                        // assistant content on the next iteration —
                        // that path will reprint the header.
                        self.header_printed = false;
                    }
                    let line = match progress {
                        ToolProgress::Start { name, .. } => format!("[tool: {name} …]"),
                        ToolProgress::Done {
                            name, ok, snippet, ..
                        } => {
                            let tag = if ok { "ok" } else { "error" };
                            if snippet.is_empty() {
                                format!("[tool: {name} → {tag}]")
                            } else {
                                format!("[tool: {name} → {tag} {snippet}]")
                            }
                        }
                    };
                    writeln!(stdout, "{line}")?;
                    stdout.flush()?;
                }
            }
        }

        if let Some(spinner) = self.spinner.take() {
            spinner.stop().await;
        }
        Ok(())
    }
}
