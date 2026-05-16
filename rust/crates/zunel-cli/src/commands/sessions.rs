//! `zunel sessions {list,show,clear,compact,prune}` — operator tools
//! for keeping persisted chat sessions trim.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Local};
use serde_json::Value;
use zunel_core::{CompactionService, Session, SessionManager};
use zunel_providers::LLMProvider;

use crate::cli::{
    SessionsArgs, SessionsClearArgs, SessionsCommand, SessionsCompactArgs, SessionsPruneArgs,
    SessionsShowArgs,
};
use crate::commands::util::truncate;

pub async fn run(args: SessionsArgs, config_path: Option<&Path>) -> Result<()> {
    let cfg = zunel_config::load_config(config_path).with_context(|| "loading zunel config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults)
        .with_context(|| "resolving workspace path")?;
    zunel_util::ensure_dir(&workspace)
        .with_context(|| format!("creating workspace dir {}", workspace.display()))?;
    let manager = SessionManager::new(&workspace);

    match args.command {
        SessionsCommand::List => list_sessions(&manager),
        SessionsCommand::Show(args) => show_session(&manager, args),
        SessionsCommand::Clear(args) => clear_session(&manager, args),
        SessionsCommand::Compact(args) => compact_session(&manager, &cfg, args).await,
        SessionsCommand::Prune(args) => prune_sessions(&manager, args),
    }
}

fn list_sessions(manager: &SessionManager) -> Result<()> {
    let mut rows: Vec<SessionRow> = Vec::new();
    for key in manager.list_keys()? {
        match build_row(manager, &key) {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => {}
            Err(err) => eprintln!("warning: failed to inspect {key}: {err}"),
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.bytes));

    if rows.is_empty() {
        println!("(no sessions)");
        return Ok(());
    }

    println!(
        "{:<48}  {:>8}  {:>10}  {:<26}  {:>17}",
        "KEY", "MSGS", "BYTES", "LAST USER TURN", "LAST CONSOLIDATED"
    );
    for row in rows {
        let last_turn = row
            .last_user_turn
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<48}  {:>8}  {:>10}  {:<26}  {:>17}",
            truncate(&row.key, 48),
            row.messages,
            row.bytes,
            last_turn,
            row.last_consolidated,
        );
    }
    Ok(())
}

fn show_session(manager: &SessionManager, args: SessionsShowArgs) -> Result<()> {
    let session = manager
        .load(&args.key)?
        .ok_or_else(|| anyhow!("session {} not found", args.key))?;
    let total = session.messages().len();
    let start = total.saturating_sub(args.tail);
    println!(
        "{}  {} messages, last_consolidated={}, showing rows {}..{}",
        session.key(),
        total,
        session.last_consolidated(),
        start,
        total
    );
    for (idx, msg) in session.messages().iter().enumerate().skip(start) {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("?");
        let timestamp = msg.get("timestamp").and_then(Value::as_str).unwrap_or("");
        let content = msg
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("(structured content)");
        let snippet = first_chars(content, 200);
        println!("[{idx:>4}] {role:<9} {timestamp}  {snippet}");
    }
    Ok(())
}

fn clear_session(manager: &SessionManager, args: SessionsClearArgs) -> Result<()> {
    if !args.yes {
        eprint!("clear session {}? [y/N] ", args.key);
        io::stderr().flush().ok();
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        let answer = buf.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            println!("aborted");
            return Ok(());
        }
    }
    let mut session = manager
        .load(&args.key)?
        .unwrap_or_else(|| Session::new(&args.key));
    let before = session.messages().len();
    session.clear();
    manager.save(&session)?;
    println!("cleared {} ({before} messages removed)", args.key);
    Ok(())
}

