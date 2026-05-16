use serde_json::json;
use tempfile::tempdir;

use zunel_context::ContextBuilder;
use zunel_skills::SkillsLoader;

fn builder(workspace: &std::path::Path) -> ContextBuilder {
    let skills = SkillsLoader::new(workspace, None, &[]);
    ContextBuilder::new(workspace.to_path_buf(), skills)
}

#[test]
fn system_prompt_contains_identity_and_skills_header_when_no_skills_present() {
    let tmp = tempdir().unwrap();
    let b = builder(tmp.path());
    let prompt = b.build_system_prompt(None).unwrap();
    // Identity uses the Python-parity heading scheme.
    assert!(
        prompt.contains("## Runtime") && prompt.contains("## Workspace"),
        "prompt did not include identity: {prompt}"
    );
    assert!(prompt.contains("## Platform Policy"));
    // No active-skills section when none are always-on.
    assert!(!prompt.contains("# Active Skills"));
}

#[test]
fn system_prompt_includes_bootstrap_files_when_present() {
    let tmp = tempdir().unwrap();
    std::fs::write(tmp.path().join("AGENTS.md"), "# AGENTS\nProject rules.\n").unwrap();
    std::fs::write(tmp.path().join("SOUL.md"), "# SOUL\nTone.\n").unwrap();
    let b = builder(tmp.path());
    let prompt = b.build_system_prompt(None).unwrap();
    assert!(prompt.contains("## AGENTS.md"));
    assert!(prompt.contains("Project rules."));
    assert!(prompt.contains("## SOUL.md"));
    assert!(prompt.contains("Tone."));
}

#[test]
fn build_messages_prepends_system_and_appends_user_turn() {
    let tmp = tempdir().unwrap();
    let b = builder(tmp.path());
    let history = vec![json!({"role": "user", "content": "hi"})];
    let msgs = b
        .build_messages(
            &history,
            "new message",
            None,
            Some("cli"),
            Some("direct"),
            "user",
            None,
        )
        .unwrap();
    assert_eq!(msgs[0]["role"].as_str(), Some("system"));
    let last = &msgs[msgs.len() - 1];
    let content = last["content"].as_str().unwrap();
    assert!(content.contains("new message"));
}

#[test]
fn build_messages_merges_consecutive_user_messages() {
    let tmp = tempdir().unwrap();
    let b = builder(tmp.path());
    let history = vec![json!({"role": "user", "content": "first"})];
    let msgs = b
        .build_messages(&history, "second", None, None, None, "user", None)
        .unwrap();
    // system + (one merged user) = 2 total
    assert_eq!(msgs.len(), 2);
    assert!(msgs[1]["content"].as_str().unwrap().contains("first"));
    assert!(msgs[1]["content"].as_str().unwrap().contains("second"));
}

#[test]
fn runtime_context_tag_is_present_and_stripable() {
    let tmp = tempdir().unwrap();
    let b = builder(tmp.path());
    let msgs = b
        .build_messages(
            &[],
            "hello",
            None,
            Some("cli"),
            Some("direct"),
            "user",
            None,
        )
        .unwrap();
    let user = msgs.last().unwrap();
    let content = user["content"].as_str().unwrap();
    assert!(
        content.contains("[Runtime Context"),
        "missing tag: {content}"
    );
    assert!(content.contains("[/Runtime Context]"));
    let stripped = zunel_context::strip_runtime_context(content);
    assert_eq!(stripped, "hello");
}
