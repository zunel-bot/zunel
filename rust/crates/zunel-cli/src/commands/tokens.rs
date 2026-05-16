//! `zunel tokens [list|show|since]` — read-only inspection of the
//! per-session usage totals persisted by `Session::record_turn_usage`.
//!
//! No LLM calls. Each subcommand walks `<workspace>/sessions/`, loads
//! each `.jsonl`, and aggregates `metadata.usage_total` /
//! `metadata.turn_usage` into either a human table or a JSON document.
//!
//! Token-count humanization (`humanize`, `format_totals`) is shared
//! with the inline footer in `zunel-core::usage_footer` so a `1.2k`
//! here matches `1.2k` in Slack/REPL footers byte-for-byte.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Local, NaiveDateTime};
use serde_json::{json, Value};
use zunel_core::{format_totals, humanize, SessionManager};
use zunel_providers::Usage;

use crate::cli::{TokensArgs, TokensCommand, TokensShowArgs, TokensSinceArgs};
use crate::commands::sessions::parse_cutoff;
use crate::commands::util::truncate;

pub async fn run(args: TokensArgs, config_path: Option<&Path>) -> Result<()> {
    let cfg = zunel_config::load_config(config_path).with_context(|| "loading zunel config")?;
    let workspace = zunel_config::workspace_path(&cfg.agents.defaults)
        .with_context(|| "resolving workspace path")?;
    zunel_util::ensure_dir(&workspace)
        .with_context(|| format!("creating workspace dir {}", workspace.display()))?;
    let manager = SessionManager::new(&workspace);

    match args.command {
        None => totals(&manager, args.json),
        Some(TokensCommand::List) => list(&manager, args.json),
        Some(TokensCommand::Show(show)) => show_session(&manager, show, args.json),
        Some(TokensCommand::Since(since)) => since_window(&manager, since, args.json),
    }
}

fn totals(manager: &SessionManager, json_out: bool) -> Result<()> {
    let rows = collect_rows(manager)?;
    let mut grand = Usage::default();
    let mut turns: u64 = 0;
    for row in &rows {
        grand += &row.total;
        turns += row.turns;
    }
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "sessions": rows.len(),
                "turns": turns,
                "prompt_tokens": grand.prompt_tokens,
                "completion_tokens": grand.completion_tokens,
                "reasoning_tokens": grand.reasoning_tokens,
                "cached_tokens": grand.cached_tokens,
                "total_tokens": grand.total(),
            }))?
        );
        return Ok(());
    }
    if rows.is_empty() {
        println!("(no sessions)");
        return Ok(());
    }
    println!(
        "{} · {} sessions · {} turns",
        format_totals(&grand),
        rows.len(),
        turns,
    );
    Ok(())
}

fn list(manager: &SessionManager, json_out: bool) -> Result<()> {
    let mut rows = collect_rows(manager)?;
    rows.sort_by_key(|r| std::cmp::Reverse(r.total.total()));
    if json_out {
        let payload: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "key": r.key,
                    "turns": r.turns,
                    "prompt_tokens": r.total.prompt_tokens,
                    "completion_tokens": r.total.completion_tokens,
                    "reasoning_tokens": r.total.reasoning_tokens,
                    "cached_tokens": r.total.cached_tokens,
                    "total_tokens": r.total.total(),
                    "last_turn": r.last_turn.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("(no sessions)");
        return Ok(());
    }
    println!(
        "{:<48}  {:>6}  {:>8}  {:>8}  {:>8}  {:>10}  {:<19}",
        "KEY", "TURNS", "IN", "OUT", "THINK", "TOTAL", "LAST TURN"
    );
    for r in rows {
        let last = r
            .last_turn
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<48}  {:>6}  {:>8}  {:>8}  {:>8}  {:>10}  {:<19}",
            truncate(&r.key, 48),
            r.turns,
            humanize(u64::from(r.total.prompt_tokens)),
            humanize(u64::from(r.total.completion_tokens)),
            humanize(u64::from(r.total.reasoning_tokens)),
            humanize(r.total.total()),
            last,
        );
    }
    Ok(())
}

