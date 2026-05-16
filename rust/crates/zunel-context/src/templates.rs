use std::sync::OnceLock;

use minijinja::{Environment, Value};

static IDENTITY: &str = include_str!("../templates/identity.md");
static PLATFORM_POLICY: &str = include_str!("../templates/platform_policy.md");
static SKILLS_SECTION: &str = include_str!("../templates/skills_section.md");
static MAX_ITERATIONS_MESSAGE: &str = include_str!("../templates/max_iterations_message.md");
static UNTRUSTED_CONTENT: &str = include_str!("../templates/untrusted_content.md");

fn env() -> &'static Environment<'static> {
    static ENV: OnceLock<Environment<'static>> = OnceLock::new();
    ENV.get_or_init(|| {
        let mut e = Environment::new();
        // trim_blocks + lstrip_blocks keeps the rendered prompt text
        // free of stray blank lines around `{% if %}` blocks.
        e.set_trim_blocks(true);
        e.set_lstrip_blocks(true);
        e.add_template("identity", IDENTITY)
            .expect("identity template compiles");
        e.add_template("platform_policy", PLATFORM_POLICY)
            .expect("policy template compiles");
        e.add_template("skills_section", SKILLS_SECTION)
            .expect("skills template compiles");
        e.add_template("max_iterations_message", MAX_ITERATIONS_MESSAGE)
            .expect("max iterations template compiles");
        // Registered so identity.md can `{% include 'untrusted_content' %}`.
        e.add_template("untrusted_content", UNTRUSTED_CONTENT)
            .expect("untrusted content snippet compiles");
        e
    })
}

pub fn render_identity(
    workspace: &str,
    runtime: &str,
    platform_policy: &str,
    channel: Option<&str>,
) -> Result<String, minijinja::Error> {
    let tmpl = env().get_template("identity")?;
    tmpl.render(Value::from_serialize(serde_json::json!({
        "workspace_path": workspace,
        "runtime": runtime,
        "platform_policy": platform_policy,
        // Default to the empty string so the template's
        // ``{% if channel %}`` checks are stable when no channel was
        // provided.
        "channel": channel.unwrap_or_default(),
    })))
}

pub fn render_platform_policy(system: &str) -> Result<String, minijinja::Error> {
    let tmpl = env().get_template("platform_policy")?;
    tmpl.render(Value::from_serialize(serde_json::json!({
        "system": system,
    })))
}

pub fn render_skills_section(skills_summary: &str) -> Result<String, minijinja::Error> {
    let tmpl = env().get_template("skills_section")?;
    tmpl.render(Value::from_serialize(serde_json::json!({
        "skills_summary": skills_summary,
    })))
}

pub fn render_max_iterations_message(tools_used: &[String]) -> Result<String, minijinja::Error> {
    let tmpl = env().get_template("max_iterations_message")?;
    tmpl.render(Value::from_serialize(serde_json::json!({
        "tools_used": tools_used,
    })))
}
