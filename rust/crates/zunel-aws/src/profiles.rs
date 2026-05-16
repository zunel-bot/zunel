//! Auto-discovery of AWS SSO profiles from `~/.aws/config`.
//!
//! Sibling of [`crate::sso_refresh`]: that module knows how to refresh
//! a single named profile, this one figures out which profiles to
//! refresh in the first place. The split lets the gateway scan once at
//! startup, log the discovered set, and then drive refresh against the
//! existing typed pipeline without further file I/O.
//!
//! ## What counts as an SSO profile
//!
//! AWS CLI v2 supports two SSO configuration shapes inside `~/.aws/config`:
//!
//! 1. **Legacy** — the section sets `sso_start_url`, `sso_region`,
//!    `sso_account_id`, `sso_role_name` directly.
//! 2. **`sso_session` (modern)** — the section sets `sso_session = NAME`
//!    pointing at a `[sso-session NAME]` block elsewhere in the file.
//!
//! Both shapes count: a profile is "SSO-bearing" iff its `[profile X]`
//! (or `[default]`) section contains either `sso_start_url` or
//! `sso_session`. We deliberately do NOT treat `[sso-session X]`
//! sections themselves as profiles — they describe the IdP/session,
//! not a credential source.
//!
//! ## What we ignore
//!
//! - Comments starting with `#` or `;` (whole-line and trailing).
//! - Blank lines.
//! - Section headers other than `[default]` and `[profile X]`. Notably:
//!   - `[sso-session X]` (referenced by profiles, not a profile itself)
//!   - `[services X]` (per-service endpoint overrides)
//!   - `[plugins]` (CLI plugin loader)
//!
//! ## File location
//!
//! Mirrors the AWS CLI: honor `AWS_CONFIG_FILE` when set and non-empty,
//! otherwise default to `~/.aws/config`. A missing file is not an error
//! — the gateway simply discovers zero profiles, the same as a user
//! who hasn't configured SSO yet.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Resolve the AWS config file path the same way the AWS CLI does:
/// `AWS_CONFIG_FILE` env var if set and non-empty, otherwise
/// `$HOME/.aws/config`. Returns `None` only when neither the env var
/// nor `$HOME` is set, which is exotic enough on a developer machine
/// that the gateway treats it as "no SSO discovery available".
pub fn resolve_aws_config_path() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("AWS_CONFIG_FILE") {
        if !raw.is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".aws").join("config"))
}

/// Parse the given AWS config file and return the names of every
/// SSO-bearing profile (legacy `sso_start_url` or modern
/// `sso_session`). Names are sorted and deduplicated so callers get
/// stable log output across reads.
///
/// A missing file returns `Ok(vec![])`, not an error: the typical
/// "user hasn't run `aws configure sso` yet" case shouldn't take down
/// the gateway. Other I/O errors (permission denied, etc.) propagate
/// so the caller can decide whether to log-and-continue.
pub fn discover_sso_profiles(path: &Path) -> io::Result<Vec<String>> {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(parse_sso_profiles(&raw))
}

/// Pure-string variant of [`discover_sso_profiles`] for tests and
/// in-memory callers. Keeping the I/O wrapper thin lets us unit-test
/// every parser quirk without touching disk.
pub fn parse_sso_profiles(contents: &str) -> Vec<String> {
    let mut current: Option<ProfileSection> = None;
    let mut found: Vec<String> = Vec::new();

    for raw_line in contents.lines() {
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            // Commit the previous profile (if any) before switching.
            commit_if_sso(current.take(), &mut found);
            current = parse_section_header(section);
            continue;
        }
        if let Some(section) = current.as_mut() {
            if is_sso_key(line) {
                section.has_sso = true;
            }
        }
    }
    commit_if_sso(current, &mut found);

    found.sort();
    found.dedup();
    found
}

/// In-flight state for one `[…]` block we're currently inside.
/// `name` is `Some` only for sections that name a profile; `has_sso`
/// flips to true the first time we see an `sso_*` key in the block.
#[derive(Debug)]
struct ProfileSection {
    name: String,
    has_sso: bool,
}

fn commit_if_sso(section: Option<ProfileSection>, out: &mut Vec<String>) {
    if let Some(s) = section {
        if s.has_sso {
            out.push(s.name);
        }
    }
}

/// Map a raw section header (the `…` in `[…]`) to a profile section
/// when it names one, or `None` for headers like `sso-session foo`,
/// `services bar`, `plugins`. The default profile uses the bare
/// `[default]` form (NOT `[profile default]`) so we accept both for
/// safety; the AWS CLI also tolerates the redundant form.
fn parse_section_header(raw: &str) -> Option<ProfileSection> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("default") {
        return Some(ProfileSection {
            name: "default".to_string(),
            has_sso: false,
        });
    }
    if let Some(rest) = trimmed.strip_prefix("profile") {
        let name = rest.trim();
        if !name.is_empty() {
            return Some(ProfileSection {
                name: name.to_string(),
                has_sso: false,
            });
        }
    }
    // [sso-session foo], [services bar], [plugins], anything else: skip.
    None
}

