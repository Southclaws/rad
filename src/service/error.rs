//! Structured product errors independent of HTTP or any other transport.

use serde::Serialize;

use crate::engine::exec::{Error, ErrorReason};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Schema,
    Preflight,
    Binding,
    Planning,
    Execution,
    Storage,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Location {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

impl Location {
    pub fn is_empty(&self) -> bool {
        self.pointer.is_none()
            && self.node.is_none()
            && self.binding.is_none()
            && self.scope.is_none()
            && self.role.is_none()
    }
}

macro_rules! reasons {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }
        }
    };
}

reasons!(InvalidReason {
    Invalid => "invalid",
    SchemaViolation => "schema_violation",
    UnknownTable => "unknown_table",
    UnknownColumn => "unknown_column",
    UnknownScope => "unknown_scope",
    UnknownBinding => "unknown_binding",
    UnknownNode => "unknown_node",
    NodeCycle => "node_cycle",
    SharedNode => "shared_node",
    UnreachableNode => "unreachable_node",
    DuplicateScope => "duplicate_scope",
    TypeMismatch => "type_mismatch",
    ScalarArity => "scalar_arity",
    NondeterministicOrder => "nondeterministic_order",
    DependentJoin => "dependent_join",
    ProjectionCollision => "projection_collision",
    BindingCycle => "binding_cycle",
    BindingOutputCollision => "binding_output_collision",
    ConstraintViolation => "constraint_violation",
    MutationTargetAmbiguous => "mutation_target_ambiguous",
    MutationShape => "mutation_shape",
    SchemaDataLossAcceptanceRequired => "schema_data_loss_acceptance_required",
    SchemaClientOutdated => "schema_client_outdated",
    SchemaServerOutdated => "schema_server_outdated",
    SchemaHistoryDiverged => "schema_history_diverged",
});

reasons!(ExecutionFailureReason {
    ExecutionFailed => "execution_failed",
    DivisionByZero => "division_by_zero",
    CardinalityViolation => "cardinality_violation",
    MutationTargetNotFound => "mutation_target_not_found",
    RecursionLimit => "recursion_limit",
    NumericOverflow => "numeric_overflow",
});

reasons!(ConflictReason {
    SerializableConflict => "serializable_conflict",
    SchemaTransitionBackpressure => "schema_transition_backpressure",
    SchemaTransitionFinalizing => "schema_transition_finalizing",
});

reasons!(NotFoundReason {
    NotFound => "not_found",
    Transaction => "transaction_not_found",
    SchemaTransition => "schema_transition_not_found",
});

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ExecutionContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crossing: Option<String>,
}

