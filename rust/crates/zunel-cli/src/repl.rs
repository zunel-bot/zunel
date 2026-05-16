use std::io::{self, BufRead, IsTerminal};
use std::sync::Arc;

use anyhow::{Context, Result};
use reedline::{DefaultPrompt, DefaultPromptSegment, FileBackedHistory, Reedline, Signal};
use tokio::sync::mpsc;
use zunel_core::{
    command::builtins, AgentLoop, CommandContext, CommandOutcome, CommandRouter, SessionManager,
};

use crate::renderer::StreamingRenderer;

pub struct ReplConfig {
    pub session_key: String,
    pub model_label: String,
    /// When `true`, print a token-usage footer after each assistant
    /// reply. Wired from `cli.showTokenFooter` or `--show-tokens`.
    pub show_token_footer: bool,
}

pub async fn run_repl(
    agent_loop: Arc<AgentLoop>,
    sessions: Arc<SessionManager>,
    config: ReplConfig,
) -> Result<()> {
    let mut router = CommandRouter::new();
    builtins::register_defaults(&mut router);

    // Register a minimal /status handler at the CLI level so it can see
    // the model label and the current session's message count (two things
    // zunel-core deliberately keeps out of the router context this slice).
    let status_sessions = sessions.clone();
    let status_model = config.model_label.clone();
    router.register_exact("/status", move |ctx: CommandContext| {
        let sessions = status_sessions.clone();
        let model = status_model.clone();
        async move {
            let count = match sessions.load(&ctx.session_key) {
                Ok(Some(session)) => session.messages().len(),
                _ => 0,
            };
            Ok(CommandOutcome::Reply(format!(
                "model: {model}\nsession: {} ({count} messages)",
                ctx.session_key
            )))
        }
    });

    println!(
        "zunel interactive mode ({}) — /help for commands, /exit or Ctrl+D to quit\n",
        config.model_label,
    );

    if io::stdin().is_terminal() {
        run_interactive(agent_loop, sessions, &router, &config).await
    } else {
        // Scripted/piped stdin can't drive reedline (it needs termios). Fall
        // back to a plain line-buffered reader so CI tests and shell pipes
        // still work.
        run_scripted(agent_loop, sessions, &router, &config).await
    }
}

async fn run_interactive(
    agent_loop: Arc<AgentLoop>,
    sessions: Arc<SessionManager>,
    router: &CommandRouter,
    config: &ReplConfig,
) -> Result<()> {
    let history_path =
        zunel_config::cli_history_path().with_context(|| "resolving CLI history path")?;
    if let Some(parent) = history_path.parent() {
        zunel_util::ensure_dir(parent).ok();
    }
    let history: Box<FileBackedHistory> = Box::new(
        FileBackedHistory::with_file(1000, history_path)
            .with_context(|| "opening reedline history")?,
    );

    let mut line_editor = Reedline::create().with_history(history);
    let prompt = DefaultPrompt::new(
        DefaultPromptSegment::Basic("you".into()),
        DefaultPromptSegment::Empty,
    );

    loop {
        match line_editor.read_line(&prompt) {
            Ok(Signal::Success(input)) => {
                match dispatch_line(
                    router,
                    agent_loop.as_ref(),
                    sessions.as_ref(),
                    &config.session_key,
                    input.trim(),
                    config.show_token_footer,
                )
                .await?
                {
                    LineFlow::Continue => continue,
                    LineFlow::Exit => break,
                    LineFlow::Restart => {
                        exec_restart()?;
                        unreachable!("exec replaces the process");
                    }
                }
            }
            Ok(Signal::CtrlC) => continue,
            Ok(Signal::CtrlD) => {
                println!("\nGoodbye!");
                break;
            }
            Err(err) => {
                return Err(anyhow::anyhow!("repl io error: {err}"));
            }
        }
    }
    Ok(())
}

async fn run_scripted(
    agent_loop: Arc<AgentLoop>,
    sessions: Arc<SessionManager>,
    router: &CommandRouter,
    config: &ReplConfig,
) -> Result<()> {
    let stdin = io::stdin();
    let mut handle = stdin.lock();
    let mut buf = String::new();
    loop {
        buf.clear();
        let read = handle
            .read_line(&mut buf)
            .with_context(|| "reading scripted stdin")?;
        if read == 0 {
            println!("\nGoodbye!");
            break;
        }
        match dispatch_line(
            router,
            agent_loop.as_ref(),
            sessions.as_ref(),
            &config.session_key,
            buf.trim(),
            config.show_token_footer,
        )
        .await?
        {
            LineFlow::Continue => continue,
            LineFlow::Exit => break,
            LineFlow::Restart => {
                exec_restart()?;
                unreachable!("exec replaces the process");
            }
        }
    }
    Ok(())
}

enum LineFlow {
    Continue,
    Exit,
    Restart,
}

async fn dispatch_line(
    router: &CommandRouter,
    agent_loop: &AgentLoop,
    sessions: &SessionManager,
    session_key: &str,
    line: &str,
    show_footer: bool,
) -> Result<LineFlow> {
    if line.is_empty() {
        return Ok(LineFlow::Continue);
    }
    if line.starts_with('/') {
        handle_command(router, agent_loop, session_key, line, sessions).await
    } else {
        run_turn(agent_loop, session_key, line, show_footer).await?;
        Ok(LineFlow::Continue)
    }
}

