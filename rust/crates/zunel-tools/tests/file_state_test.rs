use serde_json::json;
use tempfile::tempdir;

use zunel_tools::{
    fs::{EditFileTool, ReadFileTool, WriteFileTool},
    path_policy::PathPolicy,
    Tool, ToolContext,
};

fn ctx(ws: &std::path::Path) -> ToolContext {
    ToolContext::new_with_workspace(ws.to_path_buf(), "cli:direct".into())
}

#[tokio::test]
async fn edit_file_without_prior_read_is_rejected() {
    let ws = tempdir().unwrap();
    std::fs::write(ws.path().join("f.txt"), "hello\n").unwrap();

    let edit = EditFileTool::new(PathPolicy::restricted(ws.path()));
    let result = edit
        .execute(
            json!({"path": "f.txt", "old": "hello", "new": "bye"}),
            &ctx(ws.path()),
        )
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("read_file") || result.content.contains("read first"));
}

#[tokio::test]
async fn edit_file_after_read_succeeds_and_replaces() {
    let ws = tempdir().unwrap();
    std::fs::write(ws.path().join("f.txt"), "hello\nworld\n").unwrap();

    let ctx = ctx(ws.path());
    let reader = ReadFileTool::new(PathPolicy::restricted(ws.path()));
    let edit = EditFileTool::new(PathPolicy::restricted(ws.path()));

    let _ = reader.execute(json!({"path": "f.txt"}), &ctx).await;

    let result = edit
        .execute(json!({"path": "f.txt", "old": "hello", "new": "bye"}), &ctx)
        .await;
    assert!(!result.is_error, "{result:?}");
    let on_disk = std::fs::read_to_string(ws.path().join("f.txt")).unwrap();
    assert_eq!(on_disk, "bye\nworld\n");
}

#[tokio::test]
async fn edit_file_rejects_non_unique_match() {
    let ws = tempdir().unwrap();
    std::fs::write(ws.path().join("f.txt"), "abc\nabc\n").unwrap();
    let ctx = ctx(ws.path());
    let reader = ReadFileTool::new(PathPolicy::restricted(ws.path()));
    let edit = EditFileTool::new(PathPolicy::restricted(ws.path()));
    let _ = reader.execute(json!({"path": "f.txt"}), &ctx).await;

    let result = edit
        .execute(json!({"path": "f.txt", "old": "abc", "new": "xyz"}), &ctx)
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("multiple"));
}

#[tokio::test]
async fn write_file_resets_stale_state_so_edit_requires_reread() {
    let ws = tempdir().unwrap();
    std::fs::write(ws.path().join("f.txt"), "one\n").unwrap();
    let ctx = ctx(ws.path());
    let reader = ReadFileTool::new(PathPolicy::restricted(ws.path()));
    let writer = WriteFileTool::new(PathPolicy::restricted(ws.path()));
    let edit = EditFileTool::new(PathPolicy::restricted(ws.path()));

    let _ = reader.execute(json!({"path": "f.txt"}), &ctx).await;
    let _ = writer
        .execute(json!({"path": "f.txt", "content": "two\n"}), &ctx)
        .await;

    let result = edit
        .execute(json!({"path": "f.txt", "old": "two", "new": "three"}), &ctx)
        .await;
    assert!(
        result.is_error,
        "write_file should invalidate read-state: {result:?}"
    );
}
