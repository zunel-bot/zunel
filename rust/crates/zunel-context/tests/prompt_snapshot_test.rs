use std::path::PathBuf;

use zunel_context::ContextBuilder;
use zunel_skills::SkillsLoader;

/// Byte-compat snapshot: the Rust-rendered system prompt must match a
/// Python-generated fixture for a minimal controlled workspace.
///
/// The fixture (`tests/fixtures/python-system-prompt.txt`) was produced
/// by running the Python `ContextBuilder` against
/// `tests/fixtures/workspace`, with three deterministic substitutions:
///
///   - `platform.system()` → `"Darwin"`
///   - `platform.machine()` → `"arm64"`
///   - `platform.python_version()` → `"3.13.5"`
///
/// and `BUILTIN_SKILLS_DIR` redirected to a non-existent path so only
/// workspace skills are listed (matching how the Rust `SkillsLoader`
/// is wired in this crate). The absolute workspace path is replaced
/// with the literal `<WORKSPACE>` placeholder before saving.
///
/// To regenerate after a deliberate template change, see the
/// instructions in `docs/superpowers/plans/2026-04-24-rust-slice-3-local-tools.md`
/// (Task 19).
#[test]
fn system_prompt_matches_python_fixture() {
    let manifest_dir: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir.join("tests/fixtures/workspace");
    let canonical_workspace = std::fs::canonicalize(&workspace).expect("workspace exists");

    // Disable embedded builtins so the snapshot only reflects the
    // workspace fixture — matching how the Python fixture was generated
    // with `BUILTIN_SKILLS_DIR` redirected to a non-existent path. Keep
    // this list in sync with `crates/zunel-skills/builtins/`.
    let loader = SkillsLoader::new(
        &canonical_workspace,
        None,
        &["mcp-oauth-login".to_string(), "gitlab-mr-write".to_string()],
    );
    let builder = ContextBuilder::new(canonical_workspace.clone(), loader)
        .with_runtime("macOS arm64, Python 3.13.5");
    let raw = builder.build_system_prompt(Some("cli")).unwrap();

    let workspace_str = canonical_workspace.display().to_string();
    let actual = raw.replace(&workspace_str, "<WORKSPACE>");

    let fixture_path = manifest_dir.join("tests/fixtures/python-system-prompt.txt");
    let expected = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", fixture_path.display()));

    if actual != expected {
        let diff_path = manifest_dir.join("target-rust-system-prompt.txt");
        // Best-effort: write the actual prompt next to the fixture so
        // the human running the test can diff manually.
        let _ = std::fs::write(&diff_path, &actual);
        panic!(
            "Rust system prompt does not match python fixture.\n\
             Wrote actual to {}\n\
             --- diff hint ---\n{}",
            diff_path.display(),
            unified_diff_hint(&expected, &actual),
        );
    }
}

/// Tiny side-by-side diff to surface the first mismatching line in the
/// panic message; not a real diff library, just enough breadcrumbs to
/// guide a developer to the right line.
fn unified_diff_hint(expected: &str, actual: &str) -> String {
    let mut out = String::new();
    for (i, (a, b)) in expected.lines().zip(actual.lines()).enumerate() {
        if a != b {
            out.push_str(&format!(
                "line {} differs:\n  expected: {a:?}\n  actual:   {b:?}\n",
                i + 1
            ));
            break;
        }
    }
    if expected.lines().count() != actual.lines().count() {
        out.push_str(&format!(
            "line count differs: expected={} actual={}\n",
            expected.lines().count(),
            actual.lines().count()
        ));
    }
    out
}
