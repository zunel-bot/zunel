//! Disk persistence for [`super::Session`].
//!
//! Sessions are stored as JSONL with one metadata header line followed by
//! one row per message. The [`CompactFormatter`] uses `(", ", ": ")` as the
//! item/key separators (compact but with a single space after each
//! delimiter) so the on-disk layout is human-readable without bloating the
//! file. Atomic save uses the tempfile-then-rename trick so concurrent
//! loaders never see a partial write.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{naive_local_iso_now, Session};

/// Owns the `<workspace>/sessions/` directory and performs atomic load + save.
#[derive(Debug, Clone)]
pub struct SessionManager {
    sessions_dir: PathBuf,
}

impl SessionManager {
    pub fn new(workspace: &Path) -> Self {
        let sessions_dir = zunel_config::sessions_dir(workspace);
        if let Err(e) = zunel_util::ensure_dir(&sessions_dir) {
            tracing::warn!(path = %sessions_dir.display(), error = %e, "failed to create sessions dir");
        }
        Self { sessions_dir }
    }

    /// Replace path-unsafe characters with `_` and trim surrounding
    /// whitespace, so a session key like `slack:D0AUX99UNR0` becomes a
    /// safe filename `slack_D0AUX99UNR0`. The unsafe set is
    /// `[<>:"/\\|?*]`; ordinary letters, digits, spaces, periods, and
    /// non-ASCII codepoints pass through unchanged.
    pub(super) fn safe_key(key: &str) -> String {
        const UNSAFE: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
        key.chars()
            .map(|c| if UNSAFE.contains(&c) { '_' } else { c })
            .collect::<String>()
            .trim()
            .to_string()
    }

    fn session_path(&self, key: &str) -> PathBuf {
        self.sessions_dir
            .join(format!("{}.jsonl", Self::safe_key(key)))
    }

    /// Returns the on-disk path for `key`, creating the parent
    /// `sessions/` directory if needed but not the file itself. Useful
    /// for `zunel sessions show/clear/prune` which need to stat or
    /// remove the file directly.
    pub fn path_for(&self, key: &str) -> PathBuf {
        self.session_path(key)
    }

