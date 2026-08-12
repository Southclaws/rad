use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

type Source = Arc<dyn StdError + Send + Sync>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    ReadOnly,
    Conflict,
    CommitOutcomeUnknown,
    Closed,
    Unavailable,
    Invalid,
    Data,
    Internal,
}

/// Why a store reported itself permanently closed. A closed store is unusable
/// either way, but the process reacts differently: losing ownership withdraws
/// traffic and stops, while a panicked background task is a failure the
/// supervisor should restart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Closure {
    Clean,
    /// A newer writer took ownership of the storage location.
    Fenced,
    Panicked,
}

#[derive(Clone, Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    closure: Option<Closure>,
    source: Option<Source>,
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Why the store closed, for a [`ErrorKind::Closed`] error whose backend
    /// reported a cause.
    pub fn closure(&self) -> Option<Closure> {
        self.closure
    }

    pub(crate) fn message(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            closure: None,
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
            closure: None,
            source: Some(Arc::new(source)),
        }
    }

    pub(crate) fn closed(
        closure: Closure,
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind: ErrorKind::Closed,
            message: message.into(),
            closure: Some(closure),
            source: Some(Arc::new(source)),
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
