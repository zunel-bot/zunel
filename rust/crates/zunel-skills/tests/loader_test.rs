use std::fs;

use tempfile::tempdir;

use zunel_skills::{SkillsLoader, EMBEDDED_BUILTIN_LABEL};

fn write_skill(dir: &std::path::Path, name: &str, contents: &str) {
    let skill_dir = dir.join("skills").join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("SKILL.md"), contents).unwrap();
}

/// Names of every embedded builtin shipped under `builtins/`. Tests
/// that assert on exact skill counts or names need to silence these
/// so the assertions stay focused on user-supplied skills.
fn all_embedded_builtins() -> Vec<String> {
    vec!["mcp-oauth-login".to_string(), "gitlab-mr-write".to_string()]
}

fn disabled_with_builtins(extra: &[&str]) -> Vec<String> {
    let mut v = all_embedded_builtins();
    v.extend(extra.iter().map(|s| (*s).to_string()));
    v
}

#[test]
fn lists_user_skills_from_workspace() {
    let tmp = tempdir().unwrap();
    write_skill(
        tmp.path(),
        "greet",
        "---\ndescription: Says hi.\n---\n\nHello world.\n",
    );
    // Disable embedded builtins so this test is purely about user skills.
    let loader = SkillsLoader::new(tmp.path(), None, &all_embedded_builtins());
    let skills = loader.list_skills(true).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "greet");
    assert_eq!(skills[0].description, "Says hi.");
}

#[test]
fn builtin_skills_are_loaded_after_user_skills_and_deduplicated() {
    let ws = tempdir().unwrap();
    let builtin = tempdir().unwrap();
    write_skill(
        ws.path(),
        "greet",
        "---\ndescription: User.\n---\n\nUser body.\n",
    );
    write_skill(
        builtin.path(),
        "greet",
        "---\ndescription: Builtin.\n---\n\nBuiltin body.\n",
    );
    write_skill(builtin.path(), "wave", "---\ndescription: Wave.\n---\n\n");

    let loader = SkillsLoader::new(ws.path(), Some(builtin.path()), &all_embedded_builtins());
    let skills = loader.list_skills(true).unwrap();
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["greet", "wave"]);
    assert_eq!(skills[0].description, "User.");
}

#[test]
fn always_skills_are_derived_from_metadata() {
    let tmp = tempdir().unwrap();
    write_skill(
        tmp.path(),
        "watcher",
        "---\ndescription: Always-on.\nmetadata:\n  zunel:\n    always: true\n---\n\nWatcher body.",
    );
    write_skill(
        tmp.path(),
        "ondemand",
        "---\ndescription: On demand.\n---\n\n",
    );
    let loader = SkillsLoader::new(tmp.path(), None, &[]);
    let always = loader.get_always_skills().unwrap();
    assert_eq!(always, vec!["watcher".to_string()]);
}

#[test]
fn load_skills_for_context_concatenates_with_delimiter() {
    let tmp = tempdir().unwrap();
    write_skill(tmp.path(), "a", "---\ndescription: A.\n---\nBody A.");
    write_skill(tmp.path(), "b", "---\ndescription: B.\n---\nBody B.");
    let loader = SkillsLoader::new(tmp.path(), None, &[]);
    let blob = loader
        .load_skills_for_context(&["a".to_string(), "b".to_string()])
        .unwrap();
    assert!(blob.contains("### Skill: a"));
    assert!(blob.contains("### Skill: b"));
    assert!(blob.contains("\n\n---\n\n"));
    assert!(blob.contains("Body A."));
    assert!(blob.contains("Body B."));
    // Frontmatter is stripped.
    assert!(!blob.contains("description: A."));
}

#[test]
fn build_skills_summary_uses_expected_format() {
    let tmp = tempdir().unwrap();
    write_skill(tmp.path(), "hello", "---\ndescription: Hi.\n---\n\n");
    // Silence embedded builtins so the assertion is anchored on our
    // user-supplied skill regardless of alphabetical interleaving.
    let loader = SkillsLoader::new(tmp.path(), None, &all_embedded_builtins());
    let summary = loader.build_skills_summary(None).unwrap();
    assert!(summary.starts_with("- **hello** — Hi."));
    assert!(summary.contains("`"));
}

