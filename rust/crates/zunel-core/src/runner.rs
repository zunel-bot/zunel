//! Agent runner: drives a single turn's provider-stream ↔ tool-call loop.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::mpsc;

use zunel_config::{
    AgentDefaults, DEFAULT_CONTEXT_WINDOW_TOKENS, DEFAULT_MAX_TOKENS_FALLBACK,
    DEFAULT_TOOL_RESULT_BUDGET_CHARS, HISTORY_BUDGET_HEADROOM_TOKENS,
};
use zunel_providers::{
    ChatMessage, GenerationSettings, LLMProvider, Role, StreamEvent, ToolCallAccumulator,
    ToolCallRequest, ToolProgress, Usage,
};
use zunel_tools::{ToolContext, ToolRegistry};
use zunel_util::truncate_at_char_boundary;

use crate::approval::{
    tool_requires_approval, ApprovalDecision, ApprovalHandler, ApprovalRequest, ApprovalScope,
};
use crate::hook::{AgentHook, AgentHookContext};
use crate::trim::{
    apply_tool_result_budget, backfill_missing_tool_results, chat_message_to_value,
    drop_orphan_tool_results, microcompact_old_tool_results, snip_history, value_to_chat_message,
};

/// Concrete byte and token caps used by [`trim_messages_for_provider`].
///
/// Built from [`AgentDefaults`] via [`TrimBudgets::from_defaults`] so the
/// runner can be unit-tested without a full config and so callers that
/// don't care (e.g. legacy tests) can use [`TrimBudgets::default`].
#[derive(Debug, Clone, Copy)]
pub struct TrimBudgets {
    /// Per–tool-message char cap (truncates oversized tool results).
    pub tool_result_chars: usize,
    /// Total token budget for the history sent to the provider.
    pub history_tokens: usize,
}

impl Default for TrimBudgets {
    fn default() -> Self {
        Self::from_defaults(&AgentDefaults::default())
    }
}

impl TrimBudgets {
    /// Resolve concrete budgets from agent config, applying
    /// `DEFAULT_*` fallbacks when fields are unset and reserving the
    /// model's reply tokens (`max_tokens` + `HISTORY_BUDGET_HEADROOM_TOKENS`)
    /// out of `context_window_tokens`.
    pub fn from_defaults(defaults: &AgentDefaults) -> Self {
        let tool_result_chars = defaults
            .max_tool_result_chars
            .unwrap_or(DEFAULT_TOOL_RESULT_BUDGET_CHARS);
        let context_window = defaults
            .context_window_tokens
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
        let max_tokens = defaults.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS_FALLBACK);
        let reserved = max_tokens.saturating_add(HISTORY_BUDGET_HEADROOM_TOKENS);
        let history_tokens = context_window.saturating_sub(reserved).max(1) as usize;
        Self {
            tool_result_chars,
            history_tokens,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Completed,
    MaxIterations,
    Error,
    ToolError,
    EmptyFinalResponse,
}

pub struct AgentRunSpec {
    /// System + bootstrap + skills prompt plus the turn's user message.
    /// Runner mutates a working copy; callers that need the full
    /// post-run history read `AgentRunResult::messages`.
    pub initial_messages: Vec<ChatMessage>,
    pub model: String,
    pub settings: GenerationSettings,
    pub max_iterations: usize,
    pub workspace: std::path::PathBuf,
    pub session_key: String,
    pub approval_required: bool,
    pub approval_scope: ApprovalScope,
    pub hook: Option<Arc<dyn AgentHook>>,
    /// Per-turn trim budgets. `Default` falls back to the previous
    /// in-runner constants so legacy tests that don't construct this
    /// field keep working.
    pub trim_budgets: TrimBudgets,
    /// Per-turn cancellation token. The runner checks this between
    /// every iteration, between every tool call inside one
    /// iteration, and between every streamed event from the
    /// provider; when fired the run unwinds with
    /// [`crate::Error::Cancelled`]. Forwarded onto the
    /// [`ToolContext`] so individual tools that have their own
    /// cancel-aware select loops also notice. Defaults to a
    /// never-cancelled token so existing callers keep their
    /// "uninterruptible" behaviour.
    pub cancel: tokio_util::sync::CancellationToken,
}

impl Default for AgentRunSpec {
    fn default() -> Self {
        Self {
            initial_messages: Vec::new(),
            model: String::new(),
            settings: GenerationSettings::default(),
            max_iterations: 15,
            workspace: std::env::temp_dir(),
            session_key: String::new(),
            approval_required: false,
            approval_scope: ApprovalScope::default(),
            hook: None,
            trim_budgets: TrimBudgets::default(),
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }
}

pub struct AgentRunResult {
    pub content: String,
    pub tools_used: Vec<String>,
    pub messages: Vec<ChatMessage>,
    pub stop_reason: StopReason,
    /// Sum of [`Usage`] across every provider iteration in this turn.
    /// Always populated; defaults to `Usage::default()` when the provider
    /// did not emit usage in any iteration.
    pub usage: Usage,
}

pub struct AgentRunner {
    provider: Arc<dyn LLMProvider>,
    tools: Arc<ToolRegistry>,
    approval: Arc<dyn ApprovalHandler>,
    /// Optional sink for the runner's current 1-indexed iteration
    /// count. Updated at the start of every loop body and reset to 0
    /// when the run unwinds. The `self` tool consumer reads this to
    /// answer "how many iterations have we used so far this turn?"
    /// instead of the previous hard-coded `0`.
    iteration_counter: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

impl AgentRunner {
    pub fn new(
        provider: Arc<dyn LLMProvider>,
        tools: Arc<ToolRegistry>,
        approval: Arc<dyn ApprovalHandler>,
    ) -> Self {
        Self {
            provider,
            tools,
            approval,
            iteration_counter: None,
        }
    }

