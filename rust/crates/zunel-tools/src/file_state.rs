use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

/// Tracks which paths the agent has `read_file`'d in this session and
/// the mtime at read time. `edit_file` uses this to refuse stale edits.
#[derive(Debug, Clone, Default)]
pub struct FileStateTracker {
    inner: Arc<Mutex<HashMap<PathBuf, SystemTime>>>,
}

impl FileStateTracker {
    fn lock(&self) -> MutexGuard<'_, HashMap<PathBuf, SystemTime>> {
        // Recover from poisoning: a previous panic shouldn't blow up subsequent
        // file-state lookups. The map is only used to detect stale edits, so a
        // partially-updated entry is safe to read.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn mark_read(&self, path: PathBuf, mtime: SystemTime) {
        self.lock().insert(path, mtime);
    }
    pub fn last_read(&self, path: &Path) -> Option<SystemTime> {
        self.lock().get(path).copied()
    }
    pub fn invalidate(&self, path: &Path) {
        self.lock().remove(path);
    }
}
