use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};
use crate::schema::AgentDefaults;

/// Env var that disables [`guard_workspace`]. Any non-empty value
/// counts as "yes, I know"; the actual *value* is intentionally
/// not parsed so operators can't accidentally type `0` or `false`
/// and silently still bypass the guard.
pub const UNSAFE_WORKSPACE_ENV: &str = "ZUNEL_ALLOW_UNSAFE_WORKSPACE";

/// Resolve the zunel home directory.
///
/// Precedence:
/// 1. `ZUNEL_HOME` env var (used for tests and custom installs).
/// 2. `$HOME/.zunel` on Unix, platform-appropriate home on other OSes.
pub fn zunel_home() -> Result<PathBuf> {
    if let Some(val) = std::env::var_os("ZUNEL_HOME") {
        return Ok(PathBuf::from(val));
    }
    crate::instance::active_instance_home()
}

/// Default config file path: `<zunel_home>/config.json`.
pub fn default_config_path() -> Result<PathBuf> {
    Ok(zunel_home()?.join("config.json"))
}

/// Default workspace location: `<zunel_home>/workspace/`.
pub fn default_workspace_path() -> Result<PathBuf> {
    Ok(zunel_home()?.join("workspace"))
}

/// Resolve the workspace path from config.
///
/// Precedence:
/// 1. ``agents.defaults.workspace`` if set (with ``~`` expansion).
/// 2. ``<zunel_home>/workspace/``.
pub fn workspace_path(defaults: &AgentDefaults) -> Result<PathBuf> {
    match defaults.workspace.as_deref() {
        Some(raw) => Ok(expand_tilde(raw)),
        None => default_workspace_path(),
    }
}

fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(raw)
}

/// Sessions directory inside a workspace: `<workspace>/sessions/`.
pub fn sessions_dir(workspace: &std::path::Path) -> PathBuf {
    workspace.join("sessions")
}

/// Persistent REPL history file: `<zunel_home>/cli_history.txt`.
pub fn cli_history_path() -> Result<PathBuf> {
    Ok(zunel_home()?.join("cli_history.txt"))
}

/// Refuse to start when the resolved workspace is "obviously" a
/// foot-gun: the filesystem root, the user's `$HOME`, or any
/// ancestor of (or equal to) the resolved zunel home.
///
/// The motivation is simple: the agent loop and its filesystem
/// tools (`write_file`, `edit_file`, `exec`, the various
/// search/read tools) all anchor their `PathPolicy` to the
/// workspace. If that anchor is `/` or `$HOME`, "stay inside the
/// workspace" stops being a meaningful sandbox — a renamed file
/// or a stray `..` could clobber `~/.ssh/`, `~/.aws/credentials`,
/// or zunel's own state.
///
/// Triggers (each is checked, the first match wins):
///
/// 1. `workspace == "/"` — the filesystem root.
/// 2. `workspace == $HOME` — the current user's home.
/// 3. `workspace` is an ancestor of (or equal to) the resolved
///    zunel home. This catches the "I pointed workspace at
///    `~/.zunel` itself" mistake and the broader case where
///    workspace contains the sessions/config tree (which would
///    let an agent self-modify its own instance).
///
/// Escape hatch: setting [`UNSAFE_WORKSPACE_ENV`] to any
/// non-empty value short-circuits the guard. The CLI exposes the
/// same knob as `zunel --i-know-what-im-doing` for ergonomics, so
/// operators don't have to learn the env var name.
///
/// Tests live in `tests/workspace_path_test.rs` and use
/// `serial_test::serial(zunel_home_env)` because the implementation
/// reads `ZUNEL_HOME` and `HOME` from the process environment.
pub fn guard_workspace(workspace: &Path) -> Result<()> {
    if let Some(val) = std::env::var_os(UNSAFE_WORKSPACE_ENV) {
        if !val.is_empty() {
            return Ok(());
        }
    }

    let normalized = normalize(workspace);

    if is_filesystem_root(&normalized) {
        return Err(Error::UnsafeWorkspace {
            path: workspace.to_path_buf(),
            reason: "this is the filesystem root, which would let \
                    workspace-relative tools mutate any path on \
                    the system"
                .to_string(),
        });
    }

    if let Some(home) = dirs::home_dir() {
        let home_norm = normalize(&home);
        if !home_norm.as_os_str().is_empty() && normalized == home_norm {
            return Err(Error::UnsafeWorkspace {
                path: workspace.to_path_buf(),
                reason: "this is the user's home directory; \
                        workspace-relative writes could overwrite \
                        ~/.ssh, ~/.aws, or other sensitive state"
                    .to_string(),
            });
        }
    }

    if let Ok(home) = zunel_home() {
        let home_norm = normalize(&home);
        if !home_norm.as_os_str().is_empty() && path_contains(&normalized, &home_norm) {
            return Err(Error::UnsafeWorkspace {
                path: workspace.to_path_buf(),
                reason: format!(
                    "this contains the zunel runtime home ({}), so \
                     the agent loop could mutate its own config, \
                     sessions, or token cache",
                    home.display()
                ),
            });
        }
    }

    Ok(())
}

