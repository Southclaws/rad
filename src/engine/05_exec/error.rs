use std::error::Error as StdError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    InvalidInput,
    ConstraintViolation,
    DataLossAcceptance,
    MutationNotFound,
    MutationAmbiguous,
    TransitionFinalizing,
    TransitionBackpressure,
    CommitOutcomeUnknown,
    Runtime,
    RecursionLimit,
    Conflict,
    Storage,
    CorruptData,
    Internal,
}

/// Stable semantic identity retained across the executor boundary.
///
/// Kinds remain useful for local control flow. Reasons are the product-facing
/// taxonomy consumed by transport-neutral error mapping and must never be
/// reconstructed from message text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorReason {
    Invalid,
    SchemaViolation,
    UnknownTable,
    UnknownColumn,
    UnknownScope,
    UnknownBinding,
    UnknownNode,
    NodeCycle,
    SharedNode,
    UnreachableNode,
    DuplicateScope,
    TypeMismatch,
    ScalarArity,
    NondeterministicOrder,
    DependentJoin,
    ProjectionCollision,
    BindingCycle,
    BindingOutputCollision,
    ConstraintViolation,
    MutationTargetAmbiguous,
    MutationShape,
    SchemaDataLossAcceptanceRequired,
    SchemaTransitionNotFound,
    ExecutionFailed,
    DivisionByZero,
    CardinalityViolation,
    MutationTargetNotFound,
    RecursionLimit,
    NumericOverflow,
    SerializableConflict,
    SchemaTransitionBackpressure,
    SchemaTransitionFinalizing,
    CommitOutcomeUnknown,
    CatalogCorrupt,
    CatalogSchemaDrift,
    StorageUnavailable,
    Internal,
}

impl ErrorReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::SchemaViolation => "schema_violation",
            Self::UnknownTable => "unknown_table",
            Self::UnknownColumn => "unknown_column",
            Self::UnknownScope => "unknown_scope",
            Self::UnknownBinding => "unknown_binding",
            Self::UnknownNode => "unknown_node",
            Self::NodeCycle => "node_cycle",
            Self::SharedNode => "shared_node",
            Self::UnreachableNode => "unreachable_node",
            Self::DuplicateScope => "duplicate_scope",
            Self::TypeMismatch => "type_mismatch",
            Self::ScalarArity => "scalar_arity",
            Self::NondeterministicOrder => "nondeterministic_order",
            Self::DependentJoin => "dependent_join",
            Self::ProjectionCollision => "projection_collision",
            Self::BindingCycle => "binding_cycle",
            Self::BindingOutputCollision => "binding_output_collision",
            Self::ConstraintViolation => "constraint_violation",
            Self::MutationTargetAmbiguous => "mutation_target_ambiguous",
            Self::MutationShape => "mutation_shape",
            Self::SchemaDataLossAcceptanceRequired => "schema_data_loss_acceptance_required",
            Self::SchemaTransitionNotFound => "schema_transition_not_found",
            Self::ExecutionFailed => "execution_failed",
            Self::DivisionByZero => "division_by_zero",
            Self::CardinalityViolation => "cardinality_violation",
            Self::MutationTargetNotFound => "mutation_target_not_found",
            Self::RecursionLimit => "recursion_limit",
            Self::NumericOverflow => "numeric_overflow",
            Self::SerializableConflict => "serializable_conflict",
            Self::SchemaTransitionBackpressure => "schema_transition_backpressure",
            Self::SchemaTransitionFinalizing => "schema_transition_finalizing",
            Self::CommitOutcomeUnknown => "commit_outcome_unknown",
            Self::CatalogCorrupt => "catalog_corrupt",
            Self::CatalogSchemaDrift => "catalog_schema_drift",
            Self::StorageUnavailable => "storage_unavailable",
            Self::Internal => "internal",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    kind: ErrorKind,
    reason: ErrorReason,
    message: String,
    #[source]
    source: Option<Box<dyn StdError + Send + Sync>>,
}

