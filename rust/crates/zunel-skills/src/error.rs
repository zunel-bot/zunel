#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("skill io: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },
    #[error("skill frontmatter: {message}")]
    Frontmatter { message: String },
    #[error("skill not found: {name}")]
    MissingSkillFile { name: String },
}

pub type Result<T> = std::result::Result<T, Error>;