/// Resolve the workspace path *and* run the foot-gun guard.
///
/// Convenience wrapper: most CLI entry points call
/// [`workspace_path`] and immediately want to verify the result is
/// safe before doing anything destructive. Equivalent to:
///
/// ```ignore
/// let ws = workspace_path(defaults)?;
/// guard_workspace(&ws)?;
/// Ok(ws)
/// ```
pub fn workspace_path_safe(defaults: &AgentDefaults) -> Result<PathBuf> {
    let path = workspace_path(defaults)?;
    guard_workspace(&path)?;
    Ok(path)
}

fn normalize(path: &Path) -> PathBuf {
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

fn is_filesystem_root(path: &Path) -> bool {
    let mut comps = path.components();
    matches!(comps.next(), Some(Component::RootDir)) && comps.next().is_none()
}

/// Returns `true` when `outer` equals `inner` or is a strict
/// ancestor of it (i.e. `inner` lives inside `outer`). Both paths
/// must already be [`normalize`]d. We compare by component to avoid
/// the `Path::starts_with` pitfall where `"/foo"` is *not* a prefix
/// of `"/foobar"` even though their string forms suggest otherwise.
fn path_contains(outer: &Path, inner: &Path) -> bool {
    let outer_comps: Vec<_> = outer.components().collect();
    let inner_comps: Vec<_> = inner.components().collect();
    if outer_comps.len() > inner_comps.len() {
        return false;
    }
    outer_comps
        .iter()
        .zip(inner_comps.iter())
        .all(|(a, b)| a == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_filesystem_root_only_matches_bare_slash() {
        assert!(is_filesystem_root(Path::new("/")));
        assert!(!is_filesystem_root(Path::new("/foo")));
        assert!(!is_filesystem_root(Path::new("foo")));
        assert!(!is_filesystem_root(Path::new("")));
    }

    #[test]
    fn path_contains_matches_equal_paths() {
        assert!(path_contains(Path::new("/a/b"), Path::new("/a/b")));
    }

    #[test]
    fn path_contains_matches_strict_ancestors() {
        assert!(path_contains(Path::new("/a"), Path::new("/a/b")));
        assert!(path_contains(Path::new("/"), Path::new("/a")));
    }

    #[test]
    fn path_contains_rejects_sibling_prefixes() {
        // The classic Path::starts_with footgun: "/foo" is not an
        // ancestor of "/foobar" even though the strings share a
        // prefix.
        assert!(!path_contains(Path::new("/foo"), Path::new("/foobar")));
    }

    #[test]
    fn path_contains_rejects_unrelated_paths() {
        assert!(!path_contains(Path::new("/a/b"), Path::new("/a")));
        assert!(!path_contains(Path::new("/a"), Path::new("/b")));
    }

    #[test]
    fn normalize_collapses_curdir_and_parentdir() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
    }
}
