use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config file not found: {0}")]
    NotFound(PathBuf),

    #[error("failed to read config at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse config at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("no provider configured for model {0}")]
    MissingProvider(String),

    #[error("provider {0} missing apiKey")]
    MissingApiKey(String),

    #[error("Invalid instance name {0:?}: must not contain whitespace, path separators, or '..'.")]
    InvalidInstanceName(String),

    /// Detected the pre-rename `~/.zunel/profiles/` directory. zunel
    /// renamed the side-by-side data home concept from "profile" to
    /// "instance"; the directory layout moved with it. We refuse to
    /// proceed rather than silently fall back to the new path,
    /// because that would leave the user wondering where their data
    /// went. The error message tells them exactly which `mv` to run.
    #[error(
        "found legacy ~/.zunel/profiles/ directory at {profiles_path}; zunel now uses {instances_path}.\n\
         please run:\n    \
             mv {profiles_path} {instances_path}\n    \
             mv {active_profile_path} {active_instance_path}   # if it exists",
        profiles_path = profiles_path.display(),
        instances_path = instances_path.display(),
        active_profile_path = active_profile_path.display(),
        active_instance_path = active_instance_path.display(),
    )]
    LegacyProfilesDirectory {
        profiles_path: PathBuf,
        instances_path: PathBuf,
        active_profile_path: PathBuf,
        active_instance_path: PathBuf,
    },

    /// The configured workspace is one of a small set of paths that
    /// would let zunel's filesystem tools mutate state outside the
    /// "zunel sandbox" — `/`, `$HOME`, an ancestor of `$ZUNEL_HOME`,
    /// or `$ZUNEL_HOME` itself. Operators who genuinely need this
    /// (e.g. a one-off script targeting `/tmp` that happens to be a
    /// home subdir) can opt out with `ZUNEL_ALLOW_UNSAFE_WORKSPACE=1`
    /// or the `zunel --i-know-what-im-doing` global flag.
    #[error(
        "refusing to start with workspace {path:?}: {reason}. \
         Set ZUNEL_ALLOW_UNSAFE_WORKSPACE=1 or pass `zunel \
         --i-know-what-im-doing` if you really mean it."
    )]
    UnsafeWorkspace { path: PathBuf, reason: String },
}

pub type Result<T> = std::result::Result<T, Error>;
