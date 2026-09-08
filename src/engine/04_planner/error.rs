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
    catalog_kind: Option<crate::engine::catalog::ErrorKind>,
    message: String,
    #[source]
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl Error {
    pub fn invalid(reason: Reason, message: impl Into<String>) -> Self {
        debug_assert_ne!(reason, Reason::Catalog);
        Self {
            reason,
            catalog_kind: None,
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

    pub fn catalog_kind(&self) -> Option<crate::engine::catalog::ErrorKind> {
        self.catalog_kind
    }

    pub(crate) fn context(mut self, context: impl AsRef<str>) -> Self {
        self.message = format!("{}: {}", context.as_ref(), self.message);
        self
    }
}

impl From<crate::engine::catalog::Error> for Error {
    fn from(error: crate::engine::catalog::Error) -> Self {
        let catalog_kind = error.kind();
        Self {
            reason: Reason::Catalog,
            catalog_kind: Some(catalog_kind),
            message: format!("planner catalog: {error}"),
            source: Some(Box::new(error)),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
