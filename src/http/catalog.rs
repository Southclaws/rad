use axum::http::StatusCode;

use super::generated::server::{
    CatalogApi, ColumnCreateResponse, ColumnDeleteResponse, ColumnUpdateResponse,
    IndexCreateResponse, IndexDeleteResponse, TableCreateResponse, TableDeleteResponse,
    TableUpdateResponse,
};
use super::generated::types::{
    ColumnDef as WireColumnDef, ColumnUpdateProps, IndexInfo, TableDef as WireTableDef, TableInfo,
    TableUpdateProps,
};
use super::problem::ResponseProblem;
use super::server::Api;
use super::{server, wire};
use crate::engine::catalog::model::{Mode, Table};
use crate::engine::exec::{CatalogPolicy, Program, Statement};
use crate::service::error::{InvalidFailure, InvalidReason, Stage};

macro_rules! catalog_problem {
    ($response:ident, $problem:expr) => {{
        let problem = $problem;
        match problem.status {
            StatusCode::CONFLICT => $response::Conflict(problem.body),
            StatusCode::UNPROCESSABLE_ENTITY => $response::UnprocessableEntity(problem.body),
            status => $response::Default(status, problem.body),
        }
    }};
}

#[async_trait::async_trait]
impl CatalogApi for Api {
    async fn table_create(&self, body: Option<WireTableDef>) -> TableCreateResponse {
        if let Some(problem) = mode_gate(self.mode) {
            return catalog_problem!(TableCreateResponse, problem);
        }
        let Some(body) = body else {
            return catalog_problem!(TableCreateResponse, missing_body());
        };
        let name = body.name.clone();
        let table = match wire::table_draft(body) {
            Ok(table) => table,
            Err(error) => return catalog_problem!(TableCreateResponse, invalid(error)),
        };
        let tables = match run_catalog(
            self,
            Statement::CreateTable {
                name: "http_table_create".into(),
                table,
            },
        )
        .await
        {
            Ok(tables) => tables,
            Err(problem) => return catalog_problem!(TableCreateResponse, problem),
        };
        match table_response(&tables, &name) {
            Ok(table) => TableCreateResponse::Ok(table),
            Err(problem) => catalog_problem!(TableCreateResponse, *problem),
        }
    }

    async fn table_update(
        &self,
        table: String,
        body: Option<TableUpdateProps>,
    ) -> TableUpdateResponse {
        if let Some(problem) = mode_gate(self.mode) {
            return catalog_problem!(TableUpdateResponse, problem);
        }
        let Some(body) = body else {
            return catalog_problem!(TableUpdateResponse, missing_body());
        };
        let tables = match snapshot(self).await {
            Ok(tables) => tables,
            Err(problem) => return catalog_problem!(TableUpdateResponse, problem),
        };
        let Some(existing) = tables.iter().find(|candidate| candidate.name == table) else {
            return catalog_problem!(TableUpdateResponse, unknown_table(&table));
        };
        let new_name = body.name;
        let tables = match run_catalog(
            self,
            Statement::RenameTable {
                name: "http_table_update".into(),
                table_id: existing.schema_id,
                to: new_name.clone(),
            },
        )
        .await
        {
            Ok(tables) => tables,
            Err(problem) => return catalog_problem!(TableUpdateResponse, problem),
        };
        match table_response(&tables, &new_name) {
            Ok(table) => TableUpdateResponse::Ok(table),
            Err(problem) => catalog_problem!(TableUpdateResponse, *problem),
        }
    }

    async fn table_delete(&self, table: String) -> TableDeleteResponse {
        if let Some(problem) = mode_gate(self.mode) {
            return table_delete_problem(problem);
        }
        let tables = match snapshot(self).await {
            Ok(tables) => tables,
            Err(problem) => return table_delete_problem(problem),
        };
        let Some(existing) = tables.iter().find(|candidate| candidate.name == table) else {
            return table_delete_problem(unknown_table(&table));
        };
        match run_catalog(
            self,
            Statement::DeleteTable {
                name: "http_table_delete".into(),
                table_id: existing.schema_id,
            },
        )
        .await
        {
            Ok(_) => TableDeleteResponse::NoContent,
            Err(problem) => table_delete_problem(problem),
        }
    }