async fn compact_session(
    manager: &SessionManager,
    cfg: &zunel_config::Config,
    args: SessionsCompactArgs,
) -> Result<()> {
    let mut session = manager
        .load(&args.key)?
        .ok_or_else(|| anyhow!("session {} not found", args.key))?;
    let before_msgs = session.messages().len();
    let before_bytes = file_bytes(&manager.path_for(&args.key)).unwrap_or(0);

    let model = args
        .model
        .clone()
        .or_else(|| cfg.agents.defaults.compaction_model.clone())
        .unwrap_or_else(|| cfg.agents.defaults.model.clone());
    if model.is_empty() {
        bail!(
            "no compaction model configured; set agents.defaults.compaction_model or pass --model"
        );
    }

    let provider: Arc<dyn LLMProvider> = zunel_providers::build_provider(cfg)
        .await
        .with_context(|| "building provider")?;
    let svc = CompactionService::new(provider, model.clone());
    let collapsed = svc
        .compact_session(&mut session, args.keep)
        .await
        .with_context(|| "running compaction")?;
    if collapsed == 0 {
        println!(
            "nothing to compact: {} messages already at or under keep_tail={}",
            before_msgs, args.keep
        );
        return Ok(());
    }
    manager.save(&session)?;
    let after_msgs = session.messages().len();
    let after_bytes = file_bytes(&manager.path_for(&args.key)).unwrap_or(0);
    println!(
        "compacted {key}: {before_msgs} → {after_msgs} messages ({collapsed} collapsed), \
         {before_bytes} → {after_bytes} bytes, model={model}",
        key = args.key,
    );
    Ok(())
}

fn prune_sessions(manager: &SessionManager, args: SessionsPruneArgs) -> Result<()> {
    let cutoff = parse_cutoff(&args.older_than).ok_or_else(|| {
        anyhow!(
            "invalid --older-than {:?} (try 30d, 12h, 45m)",
            args.older_than
        )
    })?;
    let now = Local::now();
    let mut deleted = 0usize;
    for key in manager.list_keys()? {
        let row = match build_row(manager, &key) {
            Ok(Some(row)) => row,
            _ => continue,
        };
        let last = row.last_user_turn.unwrap_or(row.modified);
        let age = now - last;
        if age < cutoff {
            continue;
        }
        if args.dry_run {
            println!(
                "would delete {key} (last activity {})",
                last.format("%Y-%m-%d %H:%M:%S")
            );
            deleted += 1;
            continue;
        }
        if manager.delete(&key)? {
            println!(
                "deleted {key} (last activity {})",
                last.format("%Y-%m-%d %H:%M:%S")
            );
            deleted += 1;
        }
    }
    if deleted == 0 {
        println!("no sessions older than {}", args.older_than);
    } else if args.dry_run {
        println!("dry run: {deleted} session(s) would be deleted");
    } else {
        println!("pruned {deleted} session(s)");
    }
    Ok(())
}

struct SessionRow {
    key: String,
    messages: usize,
    bytes: u64,
    last_user_turn: Option<DateTime<Local>>,
    last_consolidated: usize,
    modified: DateTime<Local>,
}

fn build_row(manager: &SessionManager, key: &str) -> Result<Option<SessionRow>> {
    let path = manager.path_for(key);
    let bytes = file_bytes(&path).unwrap_or(0);
    let modified = file_modified(&path).unwrap_or_else(Local::now);
    let session = match manager.load(key)? {
        Some(s) => s,
        None => return Ok(None),
    };
    Ok(Some(SessionRow {
        key: key.to_string(),
        messages: session.messages().len(),
        bytes,
        last_user_turn: session.last_user_turn_at(),
        last_consolidated: session.last_consolidated(),
        modified,
    }))
}

fn file_bytes(path: &PathBuf) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

fn file_modified(path: &PathBuf) -> Option<DateTime<Local>> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    Some(DateTime::<Local>::from(modified))
}

pub(crate) fn parse_cutoff(raw: &str) -> Option<chrono::Duration> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (num, unit) = raw.split_at(raw.len() - 1);
    let n: i64 = num.parse().ok()?;
    if n <= 0 {
        return None;
    }
    match unit {
        "d" | "D" => Some(chrono::Duration::days(n)),
        "h" | "H" => Some(chrono::Duration::hours(n)),
        "m" | "M" => Some(chrono::Duration::minutes(n)),
        _ => None,
    }
}

fn first_chars(s: &str, n: usize) -> String {
    let cleaned = s.replace('\n', " ");
    if cleaned.chars().count() <= n {
        cleaned
    } else {
        let mut out: String = cleaned.chars().take(n).collect();
        out.push('…');
        out
    }
}
