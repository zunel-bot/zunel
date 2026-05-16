use serde_json::json;
use tempfile::tempdir;

use zunel_tools::{
    path_policy::PathPolicy,
    search::{GlobTool, GrepTool},
    Tool, ToolContext,
};

fn ctx(ws: &std::path::Path) -> ToolContext {
    ToolContext::new_with_workspace(ws.to_path_buf(), "cli:direct".into())
}

#[tokio::test]
async fn glob_matches_by_pattern() {
    let ws = tempdir().unwrap();
    std::fs::write(ws.path().join("a.rs"), "").unwrap();
    std::fs::write(ws.path().join("b.rs"), "").unwrap();
    std::fs::write(ws.path().join("c.txt"), "").unwrap();
    let tool = GlobTool::new(PathPolicy::restricted(ws.path()));
    let res = tool
        .execute(json!({"pattern": "*.rs"}), &ctx(ws.path()))
        .await;
    assert!(!res.is_error);
    assert!(res.content.contains("a.rs"));
    assert!(res.content.contains("b.rs"));
    assert!(!res.content.contains("c.txt"));
}

#[tokio::test]
async fn glob_respects_gitignore() {
    let ws = tempdir().unwrap();
    std::fs::write(ws.path().join(".gitignore"), "ignored/\n").unwrap();
    std::fs::create_dir(ws.path().join("ignored")).unwrap();
    std::fs::write(ws.path().join("ignored/hidden.rs"), "").unwrap();
    std::fs::write(ws.path().join("top.rs"), "").unwrap();

    let tool = GlobTool::new(PathPolicy::restricted(ws.path()));
    let res = tool
        .execute(json!({"pattern": "**/*.rs"}), &ctx(ws.path()))
        .await;
    assert!(res.content.contains("top.rs"));
    assert!(!res.content.contains("hidden.rs"));
}

#[tokio::test]
async fn grep_finds_lines_containing_pattern() {
    let ws = tempdir().unwrap();
    std::fs::write(ws.path().join("a.txt"), "one\ntwo\nthree two\nfour\n").unwrap();
    let tool = GrepTool::new(PathPolicy::restricted(ws.path()));
    let res = tool
        .execute(json!({"pattern": "two"}), &ctx(ws.path()))
        .await;
    assert!(!res.is_error);
    assert!(res.content.contains("two"));
    assert!(res.content.contains("three two"));
    assert!(!res.content.contains("four"));
}

#[tokio::test]
async fn grep_includes_line_numbers() {
    let ws = tempdir().unwrap();
    std::fs::write(ws.path().join("a.txt"), "alpha\nbravo\ncharlie\n").unwrap();
    let tool = GrepTool::new(PathPolicy::restricted(ws.path()));
    let res = tool
        .execute(json!({"pattern": "bravo"}), &ctx(ws.path()))
        .await;
    // Format: "a.txt:2:bravo"
    assert!(res.content.contains(":2:"));
}
