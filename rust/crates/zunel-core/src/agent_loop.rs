use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, RwLock};

use serde_json::Value;
use tokio::sync::mpsc;
use zunel_bus::{MessageBus, MessageKind, OutboundMessage};
use zunel_config::{AgentDefaults, DEFAULT_SESSION_HISTORY_WINDOW};
use zunel_providers::{ChatMessage, GenerationSettings, LLMProvider, Role, StreamEvent, Usage};
use zunel_skills::SkillsLoader;
use zunel_tokens::estimate_message_tokens;

use crate::approval::{
    AllowAllApprovalHandler, ApprovalHandler, ApprovalScope, BusApprovalHandler,
};
use crate::compaction::CompactionService;
use crate::default_tools::{reload_mcp_servers, ReloadReport};
use crate::document::extract_documents;
use crate::error::Result;
use crate::memory::{DreamOutcome, DreamService, MemoryStore};
use crate::memory_repo::{DreamCommit, DreamMemoryRepo};
use crate::runner::{AgentRunSpec, AgentRunner, TrimBudgets};
use crate::session::{ChatRole, Session, SessionManager};
use crate::trim::chat_message_to_value;
use zunel_tools::ToolRegistry;

/// Shared, hot-swappable tool registry handle.
///
/// The inner `Arc<ToolRegistry>` is the snapshot every turn sees; the
/// outer `RwLock` is only held briefly during reload to atomic-swap
/// the inner Arc via `Arc::make_mut` (clone-on-write). Readers take
/// a read lock just long enough to bump the inner Arc, so the agent
/// loop's per-turn snapshot is an Arc count bump rather than a full
/// `BTreeMap<String, Arc<dyn Tool>>` rebuild.
pub type SharedToolRegistry = Arc<RwLock<Arc<ToolRegistry>>>;

/// Outcome of a single agent turn.
///
/// - `content`: the assistant text the provider produced (possibly empty
///   if the model returned no text, e.g. a pure tool-call turn).
/// - `tools_used`: names of tools the agent invoked during the turn.
/// - `messages`: the ordered message history that was sent to the
///   provider for this turn (user-only for `process_direct`, the
///   truncated session history for `process_streamed`).
#[derive(Debug, Clone)]
pub struct RunResult {
    pub content: String,
    pub tools_used: Vec<String>,
    pub messages: Vec<ChatMessage>,
    /// Sum of provider [`Usage`] across every iteration of this turn
    /// (input/output/reasoning/cached). Always populated; defaults to
    /// `Usage::default()` when the provider did not report any usage.
    pub usage: Usage,
    /// Lifetime token total for this session **after** the just-completed
    /// turn was recorded. Read from `Session.metadata.usage_total`.
    /// Identical to `usage` for the very first turn.
    pub session_total_usage: Usage,
}

