use std::path::PathBuf;

use serde_json::{json, Value};

use zunel_skills::SkillsLoader;

use crate::error::{Error, Result};
use crate::runtime_tag::{CLOSE_TAG, OPEN_TAG};
use crate::templates::{render_identity, render_platform_policy, render_skills_section};

const SECTION_SEPARATOR: &str = "\n\n---\n\n";
const MAX_RECENT_HISTORY: usize = 50;

const BOOTSTRAP_FILES: &[&str] = &["AGENTS.md", "SOUL.md", "USER.md", "TOOLS.md"];

pub struct ContextBuilder {
    workspace: PathBuf,
    skills: SkillsLoader,
    runtime: Option<String>,
}

impl ContextBuilder {
    pub fn new(workspace: PathBuf, skills: SkillsLoader) -> Self {
        Self {
            workspace,
            skills,
            runtime: None,
        }
    }

    /// Override the platform/runtime descriptor that gets baked into the
    /// identity section (e.g. ``"macOS arm64, Rust runtime"``). When
    /// unset, one is synthesized from `std::env::consts`.
    pub fn with_runtime(mut self, runtime: impl Into<String>) -> Self {
        self.runtime = Some(runtime.into());
        self
    }

    pub fn build_system_prompt(&self, channel: Option<&str>) -> Result<String> {
        let mut parts: Vec<String> = Vec::new();

        let runtime = self
            .runtime
            .clone()
            .unwrap_or_else(default_runtime_descriptor);
        let workspace_path = workspace_display(&self.workspace);
        let policy = render_platform_policy(platform_system())?;
        let identity = render_identity(&workspace_path, &runtime, &policy, channel)?;
        parts.push(identity);

        let bootstrap = self.load_bootstrap_files()?;
        if !bootstrap.is_empty() {
            parts.push(bootstrap);
        }

        let always = self.skills.get_always_skills()?;
        if !always.is_empty() {
            let blob = self.skills.load_skills_for_context(&always)?;
            if !blob.is_empty() {
                parts.push(format!("# Active Skills\n\n{blob}"));
            }
        }
        let exclude: std::collections::HashSet<String> = always.into_iter().collect();
        let summary = self.skills.build_skills_summary(Some(&exclude))?;
        if !summary.is_empty() {
            parts.push(render_skills_section(&summary)?);
        }

        Ok(parts.join(SECTION_SEPARATOR))
    }

    fn load_bootstrap_files(&self) -> Result<String> {
        let mut blocks: Vec<String> = Vec::new();
        for name in BOOTSTRAP_FILES {
            let path = self.workspace.join(name);
            if path.exists() {
                let body = std::fs::read_to_string(&path).map_err(Error::from)?;
                blocks.push(format!("## {name}\n\n{body}"));
            }
        }
        Ok(blocks.join("\n\n"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build_messages(
        &self,
        history: &[Value],
        current_message: &str,
        media: Option<&Value>,
        channel: Option<&str>,
        chat_id: Option<&str>,
        current_role: &str,
        session_summary: Option<&str>,
    ) -> Result<Vec<Value>> {
        let mut messages: Vec<Value> = Vec::with_capacity(history.len() + 2);
        let system = self.build_system_prompt(channel)?;
        messages.push(json!({"role": "system", "content": system}));

        let start = history.len().saturating_sub(MAX_RECENT_HISTORY);
        for msg in &history[start..] {
            messages.push(msg.clone());
        }

        let runtime_block = build_runtime_block(channel, chat_id, session_summary);
        let wrapped_content = if runtime_block.is_empty() {
            current_message.to_string()
        } else {
            format!("{runtime_block}{current_message}")
        };

        let merge_with_last = messages
            .last()
            .and_then(|v| v.get("role"))
            .and_then(Value::as_str)
            == Some(current_role);

        let mut current = match (current_role, media) {
            ("user", Some(media)) => json!({
                "role": "user",
                "content": wrapped_content,
                "media": media,
            }),
            (role, _) => json!({"role": role, "content": wrapped_content}),
        };

        if merge_with_last {
            // `merge_with_last` was derived from `messages.last()`, so `pop`
            // always returns `Some` here. Guard explicitly anyway so a future
            // refactor can't silently turn this into a panic.
            if let Some(prev) = messages.pop() {
                let combined = format!(
                    "{}\n\n{}",
                    prev["content"].as_str().unwrap_or_default(),
                    current["content"].as_str().unwrap_or_default()
                );
                current["content"] = Value::String(combined);
            }
        }

        messages.push(current);
        Ok(messages)
    }
}

fn workspace_display(p: &std::path::Path) -> String {
    // Canonicalise when the path exists; otherwise print the path as-is
    // so callers (incl. tests) can pre-canonicalise.
    match std::fs::canonicalize(p) {
        Ok(canon) => canon.display().to_string(),
        Err(_) => p.display().to_string(),
    }
}

fn platform_system() -> &'static str {
    if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "Darwin"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "POSIX"
    }
}

fn default_runtime_descriptor() -> String {
    let arch = std::env::consts::ARCH;
    let os_label = match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    };
    format!("{os_label} {arch}, Rust runtime")
}

fn build_runtime_block(
    channel: Option<&str>,
    chat_id: Option<&str>,
    session_summary: Option<&str>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    lines.push(format!("time: {now}"));
    if let Some(c) = channel {
        lines.push(format!("channel: {c}"));
    }
    if let Some(c) = chat_id {
        lines.push(format!("chat_id: {c}"));
    }
    if let Some(s) = session_summary {
        if !s.is_empty() {
            lines.push(format!("summary: {s}"));
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("{OPEN_TAG}\n{}\n{CLOSE_TAG}\n", lines.join("\n"))
}
