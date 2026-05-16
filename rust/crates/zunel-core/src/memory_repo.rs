//! Git-backed durable-memory store.
//!
//! Wraps `<workspace>/memory/` (the directory holding `MEMORY.md`,
//! `history.jsonl`, and Dream's bookkeeping files) in a local git
//! repo so every Dream consolidation pass becomes an inspectable,
//! revertible commit. Users get the implicit safety net the docs
//! have always promised: "Dream changed something I didn't want →
//! `/dream-restore <sha>`."
//!
//! Implementation note: shells out to the `git` binary rather than
//! linking libgit2/gix. Git is already a hard requirement for
//! anyone who built or cloned zunel, and the operations here happen
//! at human cadence (one commit per Dream pass, a few `log` /
//! `reset` calls when users debug). The CLI overhead is irrelevant
//! at this rate and saves a chunky native dependency.
//!
//! All operations are idempotent and best-effort: if `git` isn't on
//! `PATH`, commits are skipped with a `tracing::warn!` rather than
//! failing the Dream pass. The audit-mandated "Versioned Memory"
//! claim becomes real where it can, and silently degrades where it
//! can't.

use std::path::{Path, PathBuf};
use std::process::Command;

/// One commit in the dream-memory history.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct DreamCommit {
    /// Full 40-char hex sha.
    pub sha: String,
    /// Author date in ISO 8601.
    pub date: String,
    /// First subject line of the commit message.
    pub subject: String,
}

/// Live handle to `<workspace>/memory/.git`. Cheap to construct;
/// every method shells out to the `git` binary so two concurrent
/// handles are safe (git's own lockfile serialises them).
pub struct DreamMemoryRepo {
    memory_dir: PathBuf,
}

impl DreamMemoryRepo {
    pub fn new(workspace: &Path) -> Self {
        Self {
            memory_dir: workspace.join("memory"),
        }
    }

    pub fn memory_dir(&self) -> &Path {
        &self.memory_dir
    }

    /// True when `<workspace>/memory/.git/` exists. Used by callers
    /// that want to decide "should I init?" without invoking git.
    pub fn is_initialised(&self) -> bool {
        self.memory_dir.join(".git").exists()
    }

    /// Initialise the repo if it does not exist. Sets a stable
    /// committer identity on the repo so the global git config
    /// can't accidentally tag Dream commits with the user's
    /// personal name + email. Idempotent.
    pub fn ensure_initialised(&self) -> std::io::Result<()> {
        if !self.memory_dir.exists() {
            std::fs::create_dir_all(&self.memory_dir)?;
        }
        if self.is_initialised() {
            return Ok(());
        }
        run_git(
            &self.memory_dir,
            &["init", "--quiet", "--initial-branch=main"],
        )?;
        // Pin the repo-local identity so commit logs are always
        // attributable to Dream regardless of the user's global
        // config. Both name and email must be set or git refuses
        // to commit.
        run_git(&self.memory_dir, &["config", "user.name", "Zunel Dream"])?;
        run_git(
            &self.memory_dir,
            &["config", "user.email", "dream@zunel.local"],
        )?;
        // Disable gpg signing in case the user's global config has
        // `commit.gpgsign = true` — we don't want Dream commits to
        // require an interactive key prompt.
        run_git(&self.memory_dir, &["config", "commit.gpgsign", "false"])?;
        Ok(())
    }

    /// `git add -A && git commit -m <subject>` against the memory
    /// dir. Returns the new commit's sha on success, `Ok(None)`
    /// when there was nothing to commit (Dream made no edits, or
    /// the edits matched the existing state). Errors are surfaced
    /// so the caller can choose to swallow them — Dream callers
    /// typically log and continue rather than failing the pass.
    pub fn commit_all(&self, subject: &str) -> std::io::Result<Option<String>> {
        self.ensure_initialised()?;
        run_git(&self.memory_dir, &["add", "-A"])?;
        // Detect "nothing to commit" via porcelain status: if the
        // index has no staged changes, skip the commit step
        // entirely (otherwise `git commit` returns exit code 1).
        let status = git_output(&self.memory_dir, &["status", "--porcelain"])?;
        if status.trim().is_empty() {
            return Ok(None);
        }
        run_git(
            &self.memory_dir,
            &["commit", "--quiet", "--allow-empty-message", "-m", subject],
        )?;
        let sha = git_output(&self.memory_dir, &["rev-parse", "HEAD"])?
            .trim()
            .to_string();
        Ok(Some(sha))
    }

