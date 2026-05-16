use std::path::Path;

/// Create `path` and all missing parent directories. Idempotent.
pub fn ensure_dir(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(path)
}
