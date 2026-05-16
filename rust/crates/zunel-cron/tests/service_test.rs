use serde_json::json;
use zunel_cron::CronService;

#[test]
fn service_loads_due_at_jobs_and_records_success() {
    let tmp = tempfile::tempdir().unwrap();
    let store_path = tmp.path().join("cron").join("jobs.json");
    std::fs::create_dir_all(store_path.parent().unwrap()).unwrap();
    std::fs::write(
        &store_path,
        serde_json::to_string_pretty(&json!({
            "version": 1,
            "jobs": [{
                "id": "job_1",
                "name": "one shot",
                "enabled": true,
                "schedule": {"kind": "at", "atMs": 1000},
                "payload": {"kind": "agent_turn", "message": "run", "deliver": true},
                "state": {"nextRunAtMs": 1000, "runHistory": []},
                "createdAtMs": 1,
                "updatedAtMs": 1,
                "deleteAfterRun": true
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let mut service = CronService::new(store_path.clone());
    let due = service.load_due_jobs(1_500).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, "job_1");
    assert_eq!(due[0].payload.message, "run");

    service.record_success("job_1", 1_500, 25).unwrap();

    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
    assert_eq!(saved["jobs"].as_array().unwrap().len(), 0);
}

#[test]
fn service_computes_next_every_run_after_success() {
    let tmp = tempfile::tempdir().unwrap();
    let store_path = tmp.path().join("cron").join("jobs.json");
    std::fs::create_dir_all(store_path.parent().unwrap()).unwrap();
    std::fs::write(
        &store_path,
        serde_json::to_string_pretty(&json!({
            "version": 1,
            "jobs": [{
                "id": "job_every",
                "name": "recurring",
                "enabled": true,
                "schedule": {"kind": "every", "everyMs": 500},
                "payload": {"kind": "agent_turn", "message": "tick", "deliver": false},
                "state": {"nextRunAtMs": 1000, "runHistory": []},
                "createdAtMs": 1,
                "updatedAtMs": 1,
                "deleteAfterRun": false
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let mut service = CronService::new(store_path.clone());
    assert_eq!(service.load_due_jobs(1_000).unwrap()[0].id, "job_every");
    service.record_success("job_every", 1_000, 10).unwrap();

    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
    assert_eq!(saved["jobs"][0]["state"]["nextRunAtMs"], 1_500);
    assert_eq!(saved["jobs"][0]["state"]["lastStatus"], "ok");
}

#[test]
fn service_computes_next_expr_run_with_timezone() {
    let tmp = tempfile::tempdir().unwrap();
    let store_path = tmp.path().join("cron").join("jobs.json");
    std::fs::create_dir_all(store_path.parent().unwrap()).unwrap();
    std::fs::write(
        &store_path,
        serde_json::to_string_pretty(&json!({
            "version": 1,
            "jobs": [{
                "id": "job_expr",
                "name": "cron expression",
                "enabled": true,
                "schedule": {"kind": "expr", "expr": "*/5 * * * *", "tz": "America/Los_Angeles"},
                "payload": {"kind": "agent_turn", "message": "tick", "deliver": false},
                "state": {"runHistory": []},
                "createdAtMs": 1,
                "updatedAtMs": 1,
                "deleteAfterRun": false
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let mut service = CronService::new(store_path.clone());
    let due = service.load_due_jobs(1_713_974_100_000).unwrap();
    assert!(due.is_empty());

    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
    assert_eq!(
        saved["jobs"][0]["state"]["nextRunAtMs"],
        1_713_974_400_000u64
    );

    let due = service.load_due_jobs(1_713_974_400_000).unwrap();
    assert_eq!(due[0].id, "job_expr");

    service
        .record_success("job_expr", 1_713_974_400_000, 10)
        .unwrap();
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
    assert_eq!(
        saved["jobs"][0]["state"]["nextRunAtMs"],
        1_713_974_700_000u64
    );
}

#[test]
fn service_runs_due_jobs_once_and_records_success_and_error() {
    let tmp = tempfile::tempdir().unwrap();
    let store_path = tmp.path().join("cron").join("jobs.json");
    std::fs::create_dir_all(store_path.parent().unwrap()).unwrap();
    std::fs::write(
        &store_path,
        serde_json::to_string_pretty(&json!({
            "version": 1,
            "jobs": [
                {
                    "id": "job_ok",
                    "name": "ok",
                    "enabled": true,
                    "schedule": {"kind": "every", "everyMs": 500},
                    "payload": {"kind": "agent_turn", "message": "ok", "deliver": false},
                    "state": {"nextRunAtMs": 1000, "runHistory": []},
                    "createdAtMs": 1,
                    "updatedAtMs": 1,
                    "deleteAfterRun": false
                },
                {
                    "id": "job_err",
                    "name": "err",
                    "enabled": true,
                    "schedule": {"kind": "every", "everyMs": 500},
                    "payload": {"kind": "agent_turn", "message": "err", "deliver": false},
                    "state": {"nextRunAtMs": 1000, "runHistory": []},
                    "createdAtMs": 1,
                    "updatedAtMs": 1,
                    "deleteAfterRun": false
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let mut service = CronService::new(store_path.clone());
    let outcomes = service
        .run_due_jobs_once(1_000, |job| {
            if job.id == "job_err" {
                Err("boom".to_string())
            } else {
                Ok(())
            }
        })
        .unwrap();

    assert_eq!(outcomes.len(), 2);
    assert!(outcomes
        .iter()
        .any(|outcome| outcome.job_id == "job_ok" && outcome.ok));
    assert!(outcomes
        .iter()
        .any(|outcome| outcome.job_id == "job_err" && !outcome.ok));

    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
    let ok = saved["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["id"] == "job_ok")
        .unwrap();
    let err = saved["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["id"] == "job_err")
        .unwrap();
    assert_eq!(ok["state"]["lastStatus"], "ok");
    assert_eq!(ok["state"]["nextRunAtMs"], 1_500);
    assert_eq!(err["state"]["lastStatus"], "error");
    assert_eq!(err["state"]["lastError"], "boom");
    assert_eq!(err["state"]["nextRunAtMs"], 1_500);
}
