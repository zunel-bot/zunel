use serde_json::json;
use tempfile::tempdir;
use zunel_tools::cron::CronTool;
use zunel_tools::{Tool, ToolContext};

#[tokio::test]
async fn cron_add_list_and_remove_job() {
    let dir = tempdir().unwrap();
    let state = dir.path().join("cron.json");
    let tool = CronTool::new(state.clone(), "UTC");
    let ctx = ToolContext::for_test();

    let created = tool
        .execute(
            json!({"action": "add", "name": "standup", "message": "remind me", "every_seconds": 60}),
            &ctx,
        )
        .await;
    assert!(!created.is_error, "{}", created.content);
    assert!(created.content.contains("Created job 'standup'"));
    let job_id = created
        .content
        .split("id: ")
        .nth(1)
        .unwrap()
        .trim_end_matches(')');
    let store: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(store["version"], 1);
    assert_eq!(store["jobs"][0]["payload"]["message"], "remind me");
    assert_eq!(store["jobs"][0]["schedule"]["everyMs"], 60000);

    let listed = tool.execute(json!({"action": "list"}), &ctx).await;
    assert!(listed.content.contains("standup"));
    assert!(listed.content.contains("every 60s"));

    let removed = tool
        .execute(json!({"action": "remove", "job_id": job_id}), &ctx)
        .await;
    assert!(!removed.is_error, "{}", removed.content);
    assert!(removed.content.contains("Removed job"));

    let listed = tool.execute(json!({"action": "list"}), &ctx).await;
    assert_eq!(listed.content, "No scheduled jobs.");
}

#[tokio::test]
async fn cron_add_requires_message_and_schedule() {
    let dir = tempdir().unwrap();
    let tool = CronTool::new(dir.path().join("cron.json"), "UTC");
    let ctx = ToolContext::for_test();

    let missing_message = tool
        .execute(json!({"action": "add", "every_seconds": 60}), &ctx)
        .await;
    assert!(missing_message.is_error);
    assert!(missing_message.content.contains("message"));

    let missing_schedule = tool
        .execute(json!({"action": "add", "message": "hello"}), &ctx)
        .await;
    assert!(missing_schedule.is_error);
    assert!(missing_schedule.content.contains("every_seconds"));
}

#[tokio::test]
async fn cron_refuses_to_remove_protected_system_job() {
    let dir = tempdir().unwrap();
    let state = dir.path().join("cron.json");
    std::fs::write(
        &state,
        r#"{
          "version": 1,
          "jobs": [{
            "id": "dream",
            "name": "dream",
            "enabled": true,
            "schedule": {"kind": "every", "everyMs": 3600000},
            "payload": {"kind": "system_event", "message": "system", "deliver": false},
            "state": {},
            "createdAtMs": 0,
            "updatedAtMs": 0,
            "deleteAfterRun": false
          }]
        }"#,
    )
    .unwrap();
    let tool = CronTool::new(state, "UTC");

    let result = tool
        .execute(
            json!({"action": "remove", "job_id": "dream"}),
            &ToolContext::for_test(),
        )
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("Protected"));
}