/// True for keys that mark the enclosing section as "this profile
/// authenticates via SSO". We deliberately accept both shapes:
/// `sso_session` (modern, points at a `[sso-session NAME]` block) and
/// `sso_start_url` (legacy, all SSO config inlined into the profile).
fn is_sso_key(line: &str) -> bool {
    let key = line.split('=').next().unwrap_or("").trim();
    key.eq_ignore_ascii_case("sso_session") || key.eq_ignore_ascii_case("sso_start_url")
}

/// Strip a trailing `#`/`;` comment from a line. AWS CLI's INI parser
/// treats both as comment markers anywhere on the line, so we do the
/// same. We don't try to handle quoted/escaped `#` because the AWS
/// config file format never quotes values.
fn strip_comment(line: &str) -> &str {
    let comment_at =
        line.char_indices()
            .find_map(|(i, c)| if c == '#' || c == ';' { Some(i) } else { None });
    match comment_at {
        Some(i) => &line[..i],
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_legacy_sso_start_url_profile() {
        let cfg = "\
[profile dev]
sso_start_url = https://example.awsapps.com/start
sso_region = us-west-2
sso_account_id = 111111111111
sso_role_name = Developer
";
        assert_eq!(parse_sso_profiles(cfg), vec!["dev".to_string()]);
    }

    #[test]
    fn parses_modern_sso_session_profile() {
        let cfg = "\
[sso-session corp]
sso_start_url = https://example.awsapps.com/start
sso_region = us-west-2

[profile prod]
sso_session = corp
sso_account_id = 222222222222
sso_role_name = ReadOnly
";
        assert_eq!(parse_sso_profiles(cfg), vec!["prod".to_string()]);
    }

    #[test]
    fn ignores_sso_session_blocks_themselves() {
        let cfg = "\
[sso-session corp]
sso_start_url = https://example.awsapps.com/start
sso_region = us-west-2
";
        assert!(parse_sso_profiles(cfg).is_empty());
    }

    #[test]
    fn picks_up_default_profile_when_sso() {
        let cfg = "\
[default]
sso_start_url = https://example.awsapps.com/start
sso_region = us-west-2
sso_account_id = 333333333333
sso_role_name = Developer
";
        assert_eq!(parse_sso_profiles(cfg), vec!["default".to_string()]);
    }

    #[test]
    fn skips_default_profile_without_sso() {
        let cfg = "\
[default]
region = us-east-1
output = json
";
        assert!(parse_sso_profiles(cfg).is_empty());
    }

    #[test]
    fn skips_iam_keypair_profiles() {
        let cfg = "\
[profile iam-only]
region = us-east-1
output = json
";
        assert!(parse_sso_profiles(cfg).is_empty());
    }

    #[test]
    fn handles_multiple_profiles_sorted_and_deduped() {
        let cfg = "\
[profile zeta]
sso_start_url = https://example.awsapps.com/start

[profile alpha]
sso_session = corp

[sso-session corp]
sso_start_url = https://example.awsapps.com/start

[profile beta]
sso_start_url = https://example.awsapps.com/start
";
        assert_eq!(
            parse_sso_profiles(cfg),
            vec!["alpha".to_string(), "beta".to_string(), "zeta".to_string()]
        );
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let cfg = "\
# top-level comment
; semicolon comment

[profile dev] # trailing comment on header
sso_start_url = https://example.awsapps.com/start ; trailing on value
sso_region = us-west-2

# blank section comment
[profile staging]
region = us-east-1
";
        assert_eq!(parse_sso_profiles(cfg), vec!["dev".to_string()]);
    }

    #[test]
    fn ignores_services_and_plugins_blocks() {
        let cfg = "\
[plugins]
cli_legacy_plugin_path = /path

[services my-overrides]
s3 =
  endpoint_url = http://localhost:4566

[profile dev]
sso_start_url = https://example.awsapps.com/start
";
        assert_eq!(parse_sso_profiles(cfg), vec!["dev".to_string()]);
    }

    #[test]
    fn profile_with_no_sso_keys_is_skipped_even_in_mixed_file() {
        let cfg = "\
[profile sso-one]
sso_start_url = https://example.awsapps.com/start

[profile keypair-only]
aws_access_key_id = AKIA000000000000
aws_secret_access_key = abc
";
        assert_eq!(parse_sso_profiles(cfg), vec!["sso-one".to_string()]);
    }

    #[test]
    fn missing_file_returns_empty() {
        let path = Path::new("/tmp/zunel-aws-this-file-should-not-exist-xyz");
        let result = discover_sso_profiles(path).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn discover_reads_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            "\
[profile a]
sso_start_url = https://example.awsapps.com/start

[profile b]
region = us-east-1

[profile c]
sso_session = corp
",
        )
        .unwrap();
        assert_eq!(
            discover_sso_profiles(&path).unwrap(),
            vec!["a".to_string(), "c".to_string()]
        );
    }
}