async fn handle_command(
    router: &CommandRouter,
    agent_loop: &AgentLoop,
    session_key: &str,
    line: &str,
    sessions: &SessionManager,
) -> Result<LineFlow> {
    let ctx = CommandContext {
        session_key: session_key.to_string(),
        raw: line.to_string(),
        args: String::new(),
    };
    match router.dispatch(&ctx).await? {
        Some(CommandOutcome::Reply(text)) => {
            println!("{text}");
            Ok(LineFlow::Continue)
        }
        Some(CommandOutcome::ClearSession) => {
            if let Some(mut session) = sessions.load(session_key)? {
                session.clear();
                sessions.save(&session)?;
            }
            println!("Session cleared.");
            Ok(LineFlow::Continue)
        }
        Some(CommandOutcome::Exit) => Ok(LineFlow::Exit),
        Some(CommandOutcome::Restart) => Ok(LineFlow::Restart),
        Some(CommandOutcome::ReloadMcp { target }) => {
            match agent_loop.reload_mcp(target.as_deref(), None).await {
                Ok(report) => println!("{}", format_reload_report(&report)),
                Err(err) => println!("Reload failed: {err}"),
            }
            Ok(LineFlow::Continue)
        }
        Some(CommandOutcome::RunDream) => {
            match agent_loop.run_dream().await {
                Ok(outcome) if outcome.processed_entries == 0 => {
                    println!("Dream: no new history entries to consolidate.");
                }
                Ok(outcome) => {
                    let cursor = outcome
                        .cursor_advanced_to
                        .map(|c| format!("cursor → {c}"))
                        .unwrap_or_else(|| "cursor not advanced (no edits applied)".into());
                    let edits = if outcome.edited_files.is_empty() {
                        "no files edited".to_string()
                    } else {
                        format!("edits: {}", outcome.edited_files.join(", "))
                    };
                    println!(
                        "Dream: processed {} entries; {}; {}.",
                        outcome.processed_entries, edits, cursor
                    );
                }
                Err(err) => println!("Dream failed: {err}"),
            }
            Ok(LineFlow::Continue)
        }
        None => {
            println!("Unknown command: {line}. Try /help.");
            Ok(LineFlow::Continue)
        }
    }
}

/// Render a [`zunel_core::ReloadReport`] as the multi-line summary
/// `/reload` prints to stdout. Kept separate so it can be unit
/// tested without spinning up a REPL or any IO.
fn format_reload_report(report: &zunel_core::ReloadReport) -> String {
    if report.attempted.is_empty() {
        return "Reload: no MCP servers configured.".into();
    }
    let mut out = String::new();
    out.push_str(&format!("Reload: attempted {}", report.attempted.len()));
    if !report.succeeded.is_empty() {
        out.push_str(&format!(
            ", succeeded {} ({})",
            report.succeeded.len(),
            report.succeeded.join(", "),
        ));
    }
    if !report.failed.is_empty() {
        out.push_str(&format!(", failed {}:", report.failed.len()));
        for (server, err) in &report.failed {
            out.push_str(&format!("\n  - {server}: {err}"));
        }
    }
    out
}

async fn run_turn(
    agent_loop: &AgentLoop,
    session_key: &str,
    message: &str,
    show_footer: bool,
) -> Result<()> {
    let (tx, rx) = mpsc::channel(64);
    let renderer = StreamingRenderer::start();
    let render_task = tokio::spawn(async move { renderer.drive(rx).await });

    // Build a fresh cancel token for this turn and arm a Ctrl+C watcher
    // against it. Reedline's `Signal::CtrlC` is only delivered while
    // `read_line` is active; once a turn is streaming, OS SIGINT goes to
    // the process and we need a `tokio::signal::ctrl_c()` watcher to
    // route it into the agent loop's cancellation token.
    let cancel = tokio_util::sync::CancellationToken::new();
    let watcher_cancel = cancel.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n^C interrupting current turn (Ctrl+C again at the prompt to exit)");
            watcher_cancel.cancel();
        }
    });

    let result = agent_loop
        .process_streamed_with_cancel(session_key, message, tx, cancel.clone())
        .await;
    // The turn is done (success, error, or cancellation). Take down the
    // signal watcher so a *future* Ctrl+C at the line-edit prompt
    // doesn't leak into the next turn's already-aborted token.
    signal_task.abort();

    render_task
        .await
        .map_err(|e| anyhow::anyhow!("render task failed: {e}"))??;

    let result = match result {
        Ok(r) => r,
        Err(zunel_core::Error::Cancelled) => {
            println!("\n(turn cancelled)");
            return Ok(());
        }
        Err(err) => return Err(anyhow::Error::new(err).context("running agent")),
    };

    if show_footer {
        let footer = zunel_core::format_footer(&result.usage, &result.session_total_usage);
        if !footer.is_empty() {
            println!("{footer}");
        }
    }
    Ok(())
}

#[cfg(unix)]
fn exec_restart() -> Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().context("locating current_exe")?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let err = std::process::Command::new(exe).args(args).exec();
    Err(anyhow::anyhow!("exec failed: {err}"))
}

#[cfg(not(unix))]
fn exec_restart() -> Result<()> {
    Err(anyhow::anyhow!("/restart is only supported on Unix"))
}
