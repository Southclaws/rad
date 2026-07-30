use axum::http::StatusCode;

use super::generated::server::{
    GetSchemaResponse, SchemaApi, SchemaCompatibilityResponse, SchemaDiffResponse,
    SchemaMigrateResponse,
};
use super::generated::types::{
    SchemaCompatibilityRequest, SchemaDiffResult, SchemaMigrateRequest, SchemaMigrateResult,
    SchemaRequest, SchemaState,
};
use super::problem::ResponseProblem;
use super::server::Api;
use super::{server, wire};
use crate::engine::exec::{Error, ErrorKind, ErrorReason};
use crate::engine::frontend::migration::Migration;
use crate::service::error::{InvalidFailure, InvalidReason, Stage};

#[async_trait::async_trait]
impl SchemaApi for Api {
    async fn get_schema(&self) -> GetSchemaResponse {
        let (revision, _, _) = match self.engine.schema_migration_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => return get_schema_problem(server::engine_problem(&error)),
        };
        let version = match i64::try_from(revision.version.get()) {
            Ok(version) => version,
            Err(error) => {
                return get_schema_problem(server::internal_problem(
                    "encode schema version",
                    error,
                ));
            }
        };
        GetSchemaResponse::Ok(SchemaState {
            schema: wire::schema_document(&revision.schema),
            schema_hash: revision.hash,
            schema_version: version,
        })
    }

    async fn schema_diff(&self, body: Option<SchemaRequest>) -> SchemaDiffResponse {
        let Some(body) = body else {
            return schema_diff_problem(missing_body());
        };
        let plan = match Migration::new(&self.engine)
            .plan_file("rad.schema.yaml", body.schema.as_bytes())
            .await
        {
            Ok(plan) => plan,
            Err(error) => return schema_diff_problem(server::engine_problem(&error)),
        };
        let current_version = match i64::try_from(plan.current.version.get()) {
            Ok(version) => version,
            Err(error) => {
                return schema_diff_problem(server::internal_problem(
                    "encode schema diff version",
                    error,
                ));
            }
        };
        let program = match wire::migration_program(&plan.program) {
            Ok(program) => program,
            Err(error) => {
                return schema_diff_problem(server::internal_problem(
                    "encode schema diff program",
                    error,
                ));
            }
        };
        let destructive = match wire::schema_findings(&plan.destructive) {
            Ok(findings) => findings,
            Err(error) => {
                return schema_diff_problem(server::internal_problem(
                    "encode destructive schema findings",
                    error,
                ));
            }
        };
        let blocking = match wire::schema_findings(&plan.blocking) {
            Ok(findings) => findings,
            Err(error) => {
                return schema_diff_problem(server::internal_problem(
                    "encode blocking schema findings",
                    error,
                ));
            }
        };
        SchemaDiffResponse::Ok(SchemaDiffResult {
            blocking,
            changes: wire::schema_changes(&plan.steps),
            current_hash: plan.current.hash,
            current_version,
            desired_hash: plan.desired_hash,
            destructive,
            program,
        })
    }

    async fn schema_migrate(&self, body: Option<SchemaMigrateRequest>) -> SchemaMigrateResponse {
        let Some(body) = body else {
            return schema_migrate_problem(missing_body());
        };
        let requested_version = match u64::try_from(body.current_version) {
            Ok(version) => version,
            Err(_) => {
                return schema_migrate_problem(invalid_problem(
                    InvalidReason::SchemaClientOutdated,
                    "schema version must be non-negative",
                    StatusCode::UNPROCESSABLE_ENTITY,
                ));
            }
        };
        let migration = Migration::new(&self.engine);
        let plan = match migration
            .plan_file("rad.schema.yaml", body.schema.as_bytes())
            .await
        {
            Ok(plan) => plan,
            Err(error) => return schema_migrate_problem(server::engine_problem(&error)),
        };
        let identity_matches = plan.current.version.get() == requested_version
            && plan.current.hash == body.current_hash;
        let recovers_committed_request = plan.blocking.is_empty()
            && plan.program.statements.is_empty()
            && (plan.current.hash == plan.desired_hash || !plan.transitions.is_empty());
        if !identity_matches && !recovers_committed_request {
            let error = Error::with_reason(
                ErrorKind::Conflict,
                ErrorReason::SerializableConflict,
                format!(
                    "schema changed since preflight: expected version {} ({}), found version {} ({})",
                    body.current_version,
                    body.current_hash,
                    plan.current.version,
                    plan.current.hash
                ),
            );
            return schema_migrate_problem(server::engine_problem(&error));
        }
        let result = match migration
            .apply_plan(plan, body.accept_data_loss.unwrap_or(false))
            .await
        {
            Ok(result) => result,
            Err(error) => return schema_migrate_problem(server::engine_problem(&error)),
        };
        let version = match i64::try_from(result.revision.version.get()) {
            Ok(version) => version,
            Err(error) => {
                return schema_migrate_problem(server::internal_problem(
                    "encode migrated schema version",
                    error,
                ));
            }
        };
        SchemaMigrateResponse::Ok(SchemaMigrateResult {
            changes: wire::schema_changes(&result.plan.steps),
            desired_hash: result.plan.desired_hash,
            schema: wire::schema_document(&result.revision.schema),
            schema_hash: result.revision.hash,
            schema_version: version,
            state: wire::migration_state(result.state),
            transition_ids: result
                .transition_ids
                .into_iter()
                .map(|id| id.to_string())
                .collect(),
        })
    }

    async fn schema_compatibility(
        &self,
        body: Option<SchemaCompatibilityRequest>,
    ) -> SchemaCompatibilityResponse {
        let Some(body) = body else {
            return schema_compatibility_problem(missing_body());
        };
        let client_version = match u64::try_from(body.schema_version) {
            Ok(version) => version,
            Err(_) => {
                return schema_compatibility_problem(invalid_problem(
                    InvalidReason::SchemaClientOutdated,
                    "schema version must be non-negative",
                    StatusCode::UNPROCESSABLE_ENTITY,
                ));
            }
        };
        let (revision, _, _) = match self.engine.schema_migration_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => return schema_compatibility_problem(server::engine_problem(&error)),
        };
        let server_version = revision.version.get();
        if client_version < server_version {
            return schema_compatibility_problem(invalid_problem(
                InvalidReason::SchemaClientOutdated,
                format!(
                    "this client was generated for schema version {client_version}, but the database is currently on version {server_version}"
                ),
                StatusCode::UNPROCESSABLE_ENTITY,
            ));
        }
        if client_version > server_version {
            return schema_compatibility_problem(invalid_problem(
                InvalidReason::SchemaServerOutdated,
                format!(
                    "this client expects schema version {client_version}, but the database is currently on version {server_version}"
                ),
                StatusCode::UNPROCESSABLE_ENTITY,
            ));
        }
        if body.schema_hash != revision.hash {
            return schema_compatibility_problem(invalid_problem(
                InvalidReason::SchemaHistoryDiverged,
                format!(
                    "client and server both report schema version {client_version}, but their schema hashes differ"
                ),
                StatusCode::UNPROCESSABLE_ENTITY,
            ));
        }
        SchemaCompatibilityResponse::NoContent
    }
}

