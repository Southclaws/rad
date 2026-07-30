use std::error::Error as StdError;

/// Stable catalog error classes used by higher engine layers and transports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    InvalidInput,
    NotFound,
    AlreadyExists,
    CatalogDrift,
    CatalogCorrupt,
    Conflict,
    TransitionBackpressure,
    CommitOutcomeUnknown,
    Storage,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    kind: ErrorKind,
    message: String,
    #[source]
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub(crate) fn message(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    pub(crate) fn source(
        kind: ErrorKind,
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<crate::engine::kv::Error> for Error {
    fn from(error: crate::engine::kv::Error) -> Self {
        let kind = match error.kind() {
            crate::engine::kv::ErrorKind::Conflict => ErrorKind::Conflict,
            crate::engine::kv::ErrorKind::CommitOutcomeUnknown => ErrorKind::CommitOutcomeUnknown,
            _ => ErrorKind::Storage,
        };
        Self::source(kind, format!("catalog storage: {error}"), error)
    }
}