    /// Root directory holding all session files.
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// List all persisted session keys by scanning `sessions/`. The
    /// returned keys are decoded back to `channel:chat_id` form (the
    /// inverse of `safe_key`'s `:` → `_` substitution is best-effort:
    /// we keep the on-disk filename verbatim minus the `.jsonl`).
    pub fn list_keys(&self) -> crate::Result<Vec<String>> {
        if !self.sessions_dir.exists() {
            return Ok(Vec::new());
        }
        let entries =
            std::fs::read_dir(&self.sessions_dir).map_err(|source| crate::Error::Session {
                path: self.sessions_dir.clone(),
                source: source.into(),
            })?;
        let mut keys = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                keys.push(stem.to_string());
            }
        }
        keys.sort();
        Ok(keys)
    }

    /// Delete the session file for `key`. Returns `Ok(false)` if the
    /// file did not exist (so callers can treat it as a no-op).
    pub fn delete(&self, key: &str) -> crate::Result<bool> {
        let path = self.session_path(key);
        if !path.exists() {
            return Ok(false);
        }
        std::fs::remove_file(&path).map_err(|source| crate::Error::Session {
            path,
            source: source.into(),
        })?;
        Ok(true)
    }

    pub fn load(&self, key: &str) -> crate::Result<Option<Session>> {
        let path = self.session_path(key);
        if !path.exists() {
            return Ok(None);
        }
        let file = File::open(&path).map_err(|source| crate::Error::Session {
            path: path.clone(),
            source: source.into(),
        })?;
        let reader = BufReader::new(file);

        let mut messages = Vec::new();
        let mut key_from_file: Option<String> = None;
        let mut created_at: Option<String> = None;
        let mut updated_at: Option<String> = None;
        let mut metadata = Value::Object(Default::default());
        let mut last_consolidated = 0usize;

        for line in reader.lines() {
            let line = line.map_err(|source| crate::Error::Session {
                path: path.clone(),
                source: source.into(),
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value =
                serde_json::from_str(&line).map_err(|source| crate::Error::Session {
                    path: path.clone(),
                    source: source.into(),
                })?;
            if value.get("_type").and_then(Value::as_str) == Some("metadata") {
                key_from_file = value.get("key").and_then(Value::as_str).map(str::to_string);
                created_at = value
                    .get("created_at")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                updated_at = value
                    .get("updated_at")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(m) = value.get("metadata") {
                    metadata = m.clone();
                }
                last_consolidated = value
                    .get("last_consolidated")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
            } else {
                messages.push(value);
            }
        }

        let now = naive_local_iso_now();
        Ok(Some(Session::from_parts(
            key_from_file.unwrap_or_else(|| key.to_string()),
            messages,
            created_at.unwrap_or_else(|| now.clone()),
            updated_at.unwrap_or(now),
            metadata,
            last_consolidated,
        )))
    }

    /// Async wrapper around [`Self::save`] that runs the synchronous
    /// I/O on a `spawn_blocking` thread so the caller's tokio worker
    /// isn't pinned across the write + fsync. Use this from `async fn`
    /// hot paths (notably the agent loop's per-turn persist); sync
    /// callers (tests, REPL command handlers) can keep using
    /// [`Self::save`] directly.
    pub async fn save_async(&self, session: &Session) -> crate::Result<()> {
        let manager = self.clone();
        let session = session.clone();
        let session_path = manager.session_path(&session.key);
        match tokio::task::spawn_blocking(move || manager.save(&session)).await {
            Ok(result) => result,
            Err(join_err) => Err(crate::Error::Session {
                path: session_path,
                source: Box::new(std::io::Error::other(format!(
                    "session save task panicked: {join_err}"
                ))),
            }),
        }
    }

    pub fn save(&self, session: &Session) -> crate::Result<()> {
        let path = self.session_path(&session.key);
        let tmp_path = path.with_extension("jsonl.tmp");

        // Cross-process exclusive lock. Two zunel processes pointed at
        // the same workspace (e.g. someone running `zunel agent` while
        // the gateway is processing the same channel session) would
        // otherwise race on last-writer-wins. The lock file sits
        // alongside the session as `<key>.jsonl.lock` and is held only
        // for the duration of the write. Lock acquisition is blocking
        // — the save_async wrapper already runs us on a spawn_blocking
        // thread, so this doesn't pin a tokio worker.
        let lock_path = path.with_extension("jsonl.lock");
        let lock_file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| crate::Error::Session {
                path: lock_path.clone(),
                source: source.into(),
            })?;
        let mut lock = fd_lock::RwLock::new(lock_file);
        let _guard = lock.write().map_err(|source| crate::Error::Session {
            path: lock_path.clone(),
            source: source.into(),
        })?;

        // serde_json's compact writer uses `(",", ":")`. We want
        // `(", ", ": ")` so the on-disk JSONL stays readable; hand-
        // serialize line by line through `CompactFormatter`.
        let mut file = File::create(&tmp_path).map_err(|source| crate::Error::Session {
            path: tmp_path.clone(),
            source: source.into(),
        })?;
        write_compact_json(&mut file, &session.metadata_line())?;
        file.write_all(b"\n")
            .map_err(|source| crate::Error::Session {
                path: tmp_path.clone(),
                source: source.into(),
            })?;
        for msg in &session.messages {
            write_compact_json(&mut file, msg)?;
            file.write_all(b"\n")
                .map_err(|source| crate::Error::Session {
                    path: tmp_path.clone(),
                    source: source.into(),
                })?;
        }
        // `rename` is atomic at the directory-entry level, but without an
        // fsync first the kernel can publish a rename whose target is
        // still mostly in-cache. A host crash between the rename and the
        // writeback leaves a renamed-but-truncated session file. Force
        // the bytes through before the entry-swap.
        file.sync_all().map_err(|source| crate::Error::Session {
            path: tmp_path.clone(),
            source: source.into(),
        })?;
        drop(file);

        fs::rename(&tmp_path, &path).map_err(|source| crate::Error::Session {
            path: path.clone(),
            source: source.into(),
        })?;
        Ok(())
    }
}

/// Emit JSON with `", "` between items and `": "` between keys and
/// values, matching the on-disk session-file format.
fn write_compact_json(writer: &mut impl Write, value: &Value) -> crate::Result<()> {
    let formatter = CompactFormatter;
    let mut ser = serde_json::Serializer::with_formatter(writer, formatter);
    serde::Serialize::serialize(value, &mut ser).map_err(|source| crate::Error::Session {
        path: PathBuf::new(),
        source: source.into(),
    })?;
    Ok(())
}

struct CompactFormatter;

impl serde_json::ser::Formatter for CompactFormatter {
    fn begin_array_value<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }
    fn begin_object_key<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }
    fn begin_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> std::io::Result<()> {
        writer.write_all(b": ")
    }
}