    /// Return the most recent `limit` commits, newest first. Empty
    /// when the repo has no commits yet (or doesn't exist).
    pub fn log(&self, limit: usize) -> std::io::Result<Vec<DreamCommit>> {
        if !self.is_initialised() {
            return Ok(Vec::new());
        }
        let limit_str = limit.to_string();
        // Use a literal control character as the field separator so
        // we never collide with anything in the commit subject. `git
        // log -z` then null-terminates each commit record.
        let format = "%H\x1f%aI\x1f%s";
        let out = git_output(
            &self.memory_dir,
            &[
                "log",
                "-z",
                &format!("--pretty=format:{format}"),
                "-n",
                &limit_str,
            ],
        )?;
        let mut commits = Vec::new();
        for rec in out.split('\0') {
            if rec.is_empty() {
                continue;
            }
            let mut parts = rec.split('\x1f');
            let sha = parts.next().unwrap_or_default().trim().to_string();
            let date = parts.next().unwrap_or_default().trim().to_string();
            let subject = parts.next().unwrap_or_default().trim().to_string();
            if sha.is_empty() {
                continue;
            }
            commits.push(DreamCommit { sha, date, subject });
        }
        Ok(commits)
    }

    /// Restore the memory directory to the state it was in **before**
    /// `target_sha` landed. Concretely: `git reset --hard <target>~1`.
    /// Returns the new HEAD sha. Errors are surfaced — the audit
    /// considers this destructive enough that callers should always
    /// confirm with the user before invoking.
    pub fn restore_before(&self, target_sha: &str) -> std::io::Result<String> {
        if !self.is_initialised() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "dream memory repo is not initialised; nothing to restore",
            ));
        }
        // Validate the sha exists and isn't, e.g., a hostile string.
        // `git rev-parse --verify` returns non-zero on bad refs.
        let resolved = git_output(
            &self.memory_dir,
            &["rev-parse", "--verify", &format!("{target_sha}^{{commit}}")],
        )?;
        let resolved = resolved.trim().to_string();
        if resolved.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("could not resolve sha: {target_sha}"),
            ));
        }
        let parent = format!("{resolved}~1");
        // Fail clearly if target is the root commit (no parent to reset to).
        if git_output(&self.memory_dir, &["rev-parse", "--verify", &parent]).is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("commit {resolved} is the root commit; nothing to restore to"),
            ));
        }
        run_git(&self.memory_dir, &["reset", "--hard", "--quiet", &parent])?;
        let new_head = git_output(&self.memory_dir, &["rev-parse", "HEAD"])?
            .trim()
            .to_string();
        Ok(new_head)
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> std::io::Result<()> {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn git_output(cwd: &Path, args: &[&str]) -> std::io::Result<String> {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn commit_then_log_round_trips() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        let repo = DreamMemoryRepo::new(&workspace);

        std::fs::write(workspace.join("memory").join("MEMORY.md"), "- first note").unwrap();
        let first = repo.commit_all("dream pass 1").unwrap();
        assert!(first.is_some(), "first commit should land");

        std::fs::write(workspace.join("memory").join("MEMORY.md"), "- updated note").unwrap();
        let second = repo.commit_all("dream pass 2").unwrap();
        assert!(second.is_some(), "second commit should land");
        assert_ne!(first, second, "shas must differ");

        let log = repo.log(10).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].subject, "dream pass 2");
        assert_eq!(log[1].subject, "dream pass 1");
        // Date strings are ISO 8601 — at minimum they should have a `T`.
        assert!(log[0].date.contains('T'), "got: {}", log[0].date);
    }

    #[test]
    fn commit_skipped_when_nothing_changed() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        let repo = DreamMemoryRepo::new(&workspace);
        std::fs::write(workspace.join("memory").join("MEMORY.md"), "- first").unwrap();
        repo.commit_all("first").unwrap();

        // No changes since last commit → commit_all must return None,
        // not a confusing exit-code error.
        let res = repo.commit_all("noop").unwrap();
        assert_eq!(res, None);
    }

    #[test]
    fn restore_before_target_rolls_back_one_commit() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        let repo = DreamMemoryRepo::new(&workspace);

        std::fs::write(workspace.join("memory").join("MEMORY.md"), "- original").unwrap();
        repo.commit_all("dream pass 1").unwrap();
        std::fs::write(workspace.join("memory").join("MEMORY.md"), "- bad rewrite").unwrap();
        let bad = repo.commit_all("dream pass 2").unwrap().unwrap();

        repo.restore_before(&bad).unwrap();
        let body = std::fs::read_to_string(workspace.join("memory").join("MEMORY.md")).unwrap();
        assert_eq!(body, "- original");
    }

    #[test]
    fn restore_root_commit_returns_clear_error() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().to_path_buf();
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        let repo = DreamMemoryRepo::new(&workspace);
        std::fs::write(workspace.join("memory").join("MEMORY.md"), "- only").unwrap();
        let root = repo.commit_all("first").unwrap().unwrap();
        let err = repo.restore_before(&root).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("root commit"),
            "unexpected error message: {msg}"
        );
    }
}
