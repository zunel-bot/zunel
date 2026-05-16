use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// Workspace-relative path guard.
#[derive(Debug, Clone, Default)]
pub struct PathPolicy {
    pub restrict_to: Option<PathBuf>,
    pub allowed_extras: Vec<PathBuf>,
}

impl PathPolicy {
    pub fn unrestricted() -> Self {
        Self::default()
    }

    pub fn restricted(workspace: &Path) -> Self {
        Self {
            restrict_to: Some(normalize(workspace)),
            allowed_extras: Vec::new(),
        }
    }

    pub fn with_media_dir(mut self, dir: &Path) -> Self {
        self.allowed_extras.push(normalize(dir));
        self
    }

    pub fn check(&self, path: &Path) -> Result<PathBuf> {
        let resolved = normalize(path);
        let Some(root) = &self.restrict_to else {
            return Ok(resolved);
        };
        let mut roots: Vec<&PathBuf> = vec![root];
        roots.extend(self.allowed_extras.iter());

        // Syntactic fast-path: reject paths whose normalized form is not
        // under any allowed root before touching the filesystem.
        if !roots.iter().any(|r| starts_with(&resolved, r)) {
            return Err(Error::PolicyViolation {
                tool: "<fs>".into(),
                reason: format!("path {resolved:?} is outside workspace {root:?}"),
            });
        }

        // Symlink check: resolve the path through the filesystem (following
        // symlinks for the parts that exist) and re-verify containment
        // against canonicalized roots. Without this a model can do
        // `ln -s /etc/passwd workspace/escape` and then read or write
        // `escape` — the syntactic check above sees a path that starts with
        // the workspace and accepts it.
        let real_path = resolve_real_path(&resolved);
        let real_roots: Vec<PathBuf> = roots
            .iter()
            .map(|r| std::fs::canonicalize(r).unwrap_or_else(|_| (*r).clone()))
            .collect();
        if !real_roots.iter().any(|rr| starts_with(&real_path, rr)) {
            return Err(Error::PolicyViolation {
                tool: "<fs>".into(),
                reason: format!(
                    "path {resolved:?} resolves to {real_path:?}, outside workspace {root:?} (symlink escape?)"
                ),
            });
        }

        Ok(resolved)
    }
}

fn normalize(path: &Path) -> PathBuf {
    // Non-filesystem path normalization: collapse `..` and `.` without
    // resolving symlinks. Good enough for the syntactic fast-path; the
    // symlink-aware check in `PathPolicy::check` follows up with
    // `resolve_real_path`.
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(comp),
        }
    }
    out
}

fn starts_with(candidate: &Path, root: &Path) -> bool {
    candidate
        .components()
        .collect::<Vec<_>>()
        .starts_with(&root.components().collect::<Vec<_>>())
}

/// Walk the path through the filesystem, resolving symlinks. The deepest
/// existing prefix is canonicalized (which follows any symlinks in it);
/// any non-existing tail is appended verbatim. Broken symlinks (whose
/// target is missing) are resolved via `read_link` so an attacker cannot
/// hide an escape behind a not-yet-created target.
fn resolve_real_path(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while std::fs::symlink_metadata(&existing).is_err() {
        match existing.file_name().map(|n| n.to_os_string()) {
            Some(name) => {
                tail.push(name);
                if !existing.pop() {
                    return path.to_path_buf();
                }
            }
            None => return path.to_path_buf(),
        }
    }

    // existing exists (possibly as a broken symlink).
    let canonical = match std::fs::canonicalize(&existing) {
        Ok(c) => c,
        Err(_) => {
            // Broken symlink: read its target manually and resolve it.
            match std::fs::read_link(&existing) {
                Ok(target) => {
                    let composed = if target.is_absolute() {
                        target
                    } else {
                        existing.parent().unwrap_or(Path::new("")).join(target)
                    };
                    normalize(&composed)
                }
                Err(_) => existing.clone(),
            }
        }
    };

    let mut result = canonical;
    for name in tail.iter().rev() {
        result.push(name);
    }
    result
}
