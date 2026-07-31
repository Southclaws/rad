use super::generated::server::{GetHealthResponse, GetInfoResponse, MetaApi, TableListResponse};
use super::generated::types::{DatabaseInfo, DatabaseInfoMode, Health};
use super::{server, wire};
use crate::engine::catalog::model::Mode;

use super::server::Api;

#[async_trait::async_trait]
impl MetaApi for Api {
    async fn get_info(&self) -> GetInfoResponse {
        let (revision, _, _) = match self.engine.schema_migration_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => return info_problem(server::engine_problem(&error)),
        };
        let schema_version = match i64::try_from(revision.version.get()) {
            Ok(version) => version,
            Err(error) => {
                return info_problem(server::internal_problem("encode schema version", error));
            }
        };
        GetInfoResponse::Ok(DatabaseInfo {
            location: (!self.location.is_empty()).then(|| self.location.to_string()),
            mode: match self.mode {
                Mode::Direct => DatabaseInfoMode::Direct,
                Mode::Schema => DatabaseInfoMode::Schema,
            },
            schema_hash: revision.hash,
            schema_version,
            schema_version_at: (!revision.created_at.is_zero())
                .then(|| revision.created_at.as_datetime()),
        })
    }

    async fn get_health(&self) -> GetHealthResponse {
        GetHealthResponse::Ok(Health {
            mode: match self.mode {
                Mode::Direct => "direct",
                Mode::Schema => "schema",
            }
            .into(),
            status: "ok".into(),
        })
    }

    async fn table_list(&self) -> TableListResponse {
        let (_, tables, _) = match self.engine.schema_migration_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => return table_list_problem(server::engine_problem(&error)),
        };
        TableListResponse::Ok(wire::table_list(&tables))
    }
}

fn info_problem(problem: super::problem::ResponseProblem) -> GetInfoResponse {
    GetInfoResponse::Default(problem.status, problem.body)
}

fn table_list_problem(problem: super::problem::ResponseProblem) -> TableListResponse {
    TableListResponse::Default(problem.status, problem.body)
}
