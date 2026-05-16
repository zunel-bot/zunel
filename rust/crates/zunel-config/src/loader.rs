use std::path::{Path, PathBuf};

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
