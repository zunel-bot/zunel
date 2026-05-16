use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{Error, Result};
use crate::paths::default_config_path;
use crate::schema::Config;

/// Load zunel config from disk. If `path` is `None`, uses the default
/// (`<zunel_home>/config.json`).
pub fn load_config(path: Option<&Path>) -> Result<Config> {
    let resolved: PathBuf = match path {
        Some(p) => p.to_path_buf(),
        None => default_config_path()?,
    };
    if !resolved.exists() {
        return Err(Error::NotFound(resolved));
    }
    let raw = std::fs::read_to_string(&resolved).map_err(|source| Error::Io {
        path: resolved.clone(),
        source,
    })?;
    let cfg: Config = serde_json::from_str(&raw).map_err(|source| Error::Parse {
        path: resolved.clone(),
        source,
    })?;
    Ok(cfg)
}

/// Load the raw JSON tree of the config file without coercing through
/// `Config`. Useful for surgical edits (the self-modify MCP surface)
/// that need to preserve unknown fields and field order.
pub fn load_config_json(path: Option<&Path>) -> Result<Value> {
    let resolved: PathBuf = match path {
        Some(p) => p.to_path_buf(),
        None => default_config_path()?,
    };
    if !resolved.exists() {
        return Err(Error::NotFound(resolved));
    }
    let raw = std::fs::read_to_string(&resolved).map_err(|source| Error::Io {
        path: resolved.clone(),
        source,
    })?;
    serde_json::from_str(&raw).map_err(|source| Error::Parse {
        path: resolved.clone(),
        source,
    })
}

/// Atomically write a JSON value to the config file at `path` (or the
/// default location). Validates that the value round-trips through
/// the `Config` schema first — if it does not, the file is left
/// untouched and a `Error::Parse` is returned so the caller can
/// surface the schema violation.
///
/// Atomic write: serialise to a sibling tempfile, fsync, then rename
/// over the destination. This matches the pattern used elsewhere in
/// zunel for state-file writes (session JSONL, scheduler.json) so a
/// crashed process can't publish a torn config.
pub fn save_config_json(path: Option<&Path>, value: &Value) -> Result<()> {
    let resolved: PathBuf = match path {
        Some(p) => p.to_path_buf(),
        None => default_config_path()?,
    };

    // Schema check: do not write something we couldn't load back.
    let _: Config = serde_json::from_value(value.clone()).map_err(|source| Error::Parse {
        path: resolved.clone(),
        source,
    })?;

    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let tmp_path = resolved.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(value).map_err(|source| Error::Parse {
        path: resolved.clone(),
        source,
    })?;
    std::fs::write(&tmp_path, &body).map_err(|source| Error::Io {
        path: tmp_path.clone(),
        source,
    })?;
    // Best-effort fsync via OpenOptions on the temp; ignored if the
    // platform refuses (this path is non-critical correctness-wise
    // because the rename below is the atomic primitive).
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&tmp_path) {
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp_path, &resolved).map_err(|source| Error::Io {
        path: resolved.clone(),
        source,
    })?;
    Ok(())
}
