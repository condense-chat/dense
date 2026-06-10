//! The library error type. The CLI binary renders these via color-eyre.

pub type Result<T> = std::result::Result<T, Error>;

/// Attach human context to any error, mapping it into [`Error::Message`] —
/// the lib's lightweight stand-in for color-eyre's `wrap_err`. The original
/// error rides along as the `source`, so the rendered report keeps the full
/// cause chain.
pub trait Context<T> {
    fn ctx(self, msg: impl std::fmt::Display) -> Result<T>;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Auth(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{msg}")]
    Message {
        msg: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
    #[error("{0}")]
    Profile(String),
    #[error(transparent)]
    TomlDe(#[from] toml::de::Error),
    #[error(transparent)]
    TomlSer(#[from] toml::ser::Error),
    #[error("{0}")]
    Tool(String),
}

impl Error {
    pub fn msg(s: impl Into<String>) -> Self {
        Self::Message {
            msg: s.into(),
            source: None,
        }
    }
}

impl<T, E> Context<T> for std::result::Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn ctx(self, msg: impl std::fmt::Display) -> Result<T> {
        self.map_err(|e| Error::Message {
            msg: msg.to_string(),
            source: Some(Box::new(e)),
        })
    }
}
