use std::future::Future;
use std::pin::Pin;

use crate::error::Result;

/// Input a command handler receives.
#[derive(Debug, Clone)]
pub struct CommandContext {
    pub session_key: String,
    pub raw: String,
    pub args: String,
}

/// Outcome of running a slash command.
#[derive(Debug, Clone)]
pub enum CommandOutcome {
    /// Print this text as the bot's reply.
    Reply(String),
    /// Reset the current session before the next turn.
    ClearSession,
    /// Exit the REPL.
    Exit,
    /// Re-exec the current process (handled by the CLI, not core).
    Restart,
    /// Re-discover MCP servers from disk and splice their tools into
    /// the live registry. `target = None` reloads every configured
    /// server; `target = Some(name)` reloads one. Handled by the
    /// CLI REPL because it owns the `Arc<AgentLoop>` the reload
    /// runs against.
    ReloadMcp { target: Option<String> },
}

type BoxedHandler = Box<
    dyn Fn(CommandContext) -> Pin<Box<dyn Future<Output = Result<CommandOutcome>> + Send>>
        + Send
        + Sync,
>;

#[derive(Default)]
pub struct CommandRouter {
    exact: Vec<(String, BoxedHandler)>,
    prefix: Vec<(String, BoxedHandler)>,
}

impl CommandRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_exact<F, Fut>(&mut self, cmd: &str, handler: F)
    where
        F: Fn(CommandContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CommandOutcome>> + Send + 'static,
    {
        self.exact
            .push((cmd.to_string(), Box::new(move |ctx| Box::pin(handler(ctx)))));
    }

    pub fn register_prefix<F, Fut>(&mut self, prefix: &str, handler: F)
    where
        F: Fn(CommandContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<CommandOutcome>> + Send + 'static,
    {
        self.prefix.push((
            prefix.to_string(),
            Box::new(move |ctx| Box::pin(handler(ctx))),
        ));
        // Longest prefix wins.
        self.prefix
            .sort_by_key(|entry| std::cmp::Reverse(entry.0.len()));
    }

    pub async fn dispatch(&self, ctx: &CommandContext) -> Result<Option<CommandOutcome>> {
        let raw = ctx.raw.trim().to_string();
        for (cmd, handler) in &self.exact {
            if raw.eq_ignore_ascii_case(cmd) {
                let c = CommandContext {
                    session_key: ctx.session_key.clone(),
                    raw: raw.clone(),
                    args: String::new(),
                };
                return handler(c).await.map(Some);
            }
        }
        for (prefix, handler) in &self.prefix {
            if raw
                .to_ascii_lowercase()
                .starts_with(&prefix.to_ascii_lowercase())
            {
                let args = raw[prefix.len()..].to_string();
                let c = CommandContext {
                    session_key: ctx.session_key.clone(),
                    raw: raw.clone(),
                    args,
                };
                return handler(c).await.map(Some);
            }
        }
        Ok(None)
    }
}

pub mod builtins {
    use super::{CommandContext, CommandOutcome, CommandRouter};
    use crate::error::Result;

    /// Canonical help text shown by `/help`.
    pub fn help_text() -> String {
        [
            "zunel commands:",
            "/help — Show available commands",
            "/clear — Clear the current conversation",
            "/status — Show bot status",
            "/reload [server] — Re-discover MCP servers (or one by name) without restart",
            "/restart — Restart the process",
            "/exit — Exit the REPL (alias: /quit)",
        ]
        .join("\n")
    }

    pub fn register_defaults(router: &mut CommandRouter) {
        router.register_exact("/help", |_ctx: CommandContext| async {
            Ok::<_, crate::Error>(CommandOutcome::Reply(help_text()))
        });
        router.register_exact("/clear", |_ctx: CommandContext| async {
            Ok::<_, crate::Error>(CommandOutcome::ClearSession)
        });
        router.register_exact("/restart", |_ctx: CommandContext| async {
            Ok::<_, crate::Error>(CommandOutcome::Restart)
        });
        // `/exit` and `/quit` both map to `CommandOutcome::Exit`.
        // Two aliases because users reach for one or the other —
        // cheaper to wire both than to require muscle memory.
        router.register_exact("/exit", |_ctx: CommandContext| async {
            Ok::<_, crate::Error>(CommandOutcome::Exit)
        });
        router.register_exact("/quit", |_ctx: CommandContext| async {
            Ok::<_, crate::Error>(CommandOutcome::Exit)
        });
        // `/reload` (no arg) reloads every configured MCP server;
        // `/reload <name>` reloads one. Both shapes resolve to the
        // same `ReloadMcp` outcome — the CLI REPL owns the
        // `Arc<AgentLoop>` and runs the actual reload.
        router.register_exact("/reload", |_ctx: CommandContext| async {
            Ok::<_, crate::Error>(CommandOutcome::ReloadMcp { target: None })
        });
        router.register_prefix("/reload ", |ctx: CommandContext| async move {
            let target = ctx.args.trim();
            let target = if target.is_empty() {
                None
            } else {
                Some(target.to_string())
            };
            Ok::<_, crate::Error>(CommandOutcome::ReloadMcp { target })
        });
        // /status is registered by the CLI because it needs access to
        // agent-level state (model name, session message count) that
        // zunel-core cannot see without building a bigger object graph.
    }

    #[allow(dead_code)]
    fn _unused(_: Result<CommandOutcome>) {}
}