impl Error {
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn reason(&self) -> ErrorReason {
        self.reason
    }

    /// Whether retrying the complete transaction may succeed unchanged.
    ///
    /// Transition backpressure and finalization gates are deliberately part
    /// of the conflict class even though their local kinds preserve the more
    /// useful scheduling diagnosis.
    pub fn is_conflict(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Conflict
                | ErrorKind::TransitionBackpressure
                | ErrorKind::TransitionFinalizing
        )
    }

    pub(crate) fn message(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            reason: default_reason(kind),
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
            reason: default_reason(kind),
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    pub(crate) fn with_reason(
        kind: ErrorKind,
        reason: ErrorReason,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            reason,
            message: message.into(),
            source: None,
        }
    }

    pub(crate) fn source_with_reason(
        kind: ErrorKind,
        reason: ErrorReason,
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            reason,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

impl From<crate::engine::kv::Error> for Error {
    fn from(error: crate::engine::kv::Error) -> Self {
        let (kind, reason) = match error.kind() {
            crate::engine::kv::ErrorKind::Conflict => {
                (ErrorKind::Conflict, ErrorReason::SerializableConflict)
            }
            crate::engine::kv::ErrorKind::CommitOutcomeUnknown => (
                ErrorKind::CommitOutcomeUnknown,
                ErrorReason::CommitOutcomeUnknown,
            ),
            crate::engine::kv::ErrorKind::Data => (ErrorKind::CorruptData, ErrorReason::Internal),
            _ => (ErrorKind::Storage, ErrorReason::StorageUnavailable),
        };
        Self::source_with_reason(kind, reason, format!("exec storage: {error}"), error)
    }
}

impl From<crate::engine::catalog::Error> for Error {
    fn from(error: crate::engine::catalog::Error) -> Self {
        let (kind, reason) = match error.kind() {
            crate::engine::catalog::ErrorKind::InvalidInput
            | crate::engine::catalog::ErrorKind::NotFound => {
                (ErrorKind::InvalidInput, ErrorReason::Invalid)
            }
            crate::engine::catalog::ErrorKind::AlreadyExists => (
                ErrorKind::ConstraintViolation,
                ErrorReason::ConstraintViolation,
            ),
            crate::engine::catalog::ErrorKind::Conflict => {
                (ErrorKind::Conflict, ErrorReason::SerializableConflict)
            }
            crate::engine::catalog::ErrorKind::TransitionBackpressure => (
                ErrorKind::TransitionBackpressure,
                ErrorReason::SchemaTransitionBackpressure,
            ),
            crate::engine::catalog::ErrorKind::CommitOutcomeUnknown => (
                ErrorKind::CommitOutcomeUnknown,
                ErrorReason::CommitOutcomeUnknown,
            ),
            crate::engine::catalog::ErrorKind::CatalogCorrupt
            | crate::engine::catalog::ErrorKind::CatalogDrift => (
                ErrorKind::CorruptData,
                if error.kind() == crate::engine::catalog::ErrorKind::CatalogDrift {
                    ErrorReason::CatalogSchemaDrift
                } else {
                    ErrorReason::CatalogCorrupt
                },
            ),
            _ => (ErrorKind::Storage, ErrorReason::StorageUnavailable),
        };
        Self::source_with_reason(kind, reason, format!("exec catalog: {error}"), error)
    }
}

impl From<crate::engine::lir::eval::EvalError> for Error {
    fn from(error: crate::engine::lir::eval::EvalError) -> Self {
        let kind = match error.kind() {
            crate::engine::lir::eval::EvalErrorKind::Runtime => ErrorKind::Runtime,
            crate::engine::lir::eval::EvalErrorKind::Internal => ErrorKind::Internal,
        };
        let reason = match error.reason() {
            crate::engine::lir::eval::EvalErrorReason::Runtime => ErrorReason::ExecutionFailed,
            crate::engine::lir::eval::EvalErrorReason::DivisionByZero => {
                ErrorReason::DivisionByZero
            }
            crate::engine::lir::eval::EvalErrorReason::NumericOverflow => {
                ErrorReason::NumericOverflow
            }
            crate::engine::lir::eval::EvalErrorReason::Internal => ErrorReason::Internal,
        };
        Self::source_with_reason(kind, reason, error.to_string(), error)
    }
}

impl From<crate::engine::planner::Error> for Error {
    fn from(error: crate::engine::planner::Error) -> Self {
        let kind = match error.class() {
            crate::engine::planner::ErrorClass::Invalid => ErrorKind::InvalidInput,
            crate::engine::planner::ErrorClass::Internal => ErrorKind::Internal,
        };
        let reason = planner_reason(error.reason());
        Self::source_with_reason(kind, reason, error.to_string(), error)
    }
}

fn default_reason(kind: ErrorKind) -> ErrorReason {
    match kind {
        ErrorKind::InvalidInput => ErrorReason::Invalid,
        ErrorKind::ConstraintViolation => ErrorReason::ConstraintViolation,
        ErrorKind::DataLossAcceptance => ErrorReason::SchemaDataLossAcceptanceRequired,
        ErrorKind::MutationNotFound => ErrorReason::MutationTargetNotFound,
        ErrorKind::MutationAmbiguous => ErrorReason::MutationTargetAmbiguous,
        ErrorKind::TransitionFinalizing => ErrorReason::SchemaTransitionFinalizing,
        ErrorKind::TransitionBackpressure => ErrorReason::SchemaTransitionBackpressure,
        ErrorKind::CommitOutcomeUnknown => ErrorReason::CommitOutcomeUnknown,
        ErrorKind::Runtime => ErrorReason::ExecutionFailed,
        ErrorKind::RecursionLimit => ErrorReason::RecursionLimit,
        ErrorKind::Conflict => ErrorReason::SerializableConflict,
        ErrorKind::Storage => ErrorReason::StorageUnavailable,
        ErrorKind::CorruptData => ErrorReason::CatalogCorrupt,
        ErrorKind::Internal => ErrorReason::Internal,
    }
}

fn planner_reason(reason: crate::engine::planner::Reason) -> ErrorReason {
    use crate::engine::planner::Reason;
    match reason {
        Reason::Invalid => ErrorReason::Invalid,
        Reason::UnknownTable => ErrorReason::UnknownTable,
        Reason::UnknownColumn => ErrorReason::UnknownColumn,
        Reason::UnknownScope => ErrorReason::UnknownScope,
        Reason::UnknownBinding => ErrorReason::UnknownBinding,
        Reason::DuplicateScope => ErrorReason::DuplicateScope,
        Reason::TypeMismatch => ErrorReason::TypeMismatch,
        Reason::ScalarArity => ErrorReason::ScalarArity,
        Reason::NondeterministicOrder => ErrorReason::NondeterministicOrder,
        Reason::DependentJoin => ErrorReason::DependentJoin,
        Reason::ProjectionCollision => ErrorReason::ProjectionCollision,
        Reason::BindingCycle => ErrorReason::BindingCycle,
        Reason::BindingCollision => ErrorReason::BindingOutputCollision,
        Reason::Catalog => ErrorReason::Internal,
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::{Error, ErrorKind};

    #[test]
    fn retryable_conflict_class_includes_transition_flow_control() {
        for kind in [
            ErrorKind::Conflict,
            ErrorKind::TransitionBackpressure,
            ErrorKind::TransitionFinalizing,
        ] {
            assert!(Error::message(kind, "retry").is_conflict());
        }
        assert!(!Error::message(ErrorKind::InvalidInput, "fix it").is_conflict());
        assert!(!Error::message(ErrorKind::CommitOutcomeUnknown, "reconcile first").is_conflict());
    }
}
