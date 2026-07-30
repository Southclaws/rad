use std::error::Error as StdError;
use std::fmt;

use serde::{Deserialize, Serialize};

type Source = Box<dyn StdError + Send + Sync>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Conflict,
    CommitOutcomeUnknown,
    Closed,
    Unavailable,
    Invalid,
    Data,
    Internal,
}

#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    source: Option<Source>,
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

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn StdError + 'static))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