    /// Attach a shared counter that the runner will update with the
    /// current 1-indexed iteration number at the start of each loop
    /// body and reset to 0 on exit. Used by `RuntimeSelfStateProvider`
    /// so the `self` tool reports the live iteration instead of a
    /// hard-coded zero. Optional — runners constructed without one
    /// behave exactly as before.
    pub fn with_iteration_counter(mut self, counter: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        self.iteration_counter = Some(counter);
        self
    }

    pub async fn run(
        &self,
        spec: AgentRunSpec,
        sink: mpsc::Sender<StreamEvent>,
    ) -> Result<AgentRunResult, crate::Error> {
        let mut messages = spec.initial_messages.clone();
        let mut tools_used: Vec<String> = Vec::new();
        let mut ctx =
            ToolContext::new_with_workspace(spec.workspace.clone(), spec.session_key.clone());
        // Forward the run's cancel token to every tool we hand the
        // context to. Tools that already select on `ctx.cancel` will
        // bail out the same instant the run unwinds.
        ctx.cancel = spec.cancel.clone();

        let max_iter = if spec.max_iterations == 0 {
            15
        } else {
            spec.max_iterations
        };
        let tool_defs: Vec<zunel_providers::ToolSchema> = self
            .tools
            .get_definitions()
            .into_iter()
            .map(schema_from_definition)
            .collect();

        let mut last_content = String::new();
        let mut stop = StopReason::Error;
        let mut total_usage = Usage::default();

        'outer: for iteration in 0..max_iter {
            // Publish the 1-indexed iteration count so concurrent
            // `self` tool calls see the live value rather than a
            // hard-coded zero. Reset to 0 below when the run exits.
            if let Some(counter) = self.iteration_counter.as_ref() {
                counter.store(iteration + 1, std::sync::atomic::Ordering::Relaxed);
            }
            // Cancel-check 1: before we even ask the provider for the
            // next iteration, bail out if the hub cancelled the call
            // since the previous iteration finished.
            if spec.cancel.is_cancelled() {
                return Err(crate::Error::Cancelled);
            }
            tracing::debug!(iteration, "agent iteration");
            let hook = spec.hook.as_ref();
            if let Some(hook) = hook {
                hook.before_iteration(AgentHookContext::new(iteration, messages.clone()))
                    .await;
            }
            let messages_for_model = trim_messages_for_provider(&messages, spec.trim_budgets)?;
            let (content, finish_reason, calls) = {
                let stream = self.provider.generate_stream(
                    &spec.model,
                    &messages_for_model,
                    &tool_defs,
                    &spec.settings,
                );
                futures::pin_mut!(stream);
                let mut acc = ToolCallAccumulator::default();
                let mut content = String::new();
                let mut finish_reason: Option<String> = None;
                loop {
                    // Cancel-check 2: race the stream against the
                    // cancel token. Cancelling mid-stream beats waiting
                    // for the provider to give us another delta —
                    // important for long completions / reasoning
                    // bursts.
                    let next_event = tokio::select! {
                        biased;
                        _ = spec.cancel.cancelled() => {
                            return Err(crate::Error::Cancelled);
                        }
                        event = stream.next() => event,
                    };
                    let Some(event) = next_event else { break };
                    let event = event.map_err(crate::Error::Provider)?;
                    let _ = sink.send(event.clone()).await;
                    match &event {
                        StreamEvent::ContentDelta(s) => {
                            content.push_str(s);
                            if let Some(hook) = hook {
                                let mut context =
                                    AgentHookContext::new(iteration, messages_for_model.clone());
                                context.final_content = Some(content.clone());
                                hook.on_stream(context, s.clone()).await;
                            }
                        }
                        StreamEvent::Done(resp) => {
                            finish_reason = resp.finish_reason.clone();
                            total_usage += &resp.usage;
                        }
                        _ => {}
                    }
                    acc.push(event);
                }
                let calls = acc
                    .finalize()
                    .map_err(|e| crate::Error::ToolCallAssembly(e.to_string()))?;
                if let Some(hook) = hook {
                    let mut context = AgentHookContext::new(iteration, messages_for_model.clone());
                    context.final_content = if content.is_empty() {
                        None
                    } else {
                        Some(content.clone())
                    };
                    hook.on_stream_end(context, finish_reason.as_deref() == Some("length"))
                        .await;
                }
                (content, finish_reason, calls)
            };

            if calls.is_empty() {
                stop = if content.is_empty() && finish_reason.as_deref() != Some("length") {
                    StopReason::EmptyFinalResponse
                } else {
                    StopReason::Completed
                };
                let mut final_content = if content.is_empty() {
                    None
                } else {
                    Some(content.clone())
                };
                if let Some(hook) = hook {
                    let mut context = AgentHookContext::new(iteration, messages.clone());
                    context.final_content = final_content.clone();
                    context.stop_reason = Some(format!("{stop:?}"));
                    final_content = hook.finalize_content(context, final_content);
                }
                last_content = final_content.clone().unwrap_or_default();
                if !content.is_empty() {
                    messages.push(ChatMessage {
                        role: Role::Assistant,
                        content: final_content.unwrap_or(content),
                        tool_call_id: None,
                        tool_calls: Vec::new(),
                    });
                }
                if let Some(hook) = hook {
                    let mut context = AgentHookContext::new(iteration, messages.clone());
                    context.final_content = Some(last_content.clone());
                    context.stop_reason = Some(format!("{stop:?}"));
                    hook.after_iteration(context).await;
                }
                break 'outer;
            }

            messages.push(ChatMessage {
                role: Role::Assistant,
                content: content.clone(),
                tool_call_id: None,
                tool_calls: calls.clone(),
            });

            if let Some(hook) = hook {
                let mut context = AgentHookContext::new(iteration, messages.clone());
                context.tool_calls = calls.clone();
                hook.before_execute_tools(context).await;
            }
            let mut iteration_results = Vec::new();
            for call in &calls {
                // Cancel-check 3: between two tool calls in the same
                // iteration. Important when one call has just finished
                // (so the cancel select didn't fire mid-stream) and
                // another is queued — we don't want to drag the user
                // through unwanted side effects.
                if spec.cancel.is_cancelled() {
                    return Err(crate::Error::Cancelled);
                }
                tools_used.push(call.name.clone());
                let _ = sink
                    .send(StreamEvent::ToolProgress(ToolProgress::Start {
                        index: call.index,
                        name: call.name.clone(),
                    }))
                    .await;
                if spec.approval_required && tool_requires_approval(&call.name, spec.approval_scope)
                {
                    let req = ApprovalRequest {
                        tool_name: call.name.clone(),
                        args: call.arguments.clone(),
                        description: describe_call(call),
                        scope: spec.approval_scope,
                    };
                    match self.approval.request(req).await {
                        ApprovalDecision::Approve => {}
                        ApprovalDecision::Deny => {
                            messages.push(tool_result_message(
                                &call.id,
                                &call.name,
                                "denied by user",
                            ));
                            let _ = sink
                                .send(StreamEvent::ToolProgress(ToolProgress::Done {
                                    index: call.index,
                                    name: call.name.clone(),
                                    ok: false,
                                    snippet: "denied".into(),
                                }))
                                .await;
                            continue;
                        }
                    }
                }
                let result = self
                    .tools
                    .execute(&call.name, call.arguments.clone(), &ctx)
                    .await
                    .expect("registry never fails");
                let _ = sink
                    .send(StreamEvent::ToolProgress(ToolProgress::Done {
                        index: call.index,
                        name: call.name.clone(),
                        ok: !result.is_error,
                        snippet: progress_snippet(&result.content),
                    }))
                    .await;
                messages.push(tool_result_message(&call.id, &call.name, &result.content));
                iteration_results.push(result);
            }

            if iteration + 1 == max_iter {
                stop = StopReason::MaxIterations;
            }
            if let Some(hook) = hook {
                let mut context = AgentHookContext::new(iteration, messages.clone());
                context.tool_calls = calls.clone();
                context.tool_results = iteration_results;
                context.stop_reason = Some(format!("{stop:?}"));
                hook.after_iteration(context).await;
            }
        }