    async fn column_create(
        &self,
        table: String,
        body: Option<WireColumnDef>,
    ) -> ColumnCreateResponse {
        if let Some(problem) = mode_gate(self.mode) {
            return catalog_problem!(ColumnCreateResponse, problem);
        }
        let Some(body) = body else {
            return catalog_problem!(ColumnCreateResponse, missing_body());
        };
        let column = match wire::column_draft(body) {
            Ok(column) => column,
            Err(error) => return catalog_problem!(ColumnCreateResponse, invalid(error)),
        };
        let tables = match snapshot(self).await {
            Ok(tables) => tables,
            Err(problem) => return catalog_problem!(ColumnCreateResponse, problem),
        };
        let Some(existing) = tables.iter().find(|candidate| candidate.name == table) else {
            return catalog_problem!(ColumnCreateResponse, unknown_table(&table));
        };
        let tables = match run_catalog(
            self,
            Statement::CreateColumn {
                name: "http_column_create".into(),
                table_id: existing.schema_id,
                column,
            },
        )
        .await
        {
            Ok(tables) => tables,
            Err(problem) => return catalog_problem!(ColumnCreateResponse, problem),
        };
        match table_response(&tables, &table) {
            Ok(table) => ColumnCreateResponse::Ok(table),
            Err(problem) => catalog_problem!(ColumnCreateResponse, *problem),
        }
    }

    async fn column_update(
        &self,
        table: String,
        column: String,
        body: Option<ColumnUpdateProps>,
    ) -> ColumnUpdateResponse {
        if let Some(problem) = mode_gate(self.mode) {
            return catalog_problem!(ColumnUpdateResponse, problem);
        }
        let Some(body) = body else {
            return catalog_problem!(ColumnUpdateResponse, missing_body());
        };
        let tables = match snapshot(self).await {
            Ok(tables) => tables,
            Err(problem) => return catalog_problem!(ColumnUpdateResponse, problem),
        };
        let Some(existing) = tables.iter().find(|candidate| candidate.name == table) else {
            return catalog_problem!(ColumnUpdateResponse, unknown_table(&table));
        };
        let Some(existing_column) = existing.column(&column) else {
            return catalog_problem!(ColumnUpdateResponse, unknown_column(&table, &column));
        };
        let tables = match run_catalog(
            self,
            Statement::RenameColumn {
                name: "http_column_update".into(),
                table_id: existing.schema_id,
                column_id: existing_column.schema_id,
                to: body.name,
            },
        )
        .await
        {
            Ok(tables) => tables,
            Err(problem) => return catalog_problem!(ColumnUpdateResponse, problem),
        };
        match table_response(&tables, &table) {
            Ok(table) => ColumnUpdateResponse::Ok(table),
            Err(problem) => catalog_problem!(ColumnUpdateResponse, *problem),
        }
    }

    async fn column_delete(&self, table: String, column: String) -> ColumnDeleteResponse {
        if let Some(problem) = mode_gate(self.mode) {
            return catalog_problem!(ColumnDeleteResponse, problem);
        }
        let tables = match snapshot(self).await {
            Ok(tables) => tables,
            Err(problem) => return catalog_problem!(ColumnDeleteResponse, problem),
        };
        let Some(existing) = tables.iter().find(|candidate| candidate.name == table) else {
            return catalog_problem!(ColumnDeleteResponse, unknown_table(&table));
        };
        let Some(existing_column) = existing.column(&column) else {
            return catalog_problem!(ColumnDeleteResponse, unknown_column(&table, &column));
        };
        let tables = match run_catalog(
            self,
            Statement::DeleteColumn {
                name: "http_column_delete".into(),
                table_id: existing.schema_id,
                column_id: existing_column.schema_id,
            },
        )
        .await
        {
            Ok(tables) => tables,
            Err(problem) => return catalog_problem!(ColumnDeleteResponse, problem),
        };
        match table_response(&tables, &table) {
            Ok(table) => ColumnDeleteResponse::Ok(table),
            Err(problem) => catalog_problem!(ColumnDeleteResponse, *problem),
        }
    }

    async fn index_create(&self, table: String, body: Option<IndexInfo>) -> IndexCreateResponse {
        if let Some(problem) = mode_gate(self.mode) {
            return catalog_problem!(IndexCreateResponse, problem);
        }
        let Some(body) = body else {
            return catalog_problem!(IndexCreateResponse, missing_body());
        };
        let index = wire::index_def(body);
        let tables = match snapshot(self).await {
            Ok(tables) => tables,
            Err(problem) => return catalog_problem!(IndexCreateResponse, problem),
        };
        let Some(existing) = tables.iter().find(|candidate| candidate.name == table) else {
            return catalog_problem!(IndexCreateResponse, unknown_table(&table));
        };
        let tables = match run_catalog(
            self,
            Statement::CreateIndex {
                name: "http_index_create".into(),
                table_id: existing.schema_id,
                index,
            },
        )
        .await
        {
            Ok(tables) => tables,
            Err(problem) => return catalog_problem!(IndexCreateResponse, problem),
        };
        match table_response(&tables, &table) {
            Ok(table) => IndexCreateResponse::Ok(table),
            Err(problem) => catalog_problem!(IndexCreateResponse, *problem),
        }
    }

