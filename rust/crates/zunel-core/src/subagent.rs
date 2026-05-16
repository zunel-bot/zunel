use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zunel_providers::{ChatMessage, LLMProvider};
use zunel_tools::self_tool::{SelfState, SelfStateProvider, SubagentSummary};
use zunel_tools::spawn::SpawnHandle;
use zunel_tools::ToolRegistry;

use crate::approval::{ApprovalDecision, ApprovalHandler, ApprovalRequest, ApprovalScope};
use crate::runner::{AgentRunSpec, AgentRunner};

/// Acquire a mutex and recover from poisoning instead of panicking.
///
/// A poisoned mutex on a status/handle map only means a previous task panicked
/// while holding the lock. The data behind it is still accessible, and we'd
/// rather report a degraded subagent than tear down the whole agent loop.
fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentStatus {
    pub id: String,
    pub label: String,
    pub task: String,
    pub phase: String,
    pub iteration: usize,
    pub result: Option<String>,
    pub error: Option<String>,
}

pub struct SubagentManager {
    provider: Arc<dyn LLMProvider>,
    workspace: std::path::PathBuf,
    model: String,
    child_tools: ToolRegistry,
    statuses: Arc<Mutex<BTreeMap<String, SubagentStatus>>>,
    handles: Arc<Mutex<BTreeMap<String, JoinHandle<()>>>>,
    counter: AtomicUsize,
}

pub struct RuntimeSelfStateProvider {
    pub model: String,
    pub provider: String,
    pub workspace: String,
    pub max_iterations: u32,
    pub tools: Vec<String>,
    pub subagents: Arc<SubagentManager>,
}

impl SelfStateProvider for RuntimeSelfStateProvider {
    fn state(&self) -> SelfState {
        SelfState {
            model: self.model.clone(),
            provider: self.provider.clone(),
            workspace: self.workspace.clone(),
            max_iterations: self.max_iterations,
            current_iteration: 0,
            tools: self.tools.clone(),
            subagents: self
                .subagents
                .statuses()
                .into_iter()
                .map(|status| SubagentSummary {
                    id: status.id,
                    label: status.label,
                    phase: status.phase,
                    iteration: status.iteration as u32,
                })
                .collect(),
        }
    }
}

impl SubagentManager {
    pub fn new(
        provider: Arc<dyn LLMProvider>,
        workspace: std::path::PathBuf,
        model: String,
    ) -> Self {
        Self {
            provider,
            workspace,
            model,
            child_tools: ToolRegistry::new(),
            statuses: Arc::new(Mutex::new(BTreeMap::new())),
            handles: Arc::new(Mutex::new(BTreeMap::new())),
            counter: AtomicUsize::new(1),
        }
    }

    pub fn with_child_tools(mut self, child_tools: ToolRegistry) -> Self {
        self.child_tools = child_tools;
        self
    }

    pub fn status(&self, id: &str) -> Option<SubagentStatus> {
        lock_recover(&self.statuses).get(id).cloned()
    }

    pub fn statuses(&self) -> Vec<SubagentStatus> {
        lock_recover(&self.statuses).values().cloned().collect()
    }

    pub fn cancel(&self, id: &str) -> bool {
        let Some(handle) = lock_recover(&self.handles).remove(id) else {
            return false;
        };
        handle.abort();
        if let Some(status) = lock_recover(&self.statuses).get_mut(id) {
            status.phase = "cancelled".into();
        }
        true
    }

    fn next_id(&self) -> String {
        format!("{:08x}", self.counter.fetch_add(1, Ordering::Relaxed))
    }
}

#[async_trait]
impl SpawnHandle for SubagentManager {
    async fn spawn(&self, task: String, label: Option<String>) -> Result<String, String> {
        let id = self.next_id();
        let label = label.unwrap_or_else(|| {
            let mut s: String = task.chars().take(30).collect();
            if task.chars().count() > 30 {
                s.push_str("...");
            }
            s
        });
        let status = SubagentStatus {
            id: id.clone(),
            label: label.clone(),
            task: task.clone(),
            phase: "running".into(),
            iteration: 0,
            result: None,
            error: None,
        };
        lock_recover(&self.statuses).insert(id.clone(), status);

        let provider = Arc::clone(&self.provider);
        let statuses = Arc::clone(&self.statuses);
        let handles = Arc::clone(&self.handles);
        let workspace = self.workspace.clone();
        let model = self.model.clone();
        let child_tools = self.child_tools.clone();
        let child_id = id.clone();
        let handle = tokio::spawn(async move {
            run_child(
                provider,
                statuses,
                child_id.clone(),
                task,
                model,
                workspace,
                child_tools,
            )
            .await;
            lock_recover(&handles).remove(&child_id);
        });
        lock_recover(&self.handles).insert(id.clone(), handle);

        Ok(format!(
            "Subagent [{label}] started (id: {id}). Use the self tool to inspect status and results."
        ))
    }
}

async fn run_child(
    provider: Arc<dyn LLMProvider>,
    statuses: Arc<Mutex<BTreeMap<String, SubagentStatus>>>,
    id: String,
    task: String,
    model: String,
    workspace: std::path::PathBuf,
    child_tools: ToolRegistry,
) {
    let start = Instant::now();
    let runner = AgentRunner::new(provider, Arc::new(child_tools), Arc::new(NoApproval));
    let (tx, mut rx) = mpsc::channel(16);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = runner
        .run(
            AgentRunSpec {
                initial_messages: vec![
                    ChatMessage::system(
                        "You are a focused subagent. Complete the task and report the result.",
                    ),
                    ChatMessage::user(task),
                ],
                model,
                settings: Default::default(),
                max_iterations: 15,
                workspace,
                session_key: format!("subagent:{id}"),
                approval_required: false,
                approval_scope: ApprovalScope::All,
                hook: None,
                trim_budgets: Default::default(),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
            tx,
        )
        .await;
    let _ = drain.await;

    let mut guard = lock_recover(&statuses);
    if let Some(status) = guard.get_mut(&id) {
        status.iteration = start.elapsed().as_secs() as usize;
        match result {
            Ok(result) => {
                status.phase = "done".into();
                status.result = Some(result.content);
            }
            Err(err) => {
                status.phase = "error".into();
                status.error = Some(err.to_string());
            }
        }
    }
}

struct NoApproval;

#[async_trait]
impl ApprovalHandler for NoApproval {
    async fn request(&self, _req: ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Approve
    }
}