        // Mark the runner idle so a follow-up `self` tool call (or a
        // background observer) doesn't keep reporting the last
        // iteration of a finished turn.
        if let Some(counter) = self.iteration_counter.as_ref() {
            counter.store(0, std::sync::atomic::Ordering::Relaxed);
        }

        Ok(AgentRunResult {
            content: last_content,
            tools_used,
            messages,
            stop_reason: stop,
            usage: total_usage,
        })
    }
}

fn tool_result_message(tool_call_id: &str, _name: &str, content: &str) -> ChatMessage {
    ChatMessage {
        role: Role::Tool,
        content: content.to_string(),
        tool_call_id: Some(tool_call_id.to_string()),
        tool_calls: Vec::new(),
    }
}

fn describe_call(tc: &ToolCallRequest) -> String {
    format!("{}({})", tc.name, tc.arguments)
}

/// Trim a tool result down to a single line ≤ 80 chars suitable for
/// rendering as inline progress (`[tool: read_file → ok 12 bytes]`).
fn progress_snippet(content: &str) -> String {
    let single = content.lines().next().unwrap_or("").trim();
    match truncate_at_char_boundary(single, 79) {
        (prefix, true) => format!("{prefix}…"),
        (prefix, false) => prefix.to_string(),
    }
}

/// Apply the five-stage trim pipeline before sending history to the
/// provider. Operates on wire-format `Value` objects since that's the
/// shape the trim helpers work with.
///
/// Budgets come from [`TrimBudgets`] (built from `agents.defaults`),
/// not from in-source constants — change `context_window_tokens` and
/// `max_tool_result_chars` in config to retune.
pub fn trim_messages_for_provider(
    messages: &[ChatMessage],
    budgets: TrimBudgets,
) -> Result<Vec<ChatMessage>, crate::Error> {
    let values: Vec<serde_json::Value> = messages.iter().map(chat_message_to_value).collect();
    let values = drop_orphan_tool_results(&values);
    let values = backfill_missing_tool_results(&values);
    let values = microcompact_old_tool_results(&values);
    let values = apply_tool_result_budget(&values, budgets.tool_result_chars);
    let values = snip_history(&values, budgets.history_tokens);

    values
        .iter()
        .map(value_to_chat_message)
        .collect::<Result<_, _>>()
        .map_err(crate::Error::ToolCallAssembly)
}

fn schema_from_definition(def: serde_json::Value) -> zunel_providers::ToolSchema {
    let function = def
        .get("function")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    zunel_providers::ToolSchema {
        name: function
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        description: function
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        parameters: function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    }
}
