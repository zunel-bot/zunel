#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("template error: {source}")]
    Template {
        #[from]
        source: minijinja::Error,
    },
    #[error("context io: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },
    #[error("skills error: {source}")]
    Skills {
        #[from]
        source: zunel_skills::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