fn missing_body() -> ResponseProblem {
    invalid_problem(
        InvalidReason::SchemaViolation,
        "request body is required",
        StatusCode::BAD_REQUEST,
    )
}

fn invalid_problem(
    reason: InvalidReason,
    detail: impl Into<String>,
    status: StatusCode,
) -> ResponseProblem {
    ResponseProblem::invalid(
        InvalidFailure {
            stage: Stage::Schema,
            reason,
            detail: detail.into(),
            location: None,
            diagnostics: Vec::new(),
        },
        status,
    )
}

fn get_schema_problem(problem: ResponseProblem) -> GetSchemaResponse {
    GetSchemaResponse::Default(problem.status, problem.body)
}

fn schema_diff_problem(problem: ResponseProblem) -> SchemaDiffResponse {
    match problem.status {
        StatusCode::UNPROCESSABLE_ENTITY => SchemaDiffResponse::UnprocessableEntity(problem.body),
        status => SchemaDiffResponse::Default(status, problem.body),
    }
}

fn schema_migrate_problem(problem: ResponseProblem) -> SchemaMigrateResponse {
    match problem.status {
        StatusCode::CONFLICT => SchemaMigrateResponse::Conflict(problem.body),
        StatusCode::UNPROCESSABLE_ENTITY => {
            SchemaMigrateResponse::UnprocessableEntity(problem.body)
        }
        status => SchemaMigrateResponse::Default(status, problem.body),
    }
}

fn schema_compatibility_problem(problem: ResponseProblem) -> SchemaCompatibilityResponse {
    match problem.status {
        StatusCode::UNPROCESSABLE_ENTITY => {
            SchemaCompatibilityResponse::UnprocessableEntity(problem.body)
        }
        status => SchemaCompatibilityResponse::Default(status, problem.body),
    }
}