#[test]
fn disabled_skills_are_omitted() {
    let tmp = tempdir().unwrap();
    write_skill(tmp.path(), "keep", "---\ndescription: K.\n---\n\n");
    write_skill(tmp.path(), "skip", "---\ndescription: S.\n---\n\n");
    let loader = SkillsLoader::new(tmp.path(), None, &disabled_with_builtins(&["skip"]));
    let names: Vec<String> = loader
        .list_skills(true)
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, vec!["keep".to_string()]);
}

#[test]
fn embedded_builtin_mcp_oauth_login_is_visible() {
    // The crate ships `builtins/mcp-oauth-login/SKILL.md`; with no user
    // workspace skills, that builtin should appear in the listing and be
    // reachable via `load_skill`.
    let tmp = tempdir().unwrap();
    let loader = SkillsLoader::new(tmp.path(), None, &[]);
    let skills = loader.list_skills(false).unwrap();
    let oauth = skills
        .iter()
        .find(|s| s.name == "mcp-oauth-login")
        .expect("embedded mcp-oauth-login skill should be listed");
    assert!(
        oauth
            .path
            .display()
            .to_string()
            .contains(EMBEDDED_BUILTIN_LABEL),
        "embedded skills should label themselves as builtin: {}",
        oauth.path.display()
    );
    let body = loader
        .load_skill("mcp-oauth-login")
        .unwrap()
        .expect("should resolve embedded skill body");
    assert!(
        body.contains("MCP_AUTH_REQUIRED:"),
        "body must explain the contract"
    );
    assert!(
        body.contains("mcp_login_start"),
        "body must reference the tool"
    );
}

#[test]
fn embedded_builtin_gitlab_mr_write_is_visible_and_gated_on_glab() {
    // The crate ships `builtins/gitlab-mr-write/SKILL.md`. It must be
    // listed (regardless of availability) and its body must mention the
    // `glab` CLI so the agent knows what tool to shell out to. The
    // skill is gated on `glab` being installed via `requires.bins`, so
    // its `available` flag depends on the test runner's PATH; we don't
    // assert on it here.
    let tmp = tempdir().unwrap();
    let loader = SkillsLoader::new(tmp.path(), None, &[]);
    let skills = loader.list_skills(false).unwrap();
    let gitlab = skills
        .iter()
        .find(|s| s.name == "gitlab-mr-write")
        .expect("embedded gitlab-mr-write skill should be listed");
    assert!(
        gitlab
            .path
            .display()
            .to_string()
            .contains(EMBEDDED_BUILTIN_LABEL),
        "embedded skills should label themselves as builtin: {}",
        gitlab.path.display()
    );
    assert_eq!(
        gitlab.parsed_metadata.bins,
        vec!["glab".to_string()],
        "gitlab-mr-write must self-disable when `glab` is missing"
    );
    let body = loader
        .load_skill("gitlab-mr-write")
        .unwrap()
        .expect("should resolve embedded skill body");
    assert!(
        body.contains("glab mr approve"),
        "body must teach the approve command"
    );
    assert!(
        body.contains("discussions"),
        "body must teach the threaded-reply API"
    );
}

#[test]
fn user_skills_override_embedded_builtins_with_same_name() {
    let ws = tempdir().unwrap();
    write_skill(
        ws.path(),
        "mcp-oauth-login",
        "---\ndescription: Custom override.\n---\n\nUser body.\n",
    );
    let loader = SkillsLoader::new(ws.path(), None, &[]);
    let oauth = loader
        .list_skills(true)
        .unwrap()
        .into_iter()
        .find(|s| s.name == "mcp-oauth-login")
        .expect("user override should still be present");
    assert_eq!(oauth.description, "Custom override.");
    assert!(
        !oauth
            .path
            .display()
            .to_string()
            .contains(EMBEDDED_BUILTIN_LABEL),
        "user-overridden skill must point at the workspace path"
    );
    let body = loader.load_skill("mcp-oauth-login").unwrap().expect("body");
    assert!(body.contains("User body."));
}

#[test]
fn metadata_as_json_string_is_still_parsed() {
    // Python accepts `metadata: "<json string>"` for zunel/openclaw
    // namespace; match that fallback so hand-edited skills keep working.
    let tmp = tempdir().unwrap();
    write_skill(
        tmp.path(),
        "alt",
        "---\ndescription: Alt.\nmetadata: '{\"zunel\":{\"always\":true}}'\n---\n\nbody",
    );
    let loader = SkillsLoader::new(tmp.path(), None, &[]);
    let always = loader.get_always_skills().unwrap();
    assert_eq!(always, vec!["alt".to_string()]);
}
