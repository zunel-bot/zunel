use std::path::{Path, PathBuf};

use async_trait::async_trait;
use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{json, Value};

use crate::path_policy::PathPolicy;
use crate::tool::{Tool, ToolContext, ToolResult};

/// Maximum number of grep lines to return before truncation, so the
/// LLM sees a bounded result set.
const MAX_GREP_HITS: usize = 2_000;

fn root(policy: &PathPolicy, ctx: &ToolContext, base: Option<&str>) -> Result<PathBuf, String> {
    let raw = base.unwrap_or(".");
    let abs = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        ctx.workspace.join(raw)
    };
    policy.check(&abs).map_err(|e| e.to_string())
}

pub struct GlobTool {
    policy: PathPolicy,
}

impl GlobTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }
    fn description(&self) -> &'static str {
        "Recursively match file paths against a glob pattern (gitignore-aware)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "Base dir, default '.'"},
            },
            "required": ["pattern"],
        })
    }
    fn concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return ToolResult::err("glob: missing pattern".to_string());
        };
        let base = args.get("path").and_then(Value::as_str);
        let root = match root(&self.policy, ctx, base) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(format!("glob: {e}")),
        };
        let mut gs = GlobSetBuilder::new();
        let glob = match Glob::new(pattern) {
            Ok(g) => g,
            Err(e) => return ToolResult::err(format!("glob: invalid pattern: {e}")),
        };
        gs.add(glob);
        let set = match gs.build() {
            Ok(s) => s,
            Err(e) => return ToolResult::err(format!("glob: {e}")),
        };
        let mut hits = Vec::new();
        let walker = WalkBuilder::new(&root).require_git(false).build();
        for entry in walker.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let rel = p.strip_prefix(&root).unwrap_or(p);
            if set.is_match(rel) {
                hits.push(rel.display().to_string());
            }
        }
        hits.sort();
        ToolResult::ok(hits.join("\n"))
    }
}

pub struct GrepTool {
    policy: PathPolicy,
}

impl GrepTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Recursive regex search of text files, gitignore-aware. Output: path:line:match."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string", "description": "Base dir, default '.'"},
            },
            "required": ["pattern"],
        })
    }
    fn concurrency_safe(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return ToolResult::err("grep: missing pattern".to_string());
        };
        let base = args.get("path").and_then(Value::as_str);
        let root = match root(&self.policy, ctx, base) {
            Ok(p) => p,
            Err(e) => return ToolResult::err(format!("grep: {e}")),
        };
        let re = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return ToolResult::err(format!("grep: invalid regex: {e}")),
        };
        let mut out: Vec<String> = Vec::new();
        let walker = WalkBuilder::new(&root).require_git(false).build();
        'outer: for entry in walker.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let body = match std::fs::read_to_string(p) {
                Ok(b) => b,
                Err(_) => continue,
            };
            for (i, line) in body.lines().enumerate() {
                if re.is_match(line) {
                    let rel = p.strip_prefix(&root).unwrap_or(p);
                    out.push(format!("{}:{}:{}", rel.display(), i + 1, line));
                    if out.len() >= MAX_GREP_HITS {
                        break 'outer;
                    }
                }
            }
        }
        ToolResult::ok(out.join("\n"))
    }
}