impl ExecutionContext {
    pub fn is_empty(&self) -> bool {
        self.operator.is_none()
            && self.operator_id.is_none()
            && self.table.is_none()
            && self.index.is_none()
            && self.binding.is_none()
            && self.crossing.is_none()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictObject {
    Table,
    Index,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictOperation {
    Read,
    Insert,
    Update,
    Delete,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ConflictContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<ConflictObject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<ConflictOperation>,
}

impl ConflictContext {
    pub fn is_empty(&self) -> bool {
        self.object.is_none()
            && self.table.is_none()
            && self.index.is_none()
            && self.operation.is_none()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResourceContext {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidFailure {
    pub stage: Stage,
    pub reason: InvalidReason,
    pub detail: String,
    pub location: Option<Location>,
    pub diagnostics: Vec<InvalidDiagnostic>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InvalidDiagnostic {
    pub reason: InvalidReason,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionFailure {
    pub stage: Stage,
    pub reason: ExecutionFailureReason,
    pub detail: String,
    pub location: Option<Location>,
    pub execution: Option<ExecutionContext>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictFailure {
    pub stage: Stage,
    pub reason: ConflictReason,
    pub detail: String,
    pub conflict: Option<ConflictContext>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotFoundFailure {
    pub stage: Stage,
    pub reason: NotFoundReason,
    pub detail: String,
    pub resource: Option<ResourceContext>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InternalFailure {
    pub stage: Stage,
    diagnostic: String,
}

impl InternalFailure {
    pub fn diagnostic(&self) -> &str {
        &self.diagnostic
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Failure {
    Invalid(InvalidFailure),
    ExecutionFailed(ExecutionFailure),
    Conflict(ConflictFailure),
    NotFound(NotFoundFailure),
    Internal(InternalFailure),
}

impl Failure {
    pub fn from_exec(error: &Error) -> Self {
        let stage = stage(error);
        let detail = error.to_string();
        match error.reason() {
            ErrorReason::Invalid => invalid(stage, InvalidReason::Invalid, detail),
            ErrorReason::SchemaViolation => {
                invalid(Stage::Schema, InvalidReason::SchemaViolation, detail)
            }
            ErrorReason::UnknownTable => invalid(stage, InvalidReason::UnknownTable, detail),
            ErrorReason::UnknownColumn => invalid(stage, InvalidReason::UnknownColumn, detail),
            ErrorReason::UnknownScope => invalid(stage, InvalidReason::UnknownScope, detail),
            ErrorReason::UnknownBinding => invalid(stage, InvalidReason::UnknownBinding, detail),
            ErrorReason::UnknownNode => invalid(stage, InvalidReason::UnknownNode, detail),
            ErrorReason::NodeCycle => invalid(stage, InvalidReason::NodeCycle, detail),
            ErrorReason::SharedNode => invalid(stage, InvalidReason::SharedNode, detail),
            ErrorReason::UnreachableNode => invalid(stage, InvalidReason::UnreachableNode, detail),
            ErrorReason::DuplicateScope => invalid(stage, InvalidReason::DuplicateScope, detail),
            ErrorReason::TypeMismatch => invalid(stage, InvalidReason::TypeMismatch, detail),
            ErrorReason::ScalarArity => invalid(stage, InvalidReason::ScalarArity, detail),
            ErrorReason::NondeterministicOrder => {
                invalid(stage, InvalidReason::NondeterministicOrder, detail)
            }
            ErrorReason::DependentJoin => invalid(stage, InvalidReason::DependentJoin, detail),
            ErrorReason::ProjectionCollision => {
                invalid(stage, InvalidReason::ProjectionCollision, detail)
            }
            ErrorReason::BindingCycle => invalid(stage, InvalidReason::BindingCycle, detail),
            ErrorReason::BindingOutputCollision => {
                invalid(stage, InvalidReason::BindingOutputCollision, detail)
            }
            ErrorReason::ConstraintViolation => {
                invalid(stage, InvalidReason::ConstraintViolation, detail)
            }
            ErrorReason::MutationTargetAmbiguous => {
                invalid(stage, InvalidReason::MutationTargetAmbiguous, detail)
            }
            ErrorReason::MutationShape => invalid(stage, InvalidReason::MutationShape, detail),
            ErrorReason::SchemaDataLossAcceptanceRequired => invalid(
                stage,
                InvalidReason::SchemaDataLossAcceptanceRequired,
                detail,
            ),
            ErrorReason::SchemaTransitionNotFound => {
                Self::not_found(stage, NotFoundReason::SchemaTransition, detail, None)
            }
            ErrorReason::ExecutionFailed => {
                execution(stage, ExecutionFailureReason::ExecutionFailed, detail)
            }
            ErrorReason::DivisionByZero => {
                execution(stage, ExecutionFailureReason::DivisionByZero, detail)
            }
            ErrorReason::CardinalityViolation => {
                execution(stage, ExecutionFailureReason::CardinalityViolation, detail)
            }
            ErrorReason::MutationTargetNotFound => execution(
                stage,
                ExecutionFailureReason::MutationTargetNotFound,
                detail,
            ),
            ErrorReason::RecursionLimit => {
                execution(stage, ExecutionFailureReason::RecursionLimit, detail)
            }
            ErrorReason::NumericOverflow => {
                execution(stage, ExecutionFailureReason::NumericOverflow, detail)
            }
            ErrorReason::SerializableConflict => {
                conflict(stage, ConflictReason::SerializableConflict, detail)
            }
            ErrorReason::SchemaTransitionBackpressure => {
                conflict(stage, ConflictReason::SchemaTransitionBackpressure, detail)
            }
            ErrorReason::SchemaTransitionFinalizing => {
                conflict(stage, ConflictReason::SchemaTransitionFinalizing, detail)
            }
            ErrorReason::CommitOutcomeUnknown
            | ErrorReason::CatalogCorrupt
            | ErrorReason::CatalogSchemaDrift
            | ErrorReason::StorageUnavailable
            | ErrorReason::Internal => Self::Internal(InternalFailure {
                stage,
                diagnostic: detail,
            }),
        }
    }

    pub fn not_found(
        stage: Stage,
        reason: NotFoundReason,
        detail: impl Into<String>,
        resource: Option<ResourceContext>,
    ) -> Self {
        Self::NotFound(NotFoundFailure {
            stage,
            reason,
            detail: detail.into(),
            resource,
        })
    }
}

impl From<&Error> for Failure {
    fn from(error: &Error) -> Self {
        Self::from_exec(error)
    }
}

fn invalid(stage: Stage, reason: InvalidReason, detail: String) -> Failure {
    Failure::Invalid(InvalidFailure {
        stage,
        reason,
        detail,
        location: None,
        diagnostics: Vec::new(),
    })
}

fn execution(stage: Stage, reason: ExecutionFailureReason, detail: String) -> Failure {
    Failure::ExecutionFailed(ExecutionFailure {
        stage,
        reason,
        detail,
        location: None,
        execution: None,
    })
}

fn conflict(stage: Stage, reason: ConflictReason, detail: String) -> Failure {
    Failure::Conflict(ConflictFailure {
        stage,
        reason,
        detail,
        conflict: None,
    })
}

fn stage(error: &Error) -> Stage {
    use crate::engine::exec::ErrorKind;
    match error.kind() {
        ErrorKind::InvalidInput | ErrorKind::DataLossAcceptance => {
            if matches!(
                error.reason(),
                ErrorReason::UnknownTable
                    | ErrorReason::UnknownColumn
                    | ErrorReason::UnknownScope
                    | ErrorReason::UnknownBinding
                    | ErrorReason::DuplicateScope
                    | ErrorReason::TypeMismatch
                    | ErrorReason::ScalarArity
                    | ErrorReason::NondeterministicOrder
                    | ErrorReason::DependentJoin
                    | ErrorReason::ProjectionCollision
                    | ErrorReason::BindingCycle
                    | ErrorReason::BindingOutputCollision
            ) {
                Stage::Binding
            } else {
                Stage::Preflight
            }
        }
        ErrorKind::Storage | ErrorKind::CorruptData | ErrorKind::CommitOutcomeUnknown => {
            Stage::Storage
        }
        ErrorKind::ConstraintViolation
        | ErrorKind::MutationNotFound
        | ErrorKind::MutationAmbiguous
        | ErrorKind::TransitionFinalizing
        | ErrorKind::TransitionBackpressure
        | ErrorKind::Runtime
        | ErrorKind::RecursionLimit
        | ErrorKind::Conflict
        | ErrorKind::Internal => Stage::Execution,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{exec, planner};

    #[test]
    fn planner_reason_survives_the_executor_boundary() {
        let planner = planner::Error::invalid(
            planner::Reason::UnknownColumn,
            "binding: unknown column \"missing\"",
        );
        let error = exec::Error::from(planner);

        assert_eq!(error.reason(), exec::ErrorReason::UnknownColumn);
        assert_eq!(
            Failure::from_exec(&error),
            Failure::Invalid(InvalidFailure {
                stage: Stage::Binding,
                reason: InvalidReason::UnknownColumn,
                detail: "binding: unknown column \"missing\"".into(),
                location: None,
                diagnostics: Vec::new(),
            })
        );
    }

    #[test]
    fn retryability_classes_do_not_collapse() {
        let constraint = exec::Error::with_reason(
            exec::ErrorKind::ConstraintViolation,
            exec::ErrorReason::ConstraintViolation,
            "duplicate primary key",
        );
        let conflict = exec::Error::with_reason(
            exec::ErrorKind::Conflict,
            exec::ErrorReason::SerializableConflict,
            "transaction raced",
        );

        assert!(matches!(
            Failure::from_exec(&constraint),
            Failure::Invalid(InvalidFailure {
                reason: InvalidReason::ConstraintViolation,
                ..
            })
        ));
        assert!(matches!(
            Failure::from_exec(&conflict),
            Failure::Conflict(ConflictFailure {
                reason: ConflictReason::SerializableConflict,
                ..
            })
        ));
    }
}
