use axum::http::StatusCode;

use super::generated::types as wire;
use crate::service::error::{
    ConflictContext, ConflictFailure, ConflictObject, ConflictOperation, ExecutionContext,
    ExecutionFailure, Failure, InternalFailure, InvalidFailure, Location, NotFoundFailure,
    ResourceContext, Stage,
};

pub(super) struct ResponseProblem {
    pub status: StatusCode,
    pub body: wire::Problem,
}

impl ResponseProblem {
    pub(super) fn from_failure(failure: Failure) -> Self {
        match failure {
            Failure::Invalid(failure) => Self::invalid(failure, StatusCode::UNPROCESSABLE_ENTITY),
            Failure::ExecutionFailed(failure) => Self::execution(failure),
            Failure::Conflict(failure) => Self::conflict(failure),
            Failure::NotFound(failure) => Self::not_found(failure),
            Failure::Internal(failure) => Self::internal(failure),
        }
    }

    pub(super) fn invalid(failure: InvalidFailure, status: StatusCode) -> Self {
        Self {
            status,
            body: wire::Problem::InvalidProblem(wire::InvalidProblem {
                detail: Some(failure.detail),
                errors: (failure.diagnostics.len() >= 2).then(|| {
                    failure
                        .diagnostics
                        .into_iter()
                        .map(|diagnostic| wire::InvalidDiagnostic {
                            detail: diagnostic.detail,
                            location: diagnostic
                                .location
                                .filter(|location| !location.is_empty())
                                .map(location),
                            reason: diagnostic.reason.as_str().into(),
                        })
                        .collect()
                }),
                location: failure
                    .location
                    .filter(|location| !location.is_empty())
                    .map(location),
                reason: failure.reason.as_str().into(),
                stage: stage(failure.stage),
                status: i64::from(status.as_u16()),
                title: wire::InvalidProblemTitle::InvalidRequest,
                r#type: wire::InvalidProblemType::UrnRadProblemInvalid,
            }),
        }
    }

    fn execution(failure: ExecutionFailure) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            body: wire::Problem::ExecutionFailedProblem(wire::ExecutionFailedProblem {
                detail: Some(failure.detail),
                execution: failure
                    .execution
                    .filter(|context| !context.is_empty())
                    .map(execution_context),
                location: failure
                    .location
                    .filter(|location| !location.is_empty())
                    .map(location),
                reason: failure.reason.as_str().into(),
                stage: stage(failure.stage),
                status: 422,
                title: wire::ExecutionFailedProblemTitle::QueryExecutionFailed,
                r#type: wire::ExecutionFailedProblemType::UrnRadProblemExecutionFailed,
            }),
        }
    }

    fn conflict(failure: ConflictFailure) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            body: wire::Problem::ConflictProblem(wire::ConflictProblem {
                conflict: failure
                    .conflict
                    .filter(|context| !context.is_empty())
                    .map(conflict_context),
                detail: Some(failure.detail),
                reason: failure.reason.as_str().into(),
                stage: stage(failure.stage),
                status: 409,
                title: wire::ConflictProblemTitle::TransactionConflict,
                r#type: wire::ConflictProblemType::UrnRadProblemConflict,
            }),
        }
    }

    fn not_found(failure: NotFoundFailure) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: wire::Problem::NotFoundProblem(wire::NotFoundProblem {
                detail: Some(failure.detail),
                reason: failure.reason.as_str().into(),
                resource: failure.resource.map(resource_context),
                stage: stage(failure.stage),
                status: 404,
                title: wire::NotFoundProblemTitle::NotFound,
                r#type: wire::NotFoundProblemType::UrnRadProblemNotFound,
            }),
        }
    }

    fn internal(_failure: InternalFailure) -> Self {
        Self::internal_transport()
    }

    pub(super) fn internal_transport() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: wire::Problem::InternalProblem(wire::InternalProblem {
                detail: Some("internal error".into()),
                incident: None,
                reason: wire::InternalProblemReason::Internal,
                status: 500,
                title: wire::InternalProblemTitle::InternalServerError,
                r#type: wire::InternalProblemType::UrnRadProblemInternal,
            }),
        }
    }
}

fn stage(value: Stage) -> wire::ProblemStage {
    match value {
        Stage::Schema => wire::ProblemStage::Schema,
        Stage::Preflight => wire::ProblemStage::Preflight,
        Stage::Binding => wire::ProblemStage::Binding,
        Stage::Planning => wire::ProblemStage::Planning,
        Stage::Execution => wire::ProblemStage::Execution,
        Stage::Storage => wire::ProblemStage::Storage,
    }
}

fn location(value: Location) -> wire::ProblemLocation {
    wire::ProblemLocation {
        binding: value.binding,
        node: value.node,
        pointer: value.pointer,
        role: value.role,
        scope: value.scope,
    }
}

fn execution_context(value: ExecutionContext) -> wire::ExecutionContext {
    wire::ExecutionContext {
        binding: value.binding,
        crossing: value.crossing,
        index: value.index,
        operator: value.operator,
        operator_id: value.operator_id,
        table: value.table,
    }
}

