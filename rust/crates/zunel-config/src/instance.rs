use std::path::PathBuf;

use crate::error::{Error, Result};

pub const DEFAULT_INSTANCE_NAME: &str = "default";
const DEFAULT_HOME_NAME: &str = ".zunel";
const INSTANCES_DIR_NAME: &str = "instances";
const ACTIVE_INSTANCE_FILE: &str = "active_instance";
const LEGACY_PROFILES_DIR_NAME: &str = "profiles";
const LEGACY_ACTIVE_PROFILE_FILE: &str = "active_profile";

pub fn default_zunel_root() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| Error::Io {
        path: PathBuf::from("~"),
        source: std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not resolve home directory",
        ),
    })?;
    Ok(home.join(DEFAULT_HOME_NAME))
}

pub fn resolve_instance_home(name: &str) -> Result<PathBuf> {
    if name.is_empty() || name == DEFAULT_INSTANCE_NAME {
        return default_zunel_root();
    }
    validate_instance_name(name)?;
    let root = default_zunel_root()?;
    check_legacy_profiles_dir(&root)?;
    Ok(root.join(INSTANCES_DIR_NAME).join(name))
}

pub fn active_instance_path() -> Result<PathBuf> {
    Ok(default_zunel_root()?.join(ACTIVE_INSTANCE_FILE))
}

pub fn read_sticky_instance() -> Option<String> {
    let path = active_instance_path().ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let name = raw.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

pub fn set_sticky_instance(name: &str) -> Result<()> {
    let path = active_instance_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    if name.is_empty() || name == DEFAULT_INSTANCE_NAME {
        if let Err(source) = std::fs::remove_file(&path) {
            if source.kind() != std::io::ErrorKind::NotFound {
                return Err(Error::Io { path, source });
            }
        }
        return Ok(());
    }
    validate_instance_name(name)?;
    std::fs::write(&path, format!("{name}\n")).map_err(|source| Error::Io { path, source })
}

pub fn active_instance_name() -> String {
    if let Some(home) = std::env::var_os("ZUNEL_HOME") {
        let path = PathBuf::from(home);
        if let Ok(default) = default_zunel_root() {
            if path == default {
                return DEFAULT_INSTANCE_NAME.to_string();
            }
        }
        if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            return name.to_string();
        }
    }
    read_sticky_instance().unwrap_or_else(|| DEFAULT_INSTANCE_NAME.to_string())
}

pub fn active_instance_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("ZUNEL_HOME") {
        return Ok(PathBuf::from(home));
    }
    match read_sticky_instance() {
        Some(instance) => resolve_instance_home(&instance),
        None => default_zunel_root(),
    }
}

pub fn list_instances() -> Result<Vec<String>> {
    let root = default_zunel_root()?;
    let mut instances = Vec::new();
    if root.exists() {
        instances.push(DEFAULT_INSTANCE_NAME.to_string());
    }
    check_legacy_profiles_dir(&root)?;
    let instances_dir = root.join(INSTANCES_DIR_NAME);
    if !instances_dir.exists() {
        return Ok(instances);
    }
    for entry in std::fs::read_dir(&instances_dir).map_err(|source| Error::Io {
        path: instances_dir.clone(),
        source,
    })? {
        let entry = entry.map_err(|source| Error::Io {
            path: instances_dir.clone(),
            source,
        })?;
        if !entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if validate_instance_name(&name).is_ok() {
            instances.push(name);
        }
    }
    instances.sort();
    instances.dedup();
    Ok(instances)
}

pub fn validate_instance_name(name: &str) -> Result<()> {
    if name.chars().any(char::is_whitespace)
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
    {
        return Err(Error::InvalidInstanceName(name.to_string()));
    }
    Ok(())
}

/// Refuse to operate while the legacy `~/.zunel/profiles/` directory is
/// present (and the new `~/.zunel/instances/` does not yet exist). This
/// is the one-time migration gate after the `profile` → `instance` rename:
/// an explicit error with a `mv` recipe is friendlier than silently
/// reading from the wrong path or auto-touching user state.
fn check_legacy_profiles_dir(root: &std::path::Path) -> Result<()> {
    let legacy_dir = root.join(LEGACY_PROFILES_DIR_NAME);
    let new_dir = root.join(INSTANCES_DIR_NAME);
    if legacy_dir.is_dir() && !new_dir.exists() {
        return Err(Error::LegacyProfilesDirectory {
            profiles_path: legacy_dir,
            instances_path: new_dir,
            active_profile_path: root.join(LEGACY_ACTIVE_PROFILE_FILE),
            active_instance_path: root.join(ACTIVE_INSTANCE_FILE),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_instance_name_rejects_unsafe_inputs() {
        assert!(validate_instance_name("ok").is_ok());
        assert!(validate_instance_name("dev-1").is_ok());
        assert!(validate_instance_name("a b").is_err());
        assert!(validate_instance_name("../escape").is_err());
        assert!(validate_instance_name("a/b").is_err());
        assert!(validate_instance_name("a\\b").is_err());
    }

    #[test]
    fn check_legacy_profiles_dir_errors_when_only_legacy_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join(LEGACY_PROFILES_DIR_NAME)).expect("create legacy dir");
        let err = check_legacy_profiles_dir(root).expect_err("legacy dir must error");
        match err {
            Error::LegacyProfilesDirectory {
                profiles_path,
                instances_path,
                ..
            } => {
                assert_eq!(profiles_path, root.join(LEGACY_PROFILES_DIR_NAME));
                assert_eq!(instances_path, root.join(INSTANCES_DIR_NAME));
                let rendered = err_render(&Error::LegacyProfilesDirectory {
                    profiles_path,
                    instances_path,
                    active_profile_path: root.join(LEGACY_ACTIVE_PROFILE_FILE),
                    active_instance_path: root.join(ACTIVE_INSTANCE_FILE),
                });
                assert!(
                    rendered.contains("mv "),
                    "error message should suggest a mv command, got: {rendered}"
                );
            }
            other => panic!("expected LegacyProfilesDirectory, got {other:?}"),
        }
    }

    #[test]
    fn check_legacy_profiles_dir_passes_when_new_dir_also_exists() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join(LEGACY_PROFILES_DIR_NAME)).expect("create legacy dir");
        std::fs::create_dir_all(root.join(INSTANCES_DIR_NAME)).expect("create new dir");
        check_legacy_profiles_dir(root).expect("both present should be permitted");
    }

    #[test]
    fn check_legacy_profiles_dir_passes_when_neither_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        check_legacy_profiles_dir(tmp.path()).expect("neither present should be permitted");
    }

    fn err_render(err: &Error) -> String {
        format!("{err}")
    }
}
