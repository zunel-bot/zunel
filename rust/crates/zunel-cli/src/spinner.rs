use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::{cursor, execute, style::Print, terminal};
use tokio::task::JoinHandle;
use tokio::time::sleep;

const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const LABEL: &str = "zunel is thinking...";

/// Non-blocking thinking spinner printed to stderr. stderr is chosen so
/// the spinner never interleaves with the streaming response on stdout
/// and so `2>/dev/null` gives a clean transcript.
pub struct ThinkingSpinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ThinkingSpinner {
    pub fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let handle = tokio::spawn(async move {
            if !is_stderr_tty() {
                return;
            }
            let mut frame = 0usize;
            let mut err = io::stderr();
            while !stop_clone.load(Ordering::Acquire) {
                let glyph = FRAMES[frame % FRAMES.len()];
                let _ = execute!(
                    err,
                    cursor::SavePosition,
                    terminal::Clear(terminal::ClearType::CurrentLine),
                    cursor::MoveToColumn(0),
                    Print(format!("{glyph} {LABEL}")),
                    cursor::RestorePosition,
                );
                let _ = err.flush();
                frame += 1;
                sleep(Duration::from_millis(100)).await;
            }
            let _ = execute!(
                err,
                terminal::Clear(terminal::ClearType::CurrentLine),
                cursor::MoveToColumn(0),
            );
            let _ = err.flush();
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub async fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

fn is_stderr_tty() -> bool {
    use std::io::IsTerminal;
    io::stderr().is_terminal()
}