/// Agent loop. Exposes `process_direct` (one-shot, stateless) and
/// `process_streamed` (uses a `SessionManager` for persistent
/// conversation history and streams deltas to the caller).
///
/// Concurrency note: `SessionManager` uses atomic temp-file-+-rename
/// writes (with `fsync` before the rename so a crashed process can't
/// publish a torn file) and is safe for concurrent reads. Two
/// simultaneous writes to the same session, however, race on
/// last-writer-wins semantics — there is no cross-process lock. The
/// REPL is inherently sequential; the Slack gateway funnels every
/// turn through a single async task per session-key so it doesn't
/// hit this either, but two unrelated zunel processes pointed at the
/// same workspace would.
pub struct AgentLoop {
    provider: Arc<dyn LLMProvider>,
    defaults: AgentDefaults,
    sessions: Option<Arc<SessionManager>>,
    tools: SharedToolRegistry,
    approval: Arc<dyn ApprovalHandler>,
    approval_required: bool,
    approval_scope: ApprovalScope,
    workspace: std::path::PathBuf,
    /// Optional skills loader. When set, every turn through
    /// [`process_streamed`] / [`process_inbound_once`] gets a single
    /// `system` message prepended that contains always-on skill bodies
    /// plus the on-demand skills summary. The system message is
    /// regenerated from the loader on each turn — never persisted into
    /// the session — so updates on disk (or upgrades that swap an
    /// embedded builtin) take effect on the next turn.
    skills: Option<Arc<SkillsLoader>>,
    /// Per-loop operator persona prepended ahead of the skills system
    /// message every turn. Mode 2's `helper_ask` uses this to honour
    /// the caller's `system_prompt` arg without polluting the
    /// helper's persisted session log: like the skills system
    /// message, the value is reapplied on every turn from the live
    /// builder, never written to disk.
    extra_system_message: Option<String>,
    /// Per-loop cancellation token. Mode 2's `helper_ask` swaps in a
    /// fresh token registered with the dispatcher's [`CancelRegistry`]
    /// so a `notifications/cancelled` from the hub interrupts the
    /// helper's loop. Defaults to a never-cancelled token, which
    /// matches the legacy "loops don't honour cancellation" behaviour
    /// for callers that didn't opt in.
    cancel: tokio_util::sync::CancellationToken,
    /// When `true`, [`process_inbound_once`] appends a one-line token
    /// footer to the outbound message before publishing it on the bus.
    /// Wired from `channels.showTokenFooter` so it follows the same
    /// per-deployment opt-in as everything else channel-related.
    show_token_footer: bool,
    /// Optional Stage 1 wire-up. When set, every successful idle
    /// compaction appends the produced summary to
    /// `<workspace>/memory/history.jsonl` so the gateway's Dream
    /// service has fresh input to consolidate into `MEMORY.md` /
    /// `SOUL.md` / `USER.md`. Without this handle the compaction
    /// summary lives only inside the session JSONL and Dream sees
    /// nothing — see `docs/memory.md` Stage 1.
    memory_store: Option<Arc<MemoryStore>>,
    /// Shared iteration counter. The runner publishes its 1-indexed
    /// iteration into this on every loop body; the `self` tool's
    /// `RuntimeSelfStateProvider` reads from the same `Arc` so it
    /// reports a live count instead of the historical hard-coded 0.
    iteration_counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl AgentLoop {
    /// How many of the most recent unconsolidated session messages to
    /// replay on each turn. Wired from
    /// `agents.defaults.session_history_window` with a safe default
    /// (`DEFAULT_SESSION_HISTORY_WINDOW`).
    fn history_window(&self) -> usize {
        self.defaults
            .session_history_window
            .unwrap_or(DEFAULT_SESSION_HISTORY_WINDOW)
            .max(1)
    }

    fn trim_budgets(&self) -> TrimBudgets {
        TrimBudgets::from_defaults(&self.defaults)
    }

    /// Returns the configured idle-compaction threshold in minutes, or
    /// `None` when compaction is disabled (`0` or unset).
    fn idle_compact_threshold_minutes(&self) -> Option<u32> {
        self.defaults.idle_compact_after_minutes.filter(|m| *m > 0)
    }
}

impl AgentLoop {
    /// Stateless constructor — no session persistence.
    pub fn new(provider: Arc<dyn LLMProvider>, defaults: AgentDefaults) -> Self {
        Self {
            provider,
            defaults,
            sessions: None,
            tools: Arc::new(RwLock::new(Arc::new(ToolRegistry::new()))),
            approval: Arc::new(AllowAllApprovalHandler),
            approval_required: false,
            approval_scope: ApprovalScope::default(),
            workspace: std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()),
            skills: None,
            extra_system_message: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            show_token_footer: false,
            memory_store: None,
            iteration_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Session-aware constructor — sessions persist to `<workspace>/sessions/`.
    pub fn with_sessions(
        provider: Arc<dyn LLMProvider>,
        defaults: AgentDefaults,
        sessions: SessionManager,
    ) -> Self {
        Self {
            provider,
            defaults,
            sessions: Some(Arc::new(sessions)),
            tools: Arc::new(RwLock::new(Arc::new(ToolRegistry::new()))),
            approval: Arc::new(AllowAllApprovalHandler),
            approval_required: false,
            approval_scope: ApprovalScope::default(),
            workspace: std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()),
            skills: None,
            extra_system_message: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            show_token_footer: false,
            memory_store: None,
            iteration_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Inject a tool registry + approval handler. Wraps the supplied
    /// registry in a fresh `Arc<RwLock<...>>`. Callers that need to
    /// share the live registry handle with another consumer (e.g. the
    /// `mcp_reconnect` native tool) should use
    /// [`AgentLoop::with_tools_arc`] instead.
    pub fn with_tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = Arc::new(RwLock::new(Arc::new(tools)));
        self
    }

    /// Inject a pre-built shared registry handle. Use this when the
    /// caller (CLI / gateway) already shares the registry with another
    /// component such as the `mcp_reconnect` tool that needs to splice
    /// MCP entries in/out at runtime.
    pub fn with_tools_arc(mut self, tools: SharedToolRegistry) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_approval(mut self, approval: Arc<dyn ApprovalHandler>) -> Self {
        self.approval = approval;
        self
    }

    pub fn with_approval_required(mut self, required: bool) -> Self {
        self.approval_required = required;
        self
    }

    pub fn with_approval_scope(mut self, scope: ApprovalScope) -> Self {
        self.approval_scope = scope;
        self
    }

    pub fn with_workspace(mut self, workspace: std::path::PathBuf) -> Self {
        self.workspace = workspace;
        self
    }

    /// Inject a [`SkillsLoader`] so every persisted turn gets a single
    /// `system` message prepended with the always-on skill bodies plus
    /// the on-demand skills summary. Without this call, the agent runs
    /// with no system message (existing pre-v0.2.8 behavior).
    pub fn with_skills(mut self, skills: SkillsLoader) -> Self {
        self.skills = Some(Arc::new(skills));
        self
    }

    /// Inject a per-call operator persona that gets prepended ahead of
    /// the skills system message every turn. Mode 2's `helper_ask`
    /// uses this to honour the caller's `system_prompt` arg without
    /// polluting the helper's persisted session log: like
    /// [`build_skills_system_message`], the value is reapplied on
    /// every turn from the live builder, never written to disk.
    ///
    /// `Some("")` is normalised to `None` so callers can blindly
    /// forward whatever string came off the wire.
    pub fn with_extra_system_message(mut self, msg: Option<String>) -> Self {
        self.extra_system_message = msg.filter(|s| !s.is_empty());
        self
    }

    /// Inject a per-loop cancellation token. Mode 2's `helper_ask`
    /// uses this to register the helper's loop under the inbound
    /// JSON-RPC id so a `notifications/cancelled` from the hub can
    /// interrupt mid-turn. The token is forwarded to every
    /// [`ToolContext`] the loop hands to its tools so individual
    /// tool runs honour the same cancellation.
    ///
    /// Defaults to a never-cancelled token; legacy callers that
    /// don't opt in see no behavioural change.
    pub fn with_cancel(mut self, cancel: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Toggle the per-message token-usage footer on the gateway path.
    /// Has no effect on `process_direct` / `process_streamed`, which
    /// just return `RunResult.usage` for the caller to decide what to
    /// render.
    pub fn with_show_token_footer(mut self, enabled: bool) -> Self {
        self.show_token_footer = enabled;
        self
    }

    /// Wire Stage 1: every successful idle compaction appends the
    /// produced summary to `<workspace>/memory/history.jsonl` so the
    /// gateway's Dream service has input to consolidate. Without this
    /// the compaction summary lives only in the session JSONL and
    /// Dream silently no-ops on every tick.
    pub fn with_memory_store(mut self, store: MemoryStore) -> Self {
        self.memory_store = Some(Arc::new(store));
        self
    }

    /// Clone the shared iteration counter so the CLI/gateway setup
    /// can hand it to `RuntimeSelfStateProvider` — the same `Arc`
    /// that the per-turn `AgentRunner` writes into. Without this
    /// hookup, the `self` tool keeps reporting `iterations: 0/<max>`
    /// even mid-turn (the historical bug).
    pub fn iteration_counter(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        self.iteration_counter.clone()
    }

    /// Read-only snapshot of the live tool registry. Returns an
    /// `Arc<ToolRegistry>` so callers can keep the snapshot around
    /// without holding the lock; a concurrent reload publishes a new
    /// Arc in the inner cell while the caller's existing Arc remains
    /// valid until dropped. Auto-derefs through `Arc<ToolRegistry>`
    /// to `&ToolRegistry`, so `agent.tools().get("foo")` and
    /// `agent.tools().names()` keep working unchanged.
    pub fn tools(&self) -> Arc<ToolRegistry> {
        self.tools
            .read()
            .expect("zunel tool registry lock poisoned")
            .clone()
    }

    /// Clone the shared registry handle so a side component (notably
    /// the `mcp_reconnect` native tool) can read or mutate the same
    /// `ToolRegistry` the agent loop reads from on each turn.
    pub fn tools_handle(&self) -> SharedToolRegistry {
        Arc::clone(&self.tools)
    }

    pub fn register_tool(&mut self, tool: Arc<dyn zunel_tools::Tool>) {
        let mut guard = self
            .tools
            .write()
            .expect("zunel tool registry lock poisoned");
        Arc::make_mut(&mut *guard).register(tool);
    }

    /// Drop a tool by name. Returns `true` if a matching tool was
    /// removed, `false` if no tool with that name was registered.
    /// Subsequent turns no longer see the tool in the function-call
    /// schema sent to the provider.
    pub fn unregister_tool(&mut self, name: &str) -> bool {
        let mut guard = self
            .tools
            .write()
            .expect("zunel tool registry lock poisoned");
        Arc::make_mut(&mut *guard).unregister(name).is_some()
    }

    /// Return the most recent `limit` Dream-memory commits from
    /// `<workspace>/memory/.git/`. Empty when no Dream pass has
    /// ever committed (first launch, or git unavailable).
    pub fn dream_log(&self, limit: usize) -> Result<Vec<DreamCommit>> {
        let repo = DreamMemoryRepo::new(&self.workspace);
        repo.log(limit)
            .map_err(|e| crate::error::Error::Other(format!("dream_log: {e}")))
    }

    /// Restore the memory dir to the state it had *before* the
    /// commit at `sha` landed. Returns the new HEAD sha. Errors
    /// surface to the caller — this is destructive enough that the
    /// REPL prompts for confirmation before invoking.
    pub fn dream_restore(&self, sha: &str) -> Result<String> {
        let repo = DreamMemoryRepo::new(&self.workspace);
        repo.restore_before(sha)
            .map_err(|e| crate::error::Error::Other(format!("dream_restore: {e}")))
    }

    /// Run a Dream consolidation pass now using this loop's configured
    /// provider, model, memory store, and `DreamConfig`. Returns
    /// `Err` if no memory store is attached (Stage 1 not wired). The
    /// `/dream` slash command and a future native `zunel_dream_run`
    /// tool both call into this so they share a single canonical
    /// execution path.
    pub async fn run_dream(&self) -> Result<DreamOutcome> {
        let store = self
            .memory_store
            .as_ref()
            .ok_or_else(|| {
                crate::error::Error::Other(
                    "Dream is not wired: AgentLoop has no MemoryStore attached".into(),
                )
            })?
            .as_ref()
            .clone();
        let svc = DreamService::new(store, self.provider.clone(), self.defaults.model.clone())
            .with_config(&self.defaults.dream);
        svc.run().await
    }

    /// Reload MCP servers from disk and splice the freshly listed
    /// tools into the live registry. `target = None` reloads every
    /// configured server (matching boot-time `register_mcp_tools`
    /// behavior); `target = Some(name)` reloads a single one.
    /// `config_path = None` reads `<zunel_home>/config.json`. The
    /// caller is responsible for surfacing the [`ReloadReport`] to
    /// the user.
    ///
    /// Network I/O happens off-lock — the live registry is only
    /// briefly write-locked at the end to atomically swap entries —
    /// so concurrent turns never block on reload.
    pub async fn reload_mcp(
        &self,
        target: Option<&str>,
        config_path: Option<&Path>,
    ) -> std::result::Result<ReloadReport, zunel_config::Error> {
        let cfg = zunel_config::load_config(config_path)?;
        Ok(reload_mcp_servers(&self.tools, &cfg, target).await)
    }

    fn settings(&self) -> GenerationSettings {
        GenerationSettings {
            temperature: self.defaults.temperature,
            max_tokens: self.defaults.max_tokens,
            reasoning_effort: self.defaults.reasoning_effort.clone(),
        }
    }

    /// Render the per-turn workspace memory system message. Returns
    /// `None` when no `MemoryStore` is attached or all three files
    /// (`MEMORY.md`, `SOUL.md`, `USER.md`) are missing/empty. Each
    /// file is read live every turn (so Dream edits take effect
    /// immediately) and capped at `MEMORY_FILE_CAP_BYTES` to keep
    /// the per-turn token cost bounded as MEMORY.md grows. Read I/O
    /// failures degrade silently to "skip this file" — a transient
    /// disk error must never break the agent loop.
    fn build_memory_system_message(&self) -> Option<ChatMessage> {
        const MEMORY_FILE_CAP_BYTES: usize = 10 * 1024;
        let store = self.memory_store.as_ref()?;
        let mut sections: Vec<String> = Vec::new();
        let pieces: [(&str, std::result::Result<String, _>); 3] = [
            ("USER.md", store.read_user()),
            ("SOUL.md", store.read_soul()),
            ("MEMORY.md", store.read_memory()),
        ];
        for (label, result) in pieces {
            let Ok(body) = result else { continue };
            let trimmed = body.trim();
            if trimmed.is_empty() {
                continue;
            }
            let (slice, truncated) =
                zunel_util::truncate_at_char_boundary(trimmed, MEMORY_FILE_CAP_BYTES);
            let suffix = if truncated {
                "\n\n…(truncated; see file on disk for full contents)"
            } else {
                ""
            };
            sections.push(format!("## {label}\n{slice}{suffix}"));
        }
        if sections.is_empty() {
            return None;
        }
        let blob = sections.join("\n\n---\n\n");
        Some(ChatMessage::system(format!(
            "# Workspace Memory\n\n\
             Durable cross-session notes. USER.md describes the user, SOUL.md is your persona, \
             MEMORY.md is the long-term ledger Dream consolidates from your sessions.\n\n\
             {blob}"
        )))
    }

    /// Render the per-turn skills system message. Returns `None` when
    /// no loader is configured, when the loader yields no always-on
    /// bodies *and* no summary lines, or when the loader fails to read
    /// any of its sources (failures degrade to "no system message" so a
    /// transient I/O error never breaks the agent loop).
    fn build_skills_system_message(&self) -> Option<ChatMessage> {
        let loader = self.skills.as_ref()?;
        let always = loader.get_always_skills().ok()?;
        let always_blob = loader.load_skills_for_context(&always).ok()?;
        let exclude: HashSet<String> = always.iter().cloned().collect();
        let summary = loader.build_skills_summary(Some(&exclude)).ok()?;

        let mut sections: Vec<String> = Vec::new();
        if !always_blob.is_empty() {
            sections.push(format!("# Active Skills\n\n{always_blob}"));
        }
        if !summary.is_empty() {
            sections.push(format!(
                "# Skills\n\n\
                 The following skills extend your capabilities. To use a skill, read its SKILL.md file using the read_file tool.\n\
                 Unavailable skills need dependencies installed first — you can try installing them with apt/brew.\n\n\
                 {summary}"
            ));
        }
        if sections.is_empty() {
            return None;
        }
        Some(ChatMessage::system(sections.join("\n\n---\n\n")))
    }

    /// Stateless one-shot: send a single user message and return the result.
    pub async fn process_direct(&self, message: &str) -> Result<RunResult> {
        let settings = self.settings();
        let messages = vec![ChatMessage::user(message)];
        tracing::debug!(model = %self.defaults.model, "agent_loop: generating");
        let response = self
            .provider
            .generate(&self.defaults.model, &messages, &[], &settings)
            .await?;
        let usage = response.usage.clone();
        Ok(RunResult {
            content: response.content.unwrap_or_default(),
            tools_used: Vec::new(),
            messages,
            usage: usage.clone(),
            session_total_usage: usage,
        })
    }

    /// Streaming turn with session persistence. Feeds the accumulated
    /// conversation to the provider, emits deltas via `sink`, and persists
    /// the user + assistant messages after the stream ends.
    ///
    /// `sink` may be dropped early by the caller (e.g. user hit Ctrl+C);
    /// the loop tolerates that and still completes the turn server-side.
    pub async fn process_streamed(
        &self,
        session_key: &str,
        message: &str,
        sink: mpsc::Sender<StreamEvent>,
    ) -> Result<RunResult> {
        self.process_streamed_with_approval(session_key, message, sink, self.approval.clone(), None)
            .await
    }

    /// Per-call variant that lets the caller supply a [`CancellationToken`]
    /// for this single turn. Firing the token aborts the agent loop and
    /// surfaces [`crate::Error::Cancelled`]. Used by the REPL to wire
    /// Ctrl+C through to an in-flight streaming turn without mutating the
    /// shared loop's instance-level token.
    pub async fn process_streamed_with_cancel(
        &self,
        session_key: &str,
        message: &str,
        sink: mpsc::Sender<StreamEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<RunResult> {
        self.process_streamed_with_approval(
            session_key,
            message,
            sink,
            self.approval.clone(),
            Some(cancel),
        )
        .await
    }

    async fn process_streamed_with_approval(
        &self,
        session_key: &str,
        message: &str,
        sink: mpsc::Sender<StreamEvent>,
        approval: Arc<dyn ApprovalHandler>,
        cancel_override: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<RunResult> {
        let sessions = self
            .sessions
            .as_ref()
            .expect("process_streamed requires with_sessions()");
        let mut session = sessions
            .load(session_key)?
            .unwrap_or_else(|| Session::new(session_key));

        // Idle-compaction trigger: when the session has gone untouched
        // for longer than `agents.defaults.idle_compact_after_minutes`,
        // LLM-summarize everything older than `compaction_keep_tail`
        // before adding the new user turn. Failures are tolerated —
        // a slow turn beats a refused one.
        if let Some(threshold) = self.idle_compact_threshold_minutes() {
            if let Some(idle_minutes) = session.idle_minutes_since_last_user_turn() {
                if idle_minutes >= threshold as i64 {
                    let keep_tail = self
                        .defaults
                        .compaction_keep_tail
                        .unwrap_or(zunel_config::DEFAULT_COMPACTION_KEEP_TAIL);
                    let model = self
                        .defaults
                        .compaction_model
                        .clone()
                        .unwrap_or_else(|| self.defaults.model.clone());
                    let svc = CompactionService::new(self.provider.clone(), model);
                    match svc.compact_session(&mut session, keep_tail).await {
                        Ok(outcome) => {
                            tracing::info!(
                                session_key,
                                idle_minutes,
                                threshold_minutes = threshold,
                                compacted = outcome.compacted_count,
                                keep_tail,
                                "agent_loop: idle-compacted session",
                            );
                            if outcome.compacted_count > 0 {
                                sessions.save_async(&session).await?;
                                // Stage 1: hand the compaction summary
                                // to the memory store so Dream has
                                // input next tick. Sync I/O off the
                                // reactor; a failure is logged but
                                // does not abort the turn.
                                if let Some(store) = self.memory_store.clone() {
                                    let body = outcome.summary_body.clone();
                                    let key = session_key.to_string();
                                    let join = tokio::task::spawn_blocking(move || {
                                        store.append_history(&body)
                                    })
                                    .await;
                                    match join {
                                        Ok(Ok(cursor)) => {
                                            tracing::debug!(
                                                session_key = key,
                                                cursor,
                                                "agent_loop: appended compaction summary to memory history",
                                            );
                                        }
                                        Ok(Err(err)) => {
                                            tracing::warn!(
                                                session_key = key,
                                                error = %err,
                                                "agent_loop: failed to append compaction summary to memory history",
                                            );
                                        }
                                        Err(err) => {
                                            tracing::warn!(
                                                session_key = key,
                                                error = %err,
                                                "agent_loop: memory append join error",
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            tracing::warn!(
                                session_key,
                                error = %err,
                                "agent_loop: idle compaction failed; continuing with full history",
                            );
                        }
                    }
                }
            }
        }

        session.add_message(ChatRole::User, message);
        let history = session.get_history(self.history_window());
        let mut initial_messages = history_to_chat_messages(&history);
        // Stack the per-turn system messages: operator persona first
        // (Mode 2's `system_prompt` arg), then workspace memory
        // (USER.md / SOUL.md / MEMORY.md so Dream's consolidated
        // knowledge actually reaches the model), then skills summary,
        // then history. All three are re-rendered from live sources
        // every turn, so the persisted session log never accumulates
        // ephemeral system messages.
        let mut prepended: Vec<ChatMessage> = Vec::new();
        if let Some(extra) = self.extra_system_message.as_deref() {
            prepended.push(ChatMessage::system(extra));
        }
        if let Some(memory) = self.build_memory_system_message() {
            prepended.push(memory);
        }
        if let Some(skills) = self.build_skills_system_message() {
            prepended.push(skills);
        }
        for (i, msg) in prepended.into_iter().enumerate() {
            initial_messages.insert(i, msg);
        }
        let starting_len = initial_messages.len();

        let history_values: Vec<Value> =
            initial_messages.iter().map(chat_message_to_value).collect();
        let tokens_estimated = estimate_message_tokens(&history_values);
        let budgets = self.trim_budgets();

        tracing::debug!(
            model = %self.defaults.model,
            history_len = initial_messages.len(),
            history_window = self.history_window(),
            tokens_estimated,
            tool_result_chars = budgets.tool_result_chars,
            history_token_budget = budgets.history_tokens,
            "agent_loop: streaming",
        );

        // Snapshot the registry under a brief read lock. The inner
        // `Arc<ToolRegistry>` lets us hand the runner a single Arc
        // count bump instead of cloning the whole `BTreeMap<String,
        // Arc<dyn Tool>>` per turn — important for the gateway where
        // every inbound message would otherwise pay the map-rebuild
        // cost. Concurrent reload via `mcp_reconnect` or `/reload`
        // publishes a new Arc in the inner cell without disturbing
        // in-flight turns that already hold the old snapshot.
        let tools_snapshot: Arc<ToolRegistry> = self
            .tools
            .read()
            .expect("zunel tool registry lock poisoned")
            .clone();
        let runner = AgentRunner::new(self.provider.clone(), tools_snapshot, approval)
            .with_iteration_counter(self.iteration_counter.clone());
        let result = runner
            .run(
                AgentRunSpec {
                    initial_messages,
                    model: self.defaults.model.clone(),
                    settings: self.settings(),
                    max_iterations: self.defaults.max_tool_iterations.unwrap_or(15),
                    workspace: self.workspace.clone(),
                    session_key: session_key.into(),
                    approval_required: self.approval_required,
                    approval_scope: self.approval_scope,
                    hook: None,
                    trim_budgets: budgets,
                    cancel: cancel_override
                        .clone()
                        .unwrap_or_else(|| self.cancel.clone()),
                },
                sink,
            )
            .await?;

        for msg in result.messages.iter().skip(starting_len) {
            persist_runner_message(&mut session, msg);
        }
        session.record_turn_usage(&result.usage);
        let session_total_usage = session.usage_total();
        sessions.save_async(&session).await?;

        Ok(RunResult {
            content: result.content,
            tools_used: result.tools_used,
            messages: result.messages,
            usage: result.usage,
            session_total_usage,
        })
    }

    /// Gateway path: consume one inbound bus message, process it as a session
    /// turn, and publish the final assistant reply back to the same channel.
    pub async fn process_inbound_once(&self, bus: &Arc<MessageBus>) -> Result<()> {
        let Some(inbound) = bus.next_inbound().await else {
            return Ok(());
        };
        let session_key = format!("{}:{}", inbound.channel, inbound.chat_id);
        let media_paths = inbound
            .media
            .iter()
            .map(std::path::PathBuf::from)
            .collect::<Vec<_>>();
        let (content, _image_media) = extract_documents(&inbound.content, &media_paths);
        let session_msgs = self
            .sessions
            .as_ref()
            .and_then(|m| m.load(&session_key).ok().flatten())
            .map(|s| s.messages().len())
            .unwrap_or(0);
        tracing::info!(
            channel = %inbound.channel,
            chat_id = %inbound.chat_id,
            msg_bytes = content.len(),
            session_msgs,
            history_window = self.history_window(),
            "gateway: inbound received",
        );
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let approval: Arc<dyn ApprovalHandler> = if self.approval_required {
            Arc::new(BusApprovalHandler::new(bus.clone(), session_key.clone()))
        } else {
            self.approval.clone()
        };
        let result = self
            .process_streamed_with_approval(&session_key, &content, tx, approval, None)
            .await?;
        drain.abort();
        tracing::info!(
            channel = %inbound.channel,
            chat_id = %inbound.chat_id,
            prompt_tokens = result.usage.prompt_tokens,
            completion_tokens = result.usage.completion_tokens,
            reasoning_tokens = result.usage.reasoning_tokens,
            cached_tokens = result.usage.cached_tokens,
            session_total_prompt = result.session_total_usage.prompt_tokens,
            session_total_completion = result.session_total_usage.completion_tokens,
            session_total_reasoning = result.session_total_usage.reasoning_tokens,
            "gateway: turn complete",
        );
        let mut outbound_content = result.content;
        if self.show_token_footer {
            let footer =
                crate::usage_footer::format_footer(&result.usage, &result.session_total_usage);
            if !footer.is_empty() {
                if !outbound_content.is_empty() && !outbound_content.ends_with('\n') {
                    outbound_content.push('\n');
                }
                outbound_content.push_str(&footer);
            }
        }
        bus.publish_outbound(OutboundMessage {
            channel: inbound.channel,
            chat_id: inbound.chat_id,
            message_id: None,
            content: outbound_content,
            media: Vec::new(),
            kind: MessageKind::Final,
        })
        .await?;
        Ok(())
    }
}

fn persist_runner_message(session: &mut Session, msg: &ChatMessage) {
    // Plain text turns serialize fine via add_message (it stamps a
    // timestamp). Tool messages and assistant turns carrying tool
    // calls need the wire-shaped JSON, which add_message can't
    // express, so we go through append_raw_message.
    if matches!(msg.role, Role::Tool) || !msg.tool_calls.is_empty() {
        session.append_raw_message(chat_message_to_value(msg));
    } else {
        let role = match msg.role {
            Role::User => ChatRole::User,
            Role::Assistant => ChatRole::Assistant,
            Role::System => ChatRole::System,
            Role::Tool => ChatRole::Tool,
        };
        session.add_message(role, &msg.content);
    }
}

/// Convert persisted `Value` messages (from Session::get_history) into
/// provider-ready `ChatMessage`s, dropping any rows the trim helper
/// can't normalize.
fn history_to_chat_messages(history: &[Value]) -> Vec<ChatMessage> {
    history
        .iter()
        .filter_map(|m| crate::trim::value_to_chat_message(m).ok())
        .collect()
}
