use serde_json::json;
use tempfile::tempdir;

use zunel_tools::{
    fs::{ListDirTool, ReadFileTool, WriteFileTool},
    path_policy::PathPolicy,
    Tool, ToolContext,
};

fn ctx(ws: &std::path::Path) -> ToolContext {
    ToolContext::new_with_workspace(ws.to_path_buf(), "cli:direct".into())
}

#[tokio::test]
async fn read_file_returns_contents() {
    let ws = tempdir().unwrap();
    std::fs::write(ws.path().join("note.txt"), "hello\nworld\n").unwrap();

    let tool = ReadFileTool::new(PathPolicy::restricted(ws.path()));
    let result = tool
        .execute(json!({"path": "note.txt"}), &ctx(ws.path()))
        .await;
    assert!(!result.is_error, "{result:?}");
    assert!(result.content.contains("hello"));
    assert!(result.content.contains("world"));
}

#[tokio::test]
async fn read_file_respects_workspace_policy() {
    let ws = tempdir().unwrap();
    let other = tempdir().unwrap();
    std::fs::write(other.path().join("secret.txt"), "nope").unwrap();

    let tool = ReadFileTool::new(PathPolicy::restricted(ws.path()));
    let result = tool
        .execute(
            json!({"path": other.path().join("secret.txt").display().to_string()}),
            &ctx(ws.path()),
        )
        .await;
    assert!(result.is_error, "expected policy violation: {result:?}");
    assert!(result.content.contains("outside workspace"));
}

#[tokio::test]
async fn write_file_creates_file_then_read_returns_same_content() {
    let ws = tempdir().unwrap();
    let writer = WriteFileTool::new(PathPolicy::restricted(ws.path()));
    let reader = ReadFileTool::new(PathPolicy::restricted(ws.path()));

    let write_result = writer
        .execute(
            json!({"path": "out.txt", "content": "written body"}),
            &ctx(ws.path()),
        )
        .await;
    assert!(!write_result.is_error);

    let read_result = reader
        .execute(json!({"path": "out.txt"}), &ctx(ws.path()))
        .await;
    assert!(read_result.content.contains("written body"));
}

#[tokio::test]
async fn list_dir_enumerates_files_and_dirs() {
    let ws = tempdir().unwrap();
    std::fs::write(ws.path().join("a.txt"), "").unwrap();
    std::fs::write(ws.path().join("b.txt"), "").unwrap();
    std::fs::create_dir(ws.path().join("sub")).unwrap();

    let tool = ListDirTool::new(PathPolicy::restricted(ws.path()));
    let res = tool.execute(json!({"path": "."}), &ctx(ws.path())).await;
    assert!(!res.is_error);
    assert!(res.content.contains("a.txt"));
    assert!(res.content.contains("b.txt"));
    assert!(res.content.contains("sub/"));
}

#[tokio::test]
async fn read_file_pagination_is_inclusive_and_caps_lines() {
    let ws = tempdir().unwrap();
    let mut body = String::new();
    for i in 0..50 {
        body.push_str(&format!("line {i}\n"));
    }
    std::fs::write(ws.path().join("big.txt"), &body).unwrap();

    let tool = ReadFileTool::new(PathPolicy::restricted(ws.path()));
    let res = tool
        .execute(
            json!({"path": "big.txt", "offset": 10, "limit": 3}),
            &ctx(ws.path()),
        )
        .await;
    assert!(res.content.contains("line 10"));
    assert!(res.content.contains("line 11"));
    assert!(res.content.contains("line 12"));
    assert!(!res.content.contains("line 13"));
}
