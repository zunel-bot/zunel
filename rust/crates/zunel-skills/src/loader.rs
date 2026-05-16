use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use include_dir::{include_dir, Dir, DirEntry};

use crate::error::Result;
use crate::frontmatter::{split, ParsedMetadata};

/// Skills shipped inside the binary. Resolved at compile time from the
/// crate's `builtins/` directory; the include macro silently produces an
/// empty `Dir` if the directory is missing, which keeps lower-level
/// tooling (cargo-deny, doc builds) happy without a sentinel file.
static BUILTIN_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/builtins");

/// Synthetic path prefix used when a skill is sourced from
/// [`BUILTIN_DIR`]. Visible in `Skill.path.display()` and in the skills
/// summary block so operators can tell at a glance whether a skill is
/// bundled vs. user-authored.
pub const EMBEDDED_BUILTIN_LABEL: &str = "<embedded-builtin>";

/// Summary metadata for a loaded skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub parsed_metadata: ParsedMetadata,
}

/// Reads `<workspace>/skills/<name>/SKILL.md` first, then a filesystem
/// builtin override directory (used by tests and ad-hoc operator
/// overrides), and finally the skills bundled into the binary at compile
/// time. User skills win for name collisions; the embedded source is the
/// lowest-priority fallback.
pub struct SkillsLoader {
    workspace: PathBuf,
    builtin: Option<PathBuf>,
    disabled: Vec<String>,
}

impl SkillsLoader {
    pub fn new(workspace: &Path, builtin: Option<&Path>, disabled: &[String]) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            builtin: builtin.map(Path::to_path_buf),
            disabled: disabled.to_vec(),
        }
    }

    /// List all known skills. If `filter_unavailable` is true, skills
    /// whose `requires` block fails are omitted; otherwise they appear
    /// with `available = false`.
    pub fn list_skills(&self, filter_unavailable: bool) -> Result<Vec<Skill>> {
        let mut by_name: BTreeMap<String, Skill> = BTreeMap::new();
        self.collect_into(&self.workspace, &mut by_name)?;
        if let Some(builtin) = &self.builtin {
            self.collect_into(builtin, &mut by_name)?;
        }
        collect_embedded_into(&BUILTIN_DIR, &mut by_name)?;
        let mut out: Vec<Skill> = by_name
            .into_values()
            .filter(|s| !self.disabled.contains(&s.name))
            .collect();
        if filter_unavailable {
            out.retain(|s| s.available);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Return the full markdown body for a single skill, searching
    /// workspace first, then the filesystem builtin override, then the
    /// embedded builtins. Frontmatter is stripped.
    pub fn load_skill(&self, name: &str) -> Result<Option<String>> {
        for root in self.roots() {
            let path = root.join("skills").join(name).join("SKILL.md");
            if path.exists() {
                let raw = std::fs::read_to_string(&path)?;
                let (_, body) = split(&raw)?;
                return Ok(Some(body));
            }
        }
        if let Some(raw) = embedded_skill_md(&BUILTIN_DIR, name) {
            let (_, body) = split(raw)?;
            return Ok(Some(body));
        }
        Ok(None)
    }

    /// Return a single markdown blob containing the full content of the
    /// named skills, separated by `\n\n---\n\n` and prefixed with a
    /// `### Skill: <name>` header. Missing skills are skipped.
    pub fn load_skills_for_context(&self, names: &[String]) -> Result<String> {
        let mut parts = Vec::new();
        for name in names {
            if let Some(body) = self.load_skill(name)? {
                parts.push(format!("### Skill: {name}\n\n{}", body.trim_end()));
            }
        }
        Ok(parts.join("\n\n---\n\n"))
    }

    /// Build the markdown summary block injected into the system prompt.
    /// Each line is formatted as:
    /// `- **<name>** — <description>  `<path>`` (two trailing spaces).
    /// Skills in `exclude` (typically the always-on set) are omitted.
    pub fn build_skills_summary(&self, exclude: Option<&HashSet<String>>) -> Result<String> {
        let skills = self.list_skills(false)?;
        let mut lines = Vec::with_capacity(skills.len());
        for skill in skills {
            if exclude.map(|e| e.contains(&skill.name)).unwrap_or(false) {
                continue;
            }
            let rel_path = skill.path.display().to_string();
            let availability = if skill.available {
                String::new()
            } else {
                format!(
                    " (unavailable: {})",
                    skill.unavailable_reason.unwrap_or_default()
                )
            };
            lines.push(format!(
                "- **{}** — {}  `{}`{}",
                skill.name, skill.description, rel_path, availability
            ));
        }
        Ok(lines.join("\n"))
    }

    pub fn get_always_skills(&self) -> Result<Vec<String>> {
        Ok(self
            .list_skills(true)?
            .into_iter()
            .filter(|s| s.parsed_metadata.always)
            .map(|s| s.name)
            .collect())
    }

    fn roots(&self) -> Vec<&Path> {
        let mut roots = vec![self.workspace.as_path()];
        if let Some(b) = &self.builtin {
            roots.push(b.as_path());
        }
        roots
    }

    fn collect_into(&self, root: &Path, by_name: &mut BTreeMap<String, Skill>) -> Result<()> {
        let skills_dir = root.join("skills");
        if !skills_dir.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(&skills_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if by_name.contains_key(&name) {
                continue;
            }
            let skill_md = entry.path().join("SKILL.md");
            if !skill_md.exists() {
                continue;
            }
            let raw = std::fs::read_to_string(&skill_md)?;
            let (fm, _body) = split(&raw)?;
            let meta = fm.parsed_metadata();
            let (available, unavailable_reason) = check_requirements(&meta);
            by_name.insert(
                name.clone(),
                Skill {
                    name,
                    description: fm.description,
                    path: skill_md,
                    available,
                    unavailable_reason,
                    parsed_metadata: meta,
                },
            );
        }
        Ok(())
    }
}

fn collect_embedded_into(dir: &Dir<'_>, by_name: &mut BTreeMap<String, Skill>) -> Result<()> {
    for entry in dir.entries() {
        let DirEntry::Dir(sub) = entry else {
            continue;
        };
        let Some(name) = sub.path().file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if by_name.contains_key(name) {
            continue;
        }
        let Some(skill_md) = sub.get_file(sub.path().join("SKILL.md")) else {
            continue;
        };
        let Some(raw) = skill_md.contents_utf8() else {
            continue;
        };
        let (fm, _body) = split(raw)?;
        let meta = fm.parsed_metadata();
        let (available, unavailable_reason) = check_requirements(&meta);
        let display_path = PathBuf::from(format!("{EMBEDDED_BUILTIN_LABEL}/{name}/SKILL.md"));
        by_name.insert(
            name.to_string(),
            Skill {
                name: name.to_string(),
                description: fm.description,
                path: display_path,
                available,
                unavailable_reason,
                parsed_metadata: meta,
            },
        );
    }
    Ok(())
}

fn embedded_skill_md<'a>(dir: &'a Dir<'_>, name: &str) -> Option<&'a str> {
    let path = PathBuf::from(name).join("SKILL.md");
    dir.get_file(path)?.contents_utf8()
}

fn check_requirements(meta: &ParsedMetadata) -> (bool, Option<String>) {
    for bin in &meta.bins {
        if which::which(bin).is_err() {
            return (false, Some(format!("missing bin: {bin}")));
        }
    }
    for var in &meta.env {
        if std::env::var(var).is_err() {
            return (false, Some(format!("missing env: {var}")));
        }
    }
    (true, None)
}
