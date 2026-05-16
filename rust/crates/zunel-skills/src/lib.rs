//! Skill loader for zunel. Discovers user skills under
//! `<workspace>/skills/` and (optionally) packaged builtins, parses YAML
//! frontmatter, and produces the summary + always-on list the
//! `ContextBuilder` injects into the system prompt.

mod error;
mod frontmatter;
mod loader;

pub use error::{Error, Result};
pub use frontmatter::{Frontmatter, MetadataRaw, ParsedMetadata};
pub use loader::{Skill, SkillsLoader, EMBEDDED_BUILTIN_LABEL};
