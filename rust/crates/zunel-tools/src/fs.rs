use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::path_policy::PathPolicy;
use crate::tool::{Tool, ToolContext, ToolResult};

fn resolve_path(policy: &PathPolicy, ctx: &ToolContext, raw: &str) -> Result<PathBuf, String> {
    let as_path = Path::new(raw);
    let abs: PathBuf = if as_path.is_absolute() {
        as_path.to_path_buf()
    } else {
        ctx.workspace.join(as_path)
    };
    policy.check(&abs).map_err(|e| e.to_string())
}

pub struct ReadFileTool {
    policy: PathPolicy,
}

impl ReadFileTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }
    fn description(&self) -> &'static str {
        "Read a text file from the workspace. Returns contents with optional offset/limit line pagination."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Workspace-relative or absolute path."},
                "offset": {"type": "integer", "description": "Zero-based first line to include.", "default": 0},
                "limit": {"type": "integer", "description": "Max lines to include.", "default": 2000},
            },
            "required": ["path"],
        })
    }
    fn concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let raw = match args.get("path").and_then(Value::as_str) {
            Some(s) => s,
            None => return ToolResult::err("read_file: missing path".to_string()),
        };
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(2000) as usize;
        let path = match resolve_path(&self.policy, ctx, raw) {
            Ok(p) => p,
            Err(msg) => return ToolResult::err(format!("read_file: {msg}")),
        };
        // Stream line-by-line and stop after `offset + limit` lines.
        // The previous `tokio::fs::read_to_string` slurped the whole
        // file before slicing — a 50 GB single-line file would OOM the
        // gateway even though the model only asked for 2000 lines.
        use tokio::io::{AsyncBufReadExt, BufReader};
        let file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => return ToolResult::err(format!("read_file: {e} ({path:?})")),
        };
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut out = String::new();
        let mut emitted: usize = 0;
        let mut current: usize = 0;
        let mut any_content = false;
        loop {
            line.clear();
            let n = match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => return ToolResult::err(format!("read_file: {e} ({path:?})")),
            };
            any_content = any_content || n > 0;
            if current < offset {
                current += 1;
                continue;
            }
            if emitted >= limit {
                break;
            }
            // `read_line` keeps the trailing '\n'; the original
            // `lines().join("\n")` did not. Keep parity: strip trailing
            // newline pieces, then re-join with `\n` ourselves.
            let trimmed = line.strip_suffix('\n').unwrap_or(&line);
            let trimmed = trimmed.strip_suffix('\r').unwrap_or(trimmed);
            if emitted > 0 {
                out.push('\n');
            }
            out.push_str(trimmed);
            emitted += 1;
            current += 1;
        }
        if !out.ends_with('\n') && any_content {
            out.push('\n');
        }
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            if let Ok(mtime) = meta.modified() {
                ctx.file_state.mark_read(path.clone(), mtime);
            }
        }
        ToolResult::ok(out)
    }
}

pub struct WriteFileTool {
    policy: PathPolicy,
}

impl WriteFileTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }
    fn description(&self) -> &'static str {
        "Create or overwrite a workspace file with the given UTF-8 contents."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"},
            },
            "required": ["path", "content"],
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let raw = match args.get("path").and_then(Value::as_str) {
            Some(s) => s,
            None => return ToolResult::err("write_file: missing path".to_string()),
        };
        let content = args.get("content").and_then(Value::as_str).unwrap_or("");
        let path = match resolve_path(&self.policy, ctx, raw) {
            Ok(p) => p,
            Err(msg) => return ToolResult::err(format!("write_file: {msg}")),
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolResult::err(format!("write_file: mkdir {parent:?}: {e}"));
            }
        }
        match tokio::fs::write(&path, content).await {
            Ok(()) => {
                ctx.file_state.invalidate(&path);
                ToolResult::ok(format!(
                    "wrote {} bytes to {}",
                    content.len(),
                    path.display()
                ))
            }
            Err(e) => ToolResult::err(format!("write_file: {e}")),
        }
    }
}

pub struct EditFileTool {
    policy: PathPolicy,
}

impl EditFileTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
    }
    fn description(&self) -> &'static str {
        "Replace `old` with `new` in a previously-read workspace file. `old` must occur exactly once."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old": {"type": "string"},
                "new": {"type": "string"},
            },
            "required": ["path", "old", "new"],
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let Some(raw) = args.get("path").and_then(Value::as_str) else {
            return ToolResult::err("edit_file: missing path".to_string());
        };
        let Some(old) = args.get("old").and_then(Value::as_str) else {
            return ToolResult::err("edit_file: missing old".to_string());
        };
        let Some(new) = args.get("new").and_then(Value::as_str) else {
            return ToolResult::err("edit_file: missing new".to_string());
        };
        let path = match resolve_path(&self.policy, ctx, raw) {
            Ok(p) => p,
            Err(msg) => return ToolResult::err(format!("edit_file: {msg}")),
        };
        let prior = ctx.file_state.last_read(&path);
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) => return ToolResult::err(format!("edit_file: {e}")),
        };
        let current_mtime = meta.modified().ok();
        if prior.is_none() || prior != current_mtime {
            return ToolResult::err(format!(
                "edit_file: read_file {raw} first (stale or never-read state)"
            ));
        }
        let body = match tokio::fs::read_to_string(&path).await {
            Ok(b) => b,
            Err(e) => return ToolResult::err(format!("edit_file: {e}")),
        };
        let matches = body.matches(old).count();
        if matches == 0 {
            return ToolResult::err(format!("edit_file: old string not found in {raw}"));
        }
        if matches > 1 {
            return ToolResult::err(format!(
                "edit_file: old string matched {matches} times (multiple) in {raw}; include more surrounding context"
            ));
        }
        let replaced = body.replacen(old, new, 1);
        if let Err(e) = tokio::fs::write(&path, &replaced).await {
            return ToolResult::err(format!("edit_file: {e}"));
        }
        ctx.file_state.invalidate(&path);
        ToolResult::ok(format!("edited {}", path.display()))
    }
}

pub struct ListDirTool {
    policy: PathPolicy,
}

impl ListDirTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &'static str {
        "list_dir"
    }
    fn description(&self) -> &'static str {
        "List files and sub-directories in a workspace directory."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Dir to list (default '.')"},
            },
            "required": [],
        })
    }
    fn concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let path = match resolve_path(&self.policy, ctx, raw) {
            Ok(p) => p,
            Err(msg) => return ToolResult::err(format!("list_dir: {msg}")),
        };
        let mut entries = match tokio::fs::read_dir(&path).await {
            Ok(e) => e,
            Err(err) => return ToolResult::err(format!("list_dir: {err} ({path:?})")),
        };
        let mut names: Vec<String> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let is_dir = entry.file_type().await.map(|f| f.is_dir()).unwrap_or(false);
            let name = entry.file_name().to_string_lossy().to_string();
            if is_dir {
                names.push(format!("{name}/"));
            } else {
                names.push(name);
            }
        }
        names.sort();
        ToolResult::ok(names.join("\n"))
    }
}