    async fn index_delete(&self, table: String, index: String) -> IndexDeleteResponse {
        if let Some(problem) = mode_gate(self.mode) {
            return catalog_problem!(IndexDeleteResponse, problem);
        }
        let tables = match snapshot(self).await {
            Ok(tables) => tables,
            Err(problem) => return catalog_problem!(IndexDeleteResponse, problem),
        };
        let Some(existing) = tables.iter().find(|candidate| candidate.name == table) else {
            return catalog_problem!(IndexDeleteResponse, unknown_table(&table));
        };
        if existing.index(&index).is_none() {
            return catalog_problem!(
                IndexDeleteResponse,
                invalid(format!("unknown index {table:?}.{index:?}"))
            );
        }
        let tables = match run_catalog(
            self,
            Statement::DeleteIndex {
                name: "http_index_delete".into(),
                table_id: existing.schema_id,
                index,
            },
        )
        .await
        {
            Ok(tables) => tables,
            Err(problem) => return catalog_problem!(IndexDeleteResponse, problem),
        };
        match table_response(&tables, &table) {
            Ok(table) => IndexDeleteResponse::Ok(table),
            Err(problem) => catalog_problem!(IndexDeleteResponse, *problem),
        }
    }
}

async fn run_catalog(api: &Api, statement: Statement) -> Result<Vec<Table>, ResponseProblem> {
    api.engine
        .execute_program(
            Program {
                statements: vec![statement],
                result: None,
            },
            CatalogPolicy::RevisionPerStatement,
        )
        .await
        .map_err(|error| server::engine_problem(&error))?;
    snapshot(api).await
}

async fn snapshot(api: &Api) -> Result<Vec<Table>, ResponseProblem> {
    api.engine
        .schema_migration_snapshot()
        .await
        .map(|(_, tables, _)| tables)
        .map_err(|error| server::engine_problem(&error))
}

fn table_response(tables: &[Table], name: &str) -> Result<TableInfo, Box<ResponseProblem>> {
    let table = tables
        .iter()
        .find(|candidate| candidate.name == name)
        .ok_or_else(|| {
            Box::new(server::internal_problem(
                "catalog mutation committed without its expected table",
                std::io::Error::other(format!("missing table {name:?}")),
            ))
        })?;
    wire::one_table(table, tables)
        .map_err(|error| Box::new(server::internal_problem("encode table", error)))
}

fn mode_gate(mode: Mode) -> Option<ResponseProblem> {
    (mode == Mode::Schema).then(|| {
        invalid("catalog changes are disabled because this database is managed by rad.schema.yaml")
    })
}

fn missing_body() -> ResponseProblem {
    ResponseProblem::invalid(
        InvalidFailure {
            stage: Stage::Schema,
            reason: InvalidReason::SchemaViolation,
            detail: "request body is required".into(),
            location: None,
            diagnostics: Vec::new(),
        },
        StatusCode::BAD_REQUEST,
    )
}

fn invalid(detail: impl Into<String>) -> ResponseProblem {
    invalid_with_reason(InvalidReason::Invalid, detail)
}

fn unknown_table(table: &str) -> ResponseProblem {
    invalid_with_reason(
        InvalidReason::UnknownTable,
        format!("unknown table {table:?}"),
    )
}

fn unknown_column(table: &str, column: &str) -> ResponseProblem {
    invalid_with_reason(
        InvalidReason::UnknownColumn,
        format!("unknown column {table:?}.{column:?}"),
    )
}

fn invalid_with_reason(reason: InvalidReason, detail: impl Into<String>) -> ResponseProblem {
    ResponseProblem::invalid(
        InvalidFailure {
            stage: Stage::Preflight,
            reason,
            detail: detail.into(),
            location: None,
            diagnostics: Vec::new(),
        },
        StatusCode::UNPROCESSABLE_ENTITY,
    )
}

fn table_delete_problem(problem: ResponseProblem) -> TableDeleteResponse {
    match problem.status {
        StatusCode::CONFLICT => TableDeleteResponse::Conflict(problem.body),
        StatusCode::UNPROCESSABLE_ENTITY => TableDeleteResponse::UnprocessableEntity(problem.body),
        status => TableDeleteResponse::Default(status, problem.body),
    }
}