fn conflict_context(value: ConflictContext) -> wire::ConflictContext {
    wire::ConflictContext {
        index: value.index,
        object: value.object.map(|object| match object {
            ConflictObject::Table => wire::ConflictObject::Table,
            ConflictObject::Index => wire::ConflictObject::Index,
        }),
        operation: value.operation.map(|operation| match operation {
            ConflictOperation::Read => wire::ConflictOperation::Read,
            ConflictOperation::Insert => wire::ConflictOperation::Insert,
            ConflictOperation::Update => wire::ConflictOperation::Update,
            ConflictOperation::Delete => wire::ConflictOperation::Delete,
        }),
        table: value.table,
    }
}

fn resource_context(value: ResourceContext) -> wire::ResourceContext {
    wire::ResourceContext {
        kind: value.kind,
        name: value.name,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::service::error::{
        ConflictReason, ExecutionFailureReason, InvalidDiagnostic, InvalidReason,
    };

    #[test]
    fn empty_optional_contexts_are_omitted_from_problem_json() {
        let invalid = ResponseProblem::from_failure(Failure::Invalid(InvalidFailure {
            stage: Stage::Binding,
            reason: InvalidReason::UnknownColumn,
            detail: "unknown column".into(),
            location: Some(Location::default()),
            diagnostics: vec![
                InvalidDiagnostic {
                    reason: InvalidReason::UnknownColumn,
                    detail: "first".into(),
                    location: Some(Location::default()),
                },
                InvalidDiagnostic {
                    reason: InvalidReason::UnknownColumn,
                    detail: "second".into(),
                    location: None,
                },
            ],
        }));
        let invalid = serde_json::to_value(invalid.body).unwrap();
        assert!(invalid.get("location").is_none());
        assert!(invalid["errors"][0].get("location").is_none());

        let execution = ResponseProblem::from_failure(Failure::ExecutionFailed(ExecutionFailure {
            stage: Stage::Execution,
            reason: ExecutionFailureReason::DivisionByZero,
            detail: "division by zero".into(),
            location: Some(Location::default()),
            execution: Some(ExecutionContext::default()),
        }));
        let execution = serde_json::to_value(execution.body).unwrap();
        assert!(execution.get("location").is_none());
        assert!(execution.get("execution").is_none());

        let conflict = ResponseProblem::from_failure(Failure::Conflict(ConflictFailure {
            stage: Stage::Storage,
            reason: ConflictReason::SerializableConflict,
            detail: "conflict".into(),
            conflict: Some(ConflictContext::default()),
        }));
        let conflict = serde_json::to_value(conflict.body).unwrap();
        assert!(conflict.get("conflict").is_none());

        assert_ne!(invalid, Value::Null);
    }

    #[test]
    fn every_failure_class_selects_its_typed_problem_and_status() {
        let cases = [
            (
                Failure::Invalid(InvalidFailure {
                    stage: Stage::Schema,
                    reason: InvalidReason::SchemaViolation,
                    detail: "malformed".into(),
                    location: None,
                    diagnostics: Vec::new(),
                }),
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid",
                "malformed",
            ),
            (
                Failure::ExecutionFailed(ExecutionFailure {
                    stage: Stage::Execution,
                    reason: ExecutionFailureReason::DivisionByZero,
                    detail: "division by zero".into(),
                    location: None,
                    execution: None,
                }),
                StatusCode::UNPROCESSABLE_ENTITY,
                "execution_failed",
                "division by zero",
            ),
            (
                Failure::NotFound(NotFoundFailure {
                    stage: Stage::Preflight,
                    reason: crate::service::error::NotFoundReason::SchemaTransition,
                    detail: "missing".into(),
                    resource: None,
                }),
                StatusCode::NOT_FOUND,
                "not_found",
                "missing",
            ),
            (
                Failure::Conflict(ConflictFailure {
                    stage: Stage::Storage,
                    reason: ConflictReason::SerializableConflict,
                    detail: "raced".into(),
                    conflict: None,
                }),
                StatusCode::CONFLICT,
                "conflict",
                "raced",
            ),
        ];
        for (failure, expected_status, expected_code, expected_detail) in cases {
            let problem = ResponseProblem::from_failure(failure);
            assert_eq!(problem.status, expected_status);
            let value = serde_json::to_value(problem.body).unwrap();
            assert_eq!(value["code"], expected_code);
            assert_eq!(value["status"], i64::from(expected_status.as_u16()));
            assert_eq!(value["detail"], expected_detail);
        }

        let internal_error = crate::engine::exec::Error::message(
            crate::engine::exec::ErrorKind::Storage,
            "secret storage implementation detail",
        );
        let internal = ResponseProblem::from_failure(Failure::from_exec(&internal_error));
        assert_eq!(internal.status, StatusCode::INTERNAL_SERVER_ERROR);
        let value = serde_json::to_value(internal.body).unwrap();
        assert_eq!(value["code"], "internal");
        assert_eq!(value["reason"], "internal");
        assert_eq!(value["detail"], "internal error");
        assert!(!value.to_string().contains("secret storage"));

        let malformed = ResponseProblem::invalid(
            InvalidFailure {
                stage: Stage::Schema,
                reason: InvalidReason::SchemaViolation,
                detail: "malformed JSON".into(),
                location: None,
                diagnostics: Vec::new(),
            },
            StatusCode::BAD_REQUEST,
        );
        assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
        assert_eq!(serde_json::to_value(malformed.body).unwrap()["status"], 400);
    }
}