fn show_session(manager: &SessionManager, args: TokensShowArgs, json_out: bool) -> Result<()> {
    let session = manager
        .load(&args.key)?
        .ok_or_else(|| anyhow!("session {} not found", args.key))?;
    let total = session.usage_total();
    let turn_usage = session.turn_usage();
    let total_turns = session.usage_turns();

    let limit = if args.all {
        turn_usage.len()
    } else {
        args.tail
    };
    let start = turn_usage.len().saturating_sub(limit);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "key": session.key(),
                "turns": total_turns,
                "prompt_tokens": total.prompt_tokens,
                "completion_tokens": total.completion_tokens,
                "reasoning_tokens": total.reasoning_tokens,
                "cached_tokens": total.cached_tokens,
                "total_tokens": total.total(),
                "rows": &turn_usage[start..],
            }))?
        );
        return Ok(());
    }
    println!(
        "{}  {} turns recorded, lifetime totals: {}",
        session.key(),
        total_turns,
        format_totals(&total),
    );
    if turn_usage.is_empty() {
        println!("(no turn-level usage recorded)");
        return Ok(());
    }
    println!(
        "{:>4}  {:<26}  {:>8}  {:>8}  {:>8}  {:>8}",
        "#", "TIMESTAMP", "IN", "OUT", "THINK", "CACHED"
    );
    for (idx, row) in turn_usage[start..].iter().enumerate() {
        let ts = row.get("ts").and_then(Value::as_str).unwrap_or("-");
        let prompt = row.get("prompt").and_then(Value::as_u64).unwrap_or(0);
        let completion = row.get("completion").and_then(Value::as_u64).unwrap_or(0);
        let reasoning = row.get("reasoning").and_then(Value::as_u64).unwrap_or(0);
        let cached = row.get("cached").and_then(Value::as_u64).unwrap_or(0);
        println!(
            "{:>4}  {:<26}  {:>8}  {:>8}  {:>8}  {:>8}",
            start + idx,
            ts,
            humanize(prompt),
            humanize(completion),
            humanize(reasoning),
            humanize(cached),
        );
    }
    Ok(())
}

fn since_window(manager: &SessionManager, args: TokensSinceArgs, json_out: bool) -> Result<()> {
    let cutoff = parse_cutoff(&args.cutoff)
        .ok_or_else(|| anyhow!("invalid cutoff {:?} (try 7d, 24h, 45m)", args.cutoff))?;
    let now = Local::now();
    let threshold = now - cutoff;
    let mut grand = Usage::default();
    let mut turns: u64 = 0;
    let mut sessions_touched: usize = 0;

    for key in manager.list_keys()? {
        let session = match manager.load(&key)? {
            Some(s) => s,
            None => continue,
        };
        let mut session_hits = 0u64;
        for row in session.turn_usage() {
            let ts = match row.get("ts").and_then(Value::as_str).and_then(parse_ts) {
                Some(t) => t,
                None => continue,
            };
            if ts < threshold {
                continue;
            }
            grand.prompt_tokens = grand
                .prompt_tokens
                .saturating_add(row.get("prompt").and_then(Value::as_u64).unwrap_or(0) as u32);
            grand.completion_tokens = grand
                .completion_tokens
                .saturating_add(row.get("completion").and_then(Value::as_u64).unwrap_or(0) as u32);
            grand.reasoning_tokens = grand
                .reasoning_tokens
                .saturating_add(row.get("reasoning").and_then(Value::as_u64).unwrap_or(0) as u32);
            grand.cached_tokens = grand
                .cached_tokens
                .saturating_add(row.get("cached").and_then(Value::as_u64).unwrap_or(0) as u32);
            session_hits += 1;
        }
        turns += session_hits;
        if session_hits > 0 {
            sessions_touched += 1;
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "cutoff": args.cutoff,
                "sessions": sessions_touched,
                "turns": turns,
                "prompt_tokens": grand.prompt_tokens,
                "completion_tokens": grand.completion_tokens,
                "reasoning_tokens": grand.reasoning_tokens,
                "cached_tokens": grand.cached_tokens,
                "total_tokens": grand.total(),
            }))?
        );
        return Ok(());
    }
    if turns == 0 {
        println!("no turns in the last {}", args.cutoff);
        return Ok(());
    }
    println!(
        "last {}: {} · {} sessions · {} turns",
        args.cutoff,
        format_totals(&grand),
        sessions_touched,
        turns,
    );
    Ok(())
}

struct UsageRow {
    key: String,
    total: Usage,
    turns: u64,
    last_turn: Option<DateTime<Local>>,
}

fn collect_rows(manager: &SessionManager) -> Result<Vec<UsageRow>> {
    let mut rows = Vec::new();
    for key in manager.list_keys()? {
        let session = match manager.load(&key)? {
            Some(s) => s,
            None => continue,
        };
        let total = session.usage_total();
        let turns = session.usage_turns();
        if total.total() == 0 && turns == 0 {
            // Sessions that pre-date the usage feature won't have
            // any totals — skip them so the table stays meaningful.
            continue;
        }
        let last_turn = session
            .turn_usage()
            .last()
            .and_then(|row| row.get("ts").and_then(Value::as_str).and_then(parse_ts));
        rows.push(UsageRow {
            key,
            total,
            turns,
            last_turn,
        });
    }
    Ok(rows)
}

fn parse_ts(raw: &str) -> Option<DateTime<Local>> {
    NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .and_then(|n| n.and_local_timezone(Local).single())
}
