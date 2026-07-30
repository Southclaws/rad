use std::error::Error as StdError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorClass {
    Invalid,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reason {
    Invalid,
    UnknownTable,
    UnknownColumn,
    UnknownScope,
    UnknownBinding,
    DuplicateScope,
    TypeMismatch,
    ScalarArity,
    NondeterministicOrder,
    DependentJoin,
    ProjectionCollision,
    BindingCycle,
    BindingCollision,
    Catalog,
}

impl Reason {
    pub fn class(self) -> ErrorClass {
        match self {
            Self::Catalog => ErrorClass::Internal,
            _ => ErrorClass::Invalid,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    reason: Reason,
    message: String,
    #[source]
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl Error {
    pub fn invalid(reason: Reason, message: impl Into<String>) -> Self {
        debug_assert_ne!(reason, Reason::Catalog);
        Self {
            reason,
            message: message.into(),
            source: None,
        }
    }

    pub fn reason(&self) -> Reason {
        self.reason
    }

    pub fn class(&self) -> ErrorClass {
        self.reason.class()
    }

    pub(crate) fn context(mut self, context: impl AsRef<str>) -> Self {
        self.message = format!("{}: {}", context.as_ref(), self.message);
        self
    }
}

impl From<crate::engine::catalog::Error> for Error {
    fn from(error: crate::engine::catalog::Error) -> Self {
        Self {
            reason: Reason::Catalog,
            message: format!("planner catalog: {error}"),
            source: Some(Box::new(error)),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
