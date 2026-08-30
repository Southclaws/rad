//! PostgreSQL wire-protocol compatibility frontend.

use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use futures::{Sink, stream};
use pgwire::api::auth::StartupHandler;
use pgwire::api::auth::noop::NoopStartupHandler;
use pgwire::api::portal::{Format, Portal};
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldFormat, FieldInfo,
    QueryResponse, Response, Tag,
};
use pgwire::api::stmt::{QueryParser, StoredStatement};
use pgwire::api::store::PortalStore;
use pgwire::api::{ClientInfo, ClientPortalStore, PgWireServerHandlers, Type};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::response::TransactionStatus;
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use pgwire::tokio::process_socket;
use sqlparser::ast::{
    Statement as SqlStatement, TransactionAccessMode, TransactionIsolationLevel, TransactionMode,
};
use tokio::sync::Mutex;
use tracing::Instrument as _;

use crate::engine::catalog::Catalog;
use crate::engine::catalog::model::{Mode, ScalarType, Table};
use crate::engine::exec::{CatalogPolicy, Engine, ErrorReason};
use crate::engine::frontend::Tx;
use crate::engine::lir::{Datum, RawScalar, Value};
use crate::sql::{self, CommandKind, Parameter, Prepared, ResultColumn};

#[derive(Clone)]
pub struct Server {
    backend: Arc<Backend>,
}

impl Server {
    pub fn new(engine: Arc<Engine>, catalog: Arc<Catalog>, mode: Mode) -> Self {
        Self {
            backend: Arc::new(Backend {
                engine,
                parser: Arc::new(ParserBackend { catalog }),
                mode,
            }),
        }
    }
}

impl PgWireServerHandlers for Server {
    fn simple_query_handler(&self) -> Arc<impl SimpleQueryHandler> {
        self.backend.clone()
    }

    fn extended_query_handler(&self) -> Arc<impl ExtendedQueryHandler> {
        self.backend.clone()
    }

    fn startup_handler(&self) -> Arc<impl StartupHandler> {
        self.backend.clone()
    }
}

pub async fn serve(
    listener: tokio::net::TcpListener,
    server: Server,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let server = Arc::new(server);
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let (socket, peer) = accepted?;
                let server = server.clone();
                connections.spawn(async move {
                    let span = tracing::info_span!(
                        target: "rad::telemetry",
                        "postgres.connection",
                        otel.kind = "server",
                        network.transport = "tcp",
                        network.peer.address = %peer.ip(),
                        network.peer.port = peer.port(),
                        rad.status = tracing::field::Empty,
                        error.type = tracing::field::Empty,
                        otel.status_code = tracing::field::Empty,
                    );
                    let (trace_id, span_id) = crate::telemetry::span_ids(&span);
                    crate::telemetry::postgres_connection_started();
                    tracing::debug!(
                        target: "rad",
                        event = "postgres.connection_started",
                        component = "postgres",
                        trace_id,
                        span_id,
                        client_ip = %peer.ip(),
                        message = "PostgreSQL connection started"
                    );
                    let result = process_socket(socket, None, server)
                        .instrument(span.clone())
                        .await;
                    span.record("rad.status", if result.is_ok() { "success" } else { "error" });
                    if let Err(error) = &result {
                        span.record("error.type", stable_postgres_error_reason(error));
                        span.record("otel.status_code", "ERROR");
                    }
                    crate::telemetry::postgres_connection_finished(if result.is_ok() {
                        "success"
                    } else {
                        "error"
                    });
                    if let Err(error) = &result
                        && !expected_postgres_disconnect(error)
                    {
                        tracing::warn!(
                            target: "rad",
                            event = "postgres.connection_failed",
                            component = "postgres",
                            trace_id,
                            span_id,
                            client_ip = %peer.ip(),
                            error_kind = "protocol",
                            error_reason = stable_postgres_error_reason(error),
                            message = "PostgreSQL connection failed"
                        );
                    }
                    tracing::debug!(
                        target: "rad",
                        event = "postgres.connection_stopped",
                        component = "postgres",
                        trace_id,
                        span_id,
                        client_ip = %peer.ip(),
                        status = if result.is_ok() { "success" } else { "error" },
                        message = "PostgreSQL connection stopped"
                    );
                });
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

fn stable_postgres_error_reason(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted => {
            "connection_closed"
        }
        _ => "io",
    }
}

fn expected_postgres_disconnect(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
    )
}

struct Backend {
    engine: Arc<Engine>,
    parser: Arc<ParserBackend>,
    mode: Mode,
}

#[async_trait]
impl NoopStartupHandler for Backend {
    async fn post_startup<C>(
        &self,
        client: &mut C,
        _message: PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        client
            .session_extensions()
            .get_or_insert_with(|| Mutex::new(Session::default()));
        Ok(())
    }
}

#[async_trait]
impl SimpleQueryHandler for Backend {
    async fn do_query<C>(&self, client: &mut C, sql: &str) -> PgWireResult<Vec<Response>>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let statements = sql::parse(sql).map_err(sql_error)?;
        let mut responses = Vec::with_capacity(statements.len());
        for statement in statements {
            let statement = statement.to_string();
            let prepared = self.parser.prepare(client, &statement, &[]).await?;
            match Box::pin(self.execute(client, &prepared, &[], &Format::UnifiedText)).await {
                Ok(response) => responses.push(response),
                Err(PgWireError::UserError(error)) => {
                    responses.push(Response::Error(error));
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(responses)
    }
}

#[async_trait]
impl ExtendedQueryHandler for Backend {
    type Statement = PgPrepared;
    type QueryParser = ParserBackend;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.parser.clone()
    }

    async fn do_query<C>(
        &self,
        client: &mut C,
        portal: &Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let parameters = decode_parameters(portal)?;
        Box::pin(self.execute(
            client,
            &portal.statement.statement,
            &parameters,
            &portal.result_column_format,
        ))
        .await
    }

    async fn do_describe_statement<C>(
        &self,
        _client: &mut C,
        statement: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        Ok(DescribeStatementResponse::new(
            statement
                .statement
                .parameter_types()
                .iter()
                .copied()
                .map(pg_type)
                .collect(),
            fields(statement.statement.result_columns(), None),
        ))
    }

    async fn do_describe_portal<C>(
        &self,
        _client: &mut C,
        portal: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        Ok(DescribePortalResponse::new(fields(
            portal.statement.statement.result_columns(),
            Some(&portal.result_column_format),
        )))
    }
}

impl Backend {
    async fn execute<C>(
        &self,
        client: &mut C,
        prepared: &PgPrepared,
        parameters: &[Parameter],
        format: &Format,
    ) -> PgWireResult<Response>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let response = Box::pin(async {
            match prepared {
                PgPrepared::Control(control) => self.execute_control(client, *control).await,
                PgPrepared::Catalog(plan) => {
                    self.reject_failed_transaction(client).await?;
                    let tables = self.parser.tables(client).await?;
                    let result = plan.execute(&tables, parameters)?;
                    query_response(result.columns, result.rows, CommandKind::Select, format)
                }
                PgPrepared::Sql(prepared) => {
                    self.reject_failed_transaction(client).await?;
                    let tables = self.parser.tables(client).await?;
                    let compiled =
                        sql::compile(prepared, &tables, parameters).map_err(sql_error)?;
                    let Some(program) = compiled.program else {
                        return Ok(Response::Execution(Tag::new(compiled.kind.tag())));
                    };
                    let catalog_policy = match self.mode {
                        Mode::Direct => CatalogPolicy::RevisionPerStatement,
                        Mode::Schema => CatalogPolicy::Forbidden,
                    };
                    let sql = prepared.sql();
                    let session = session(client);
                    let mut state = session.lock().await;
                    let request_id = uuid::Uuid::new_v4().to_string();
                    let client_ip = client.socket_addr().ip().to_string();
                    let result = if let Some(transaction) = &mut state.transaction {
                        let transaction_span = transaction.span();
                        let (trace_id, span_id) = crate::telemetry::span_ids(&transaction_span);
                        let context = crate::logging::RequestContext {
                            transport: "postgres",
                            request_id,
                            transaction_id: transaction.id().to_owned(),
                            client_ip,
                            transaction_state: "explicit",
                            trace_id,
                            span_id,
                            diagnostics: None,
                            parent_span: Some(transaction_span),
                        };
                        Box::pin(crate::logging::with_request_context(
                            context,
                            transaction.execute_program(program, catalog_policy),
                        ))
                        .await
                        .map_err(|error| engine_error_with_sql(error, &sql))
                    } else {
                        drop(state);
                        let current_span = tracing::Span::current();
                        let (trace_id, span_id) = crate::telemetry::span_ids(&current_span);
                        let context = crate::logging::RequestContext {
                            transport: "postgres",
                            request_id,
                            transaction_id: String::new(),
                            client_ip,
                            transaction_state: "implicit",
                            trace_id,
                            span_id,
                            diagnostics: None,
                            parent_span: None,
                        };
                        let options = crate::engine::exec::ProgramOptions {
                            catalog: catalog_policy,
                            ..crate::engine::exec::ProgramOptions::default()
                        };
                        Box::pin(crate::logging::with_request_context(
                            context,
                            crate::engine::frontend::execute_program_with_options(
                                &self.engine,
                                program,
                                options,
                            ),
                        ))
                        .await
                        .map_err(|error| engine_error_with_sql(error, &sql))
                    }?;
                    let affected = result
                        .statements
                        .iter()
                        .filter(|statement| statement.name != "sql_result")
                        .map(|statement| statement.affected)
                        .sum();
                    if compiled.result_columns.is_empty() {
                        let tag = if compiled.kind == CommandKind::Insert {
                            Tag::new("INSERT").with_oid(0).with_rows(affected)
                        } else {
                            Tag::new(compiled.kind.tag()).with_rows(affected)
                        };
                        Ok(Response::Execution(tag))
                    } else {
                        query_response(
                            compiled.result_columns,
                            datum_rows(result.result)?,
                            compiled.kind,
                            format,
                        )
                    }
                }
            }
        })
        .await;
        if response.is_err() {
            let session = session(client);
            let mut state = session.lock().await;
            if state.transaction.is_some() {
                state.failed = true;
            }
        }
        response
    }

    async fn reject_failed_transaction<C>(&self, client: &C) -> PgWireResult<()>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let session = session(client);
        let state = session.lock().await;
        if state.failed || client.transaction_status() == TransactionStatus::Error {
            return Err(user_error(
                "25P02",
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        Ok(())
    }

    async fn execute_control<C>(
        &self,
        client: &mut C,
        control: TransactionControl,
    ) -> PgWireResult<Response>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let session = session(client);
        match control {
            TransactionControl::Begin => {
                let mut state = session.lock().await;
                if state.transaction.is_none() {
                    state.transaction =
                        Some(Tx::begin(self.engine.clone()).await.map_err(engine_error)?);
                    state.failed = false;
                    tracing::debug!(
                        target: "rad",
                        event = "transaction.started",
                        component = "postgres",
                        transaction_id = state.transaction.as_ref().map_or("", Tx::id),
                        client_ip = %client.socket_addr().ip(),
                        message = "PostgreSQL transaction started"
                    );
                }
                Ok(Response::TransactionStart(Tag::new("BEGIN")))
            }
            TransactionControl::Commit => {
                let (transaction, failed) = {
                    let mut state = session.lock().await;
                    let failed =
                        state.failed || client.transaction_status() == TransactionStatus::Error;
                    state.failed = false;
                    (state.transaction.take(), failed)
                };
                match transaction {
                    Some(transaction) if failed => {
                        let transaction_id = transaction.id().to_owned();
                        transaction.rollback();
                        log_transaction_completed(
                            &transaction_id,
                            client.socket_addr().ip(),
                            "rollback",
                            "success",
                        );
                        Ok(Response::TransactionEnd(Tag::new("ROLLBACK")))
                    }
                    Some(transaction) => {
                        let transaction_id = transaction.id().to_owned();
                        let result = transaction.commit().await;
                        log_transaction_completed(
                            &transaction_id,
                            client.socket_addr().ip(),
                            "commit",
                            if result.is_ok() { "success" } else { "error" },
                        );
                        result.map_err(engine_error)?;
                        Ok(Response::TransactionEnd(Tag::new("COMMIT")))
                    }
                    None => Ok(Response::TransactionEnd(Tag::new("COMMIT"))),
                }
            }
            TransactionControl::Rollback => {
                let transaction = {
                    let mut state = session.lock().await;
                    state.failed = false;
                    state.transaction.take()
                };
                if let Some(transaction) = transaction {
                    let transaction_id = transaction.id().to_owned();
                    transaction.rollback();
                    log_transaction_completed(
                        &transaction_id,
                        client.socket_addr().ip(),
                        "rollback",
                        "success",
                    );
                }
                Ok(Response::TransactionEnd(Tag::new("ROLLBACK")))
            }
        }
    }
}

fn log_transaction_completed(
    transaction_id: &str,
    client_ip: std::net::IpAddr,
    outcome: &'static str,
    status: &'static str,
) {
    tracing::info!(
        target: "rad::program",
        event = "transaction.completed",
        component = "postgres",
        transport = "postgres",
        transaction_id,
        client_ip = %client_ip,
        outcome,
        status,
        message = "PostgreSQL transaction completed"
    );
}

#[derive(Clone)]
struct ParserBackend {
    catalog: Arc<Catalog>,
}

#[async_trait]
impl QueryParser for ParserBackend {
    type Statement = PgPrepared;

    async fn parse_sql<C>(
        &self,
        client: &C,
        sql: &str,
        types: &[Option<Type>],
    ) -> PgWireResult<Self::Statement>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        self.prepare(client, sql, types).await
    }

    fn get_parameter_types(&self, statement: &Self::Statement) -> PgWireResult<Vec<Type>> {
        Ok(statement
            .parameter_types()
            .iter()
            .copied()
            .map(pg_type)
            .collect())
    }

    fn get_result_schema(
        &self,
        statement: &Self::Statement,
        format: Option<&Format>,
    ) -> PgWireResult<Vec<FieldInfo>> {
        Ok(fields(statement.result_columns(), format))
    }
}

impl ParserBackend {
    async fn prepare<C>(
        &self,
        client: &C,
        sql: &str,
        types: &[Option<Type>],
    ) -> PgWireResult<PgPrepared>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        if let Some(control) = transaction_control(sql)? {
            return Ok(PgPrepared::Control(control));
        }
        if let Some(plan) = CatalogPlan::recognize(sql) {
            return Ok(PgPrepared::Catalog(plan));
        }
        let tables = self.tables(client).await?;
        let hints = types
            .iter()
            .map(|value| value.as_ref().and_then(scalar_from_pg))
            .collect::<Vec<_>>();
        sql::prepare(sql, &tables, &hints)
            .map(Box::new)
            .map(PgPrepared::Sql)
            .map_err(sql_error)
    }

    async fn tables<C>(&self, client: &C) -> PgWireResult<Vec<Table>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let session = session(client);
        let mut state = session.lock().await;
        match &mut state.transaction {
            Some(transaction) => transaction.list_tables().await.map_err(engine_error),
            None => {
                drop(state);
                self.catalog.list_tables().await.map_err(api_error)
            }
        }
    }
}

#[derive(Clone)]
enum PgPrepared {
    Sql(Box<Prepared>),
    Catalog(CatalogPlan),
    Control(TransactionControl),
}

impl PgPrepared {
    fn parameter_types(&self) -> Vec<ScalarType> {
        match self {
            Self::Sql(statement) => statement.parameter_types().to_vec(),
            Self::Catalog(plan) => vec![ScalarType::Text; plan.parameter_count],
            Self::Control(_) => Vec::new(),
        }
    }

    fn result_columns(&self) -> Vec<ResultColumn> {
        match self {
            Self::Sql(statement) => statement.result_columns().to_vec(),
            Self::Catalog(plan) => plan.columns.clone(),
            Self::Control(_) => Vec::new(),
        }
    }

    fn sql(&self) -> Option<String> {
        match self {
            Self::Sql(statement) => Some(statement.sql()),
            Self::Catalog(_) | Self::Control(_) => None,
        }
    }
}

#[derive(Clone, Copy)]
enum TransactionControl {
    Begin,
    Commit,
    Rollback,
}

#[derive(Default)]
struct Session {
    transaction: Option<Tx>,
    failed: bool,
}

fn session<C: ClientInfo>(client: &C) -> Arc<Mutex<Session>> {
    client
        .session_extensions()
        .get_or_insert_with(|| Mutex::new(Session::default()))
}

fn transaction_control(sql: &str) -> PgWireResult<Option<TransactionControl>> {
    let mut statements = sql::parse(sql).map_err(sql_error)?;
    if statements.len() != 1 {
        return Ok(None);
    }
    let control = match statements.remove(0) {
        SqlStatement::StartTransaction {
            modes,
            modifier,
            statements,
            exception,
            has_end_keyword,
            ..
        } => {
            if modifier.is_some()
                || !statements.is_empty()
                || exception.is_some()
                || has_end_keyword
                || modes.iter().any(|mode| {
                    !matches!(
                        mode,
                        TransactionMode::IsolationLevel(TransactionIsolationLevel::Serializable)
                            | TransactionMode::AccessMode(TransactionAccessMode::ReadWrite)
                    )
                })
            {
                return Err(user_error(
                    "0A000",
                    "Rad supports serializable, read-write transactions without modifiers",
                ));
            }
            Some(TransactionControl::Begin)
        }
        SqlStatement::Commit {
            chain,
            end,
            modifier,
        } if !chain && !end && modifier.is_none() => Some(TransactionControl::Commit),
        SqlStatement::Rollback { chain, savepoint } if !chain && savepoint.is_none() => {
            Some(TransactionControl::Rollback)
        }
        SqlStatement::Commit { .. } | SqlStatement::Rollback { .. } => {
            return Err(user_error(
                "0A000",
                "transaction chaining and savepoints are not supported",
            ));
        }
        _ => None,
    };
    Ok(control)
}

#[derive(Clone)]
struct CatalogPlan {
    kind: CatalogKind,
    parameter_count: usize,
    columns: Vec<ResultColumn>,
}

#[derive(Clone, Copy)]
enum CatalogKind {
    Version,
    Settings,
    Schemas,
    TableCount,
    Tables,
    Columns,
    Indexes,
    ForeignKeys,
    Empty,
}

struct CatalogResult {
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<Datum>>,
}

impl CatalogPlan {
    fn recognize(sql: &str) -> Option<Self> {
        let normalized = sql
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        let parameter_count = parameter_count(sql);
        let (kind, columns) = if normalized.starts_with("show server_version_num") {
            (
                CatalogKind::Version,
                vec![text_column("server_version_num", false)],
            )
        } else if normalized.contains("current_setting('server_version_num')") {
            (
                CatalogKind::Settings,
                vec![
                    text_column("current_setting", false),
                    text_column("current_setting", true),
                    text_column("current_setting", true),
                ],
            )
        } else if normalized.contains("pg_catalog.pg_namespace")
            && normalized.contains("schema_name")
        {
            (
                CatalogKind::Schemas,
                vec![
                    text_column("schema_name", false),
                    text_column("comment", true),
                ],
            )
        } else if normalized.contains("count(*)")
            && normalized.contains("information_schema.tables")
        {
            (CatalogKind::TableCount, vec![int_column("count", false)])
        } else if normalized.contains("information_schema.tables") {
            (
                CatalogKind::Tables,
                vec![
                    int_column("oid", false),
                    text_column("table_schema", false),
                    text_column("table_name", false),
                    text_column("comment", true),
                    text_column("partition_attrs", true),
                    text_column("partition_strategy", true),
                    text_column("partition_exprs", true),
                    text_column("attrs", false),
                ],
            )
        } else if normalized.contains("\"information_schema\".\"columns\"")
            || normalized.contains("information_schema.columns")
        {
            (CatalogKind::Columns, atlas_column_columns())
        } else if normalized.contains("pg_index") {
            (CatalogKind::Indexes, atlas_index_columns())
        } else if normalized.contains("pg_constraint") && normalized.contains("contype = 'f'") {
            (CatalogKind::ForeignKeys, atlas_foreign_key_columns())
        } else if normalized.contains("pg_catalog")
            || normalized.contains("information_schema")
            || normalized.contains("from pg_")
            || normalized.contains("join pg_")
        {
            (CatalogKind::Empty, catalog_select_columns(sql))
        } else {
            return None;
        };
        Some(Self {
            kind,
            parameter_count,
            columns,
        })
    }

    fn execute(&self, tables: &[Table], parameters: &[Parameter]) -> PgWireResult<CatalogResult> {
        if parameters.len() != self.parameter_count {
            return Err(user_error(
                "08P01",
                format!(
                    "catalog query expected {} parameters, got {}",
                    self.parameter_count,
                    parameters.len()
                ),
            ));
        }
        let rows = match self.kind {
            CatalogKind::Version => vec![vec![text("170000")]],
            CatalogKind::Settings => vec![vec![text("170000"), text("heap"), Datum::Null]],
            CatalogKind::Schemas => vec![vec![text("public"), Datum::Null]],
            CatalogKind::TableCount => {
                let requested = parameters
                    .last()
                    .and_then(|parameter| match &parameter.value {
                        RawScalar::Text(value) => Some(value.as_str()),
                        _ => None,
                    });
                let count = requested.map_or(tables.len(), |name| {
                    usize::from(tables.iter().any(|table| table.name == name))
                });
                vec![vec![int(count as i64)]]
            }
            CatalogKind::Tables => tables
                .iter()
                .map(|table| {
                    vec![
                        int(table.schema_id.get() as i64),
                        text("public"),
                        text(&table.name),
                        Datum::Null,
                        Datum::Null,
                        Datum::Null,
                        Datum::Null,
                        text("{}"),
                    ]
                })
                .collect(),
            CatalogKind::Columns => tables
                .iter()
                .flat_map(|table| {
                    table
                        .columns
                        .iter()
                        .enumerate()
                        .map(move |(position, column)| {
                            let (data_type, format_type, oid) =
                                catalog_type(column.scalar_type, &column.format);
                            vec![
                                text(&table.name),
                                text(&column.name),
                                text(data_type),
                                text(format_type),
                                text(if column.nullable { "YES" } else { "NO" }),
                                default_datum(
                                    column.insert_default.as_ref(),
                                    column.scalar_type,
                                    &column.format,
                                ),
                                Datum::Null,
                                Datum::Null,
                                if column.format.contains("timestamp") {
                                    int(6)
                                } else {
                                    Datum::Null
                                },
                                Datum::Null,
                                Datum::Null,
                                Datum::Null,
                                Datum::Null,
                                text("NO"),
                                Datum::Null,
                                Datum::Null,
                                Datum::Null,
                                Datum::Null,
                                Datum::Null,
                                Datum::Null,
                                text("b"),
                                int(0),
                                int(oid),
                                int((position + 1) as i64),
                            ]
                        })
                })
                .collect(),
            CatalogKind::Indexes => tables
                .iter()
                .flat_map(|table| {
                    let primary = (!table.primary_key.is_empty()).then(|| {
                        (
                            format!("{}_pkey", table.name),
                            table.primary_key.clone(),
                            true,
                            true,
                        )
                    });
                    primary
                        .into_iter()
                        .chain(table.indexes.iter().map(|index| {
                            (
                                index.name.clone(),
                                table
                                    .index_column_names(index)
                                    .into_iter()
                                    .map(str::to_owned)
                                    .collect(),
                                index.unique,
                                false,
                            )
                        }))
                        .flat_map(move |(name, columns, unique, primary)| {
                            columns.into_iter().map(move |column| {
                                vec![
                                    text(&table.name),
                                    text(&name),
                                    text("btree"),
                                    text(&column),
                                    bool_value(false),
                                    bool_value(primary),
                                    bool_value(unique),
                                    Datum::Null,
                                    Datum::Null,
                                    Datum::Null,
                                    Datum::Null,
                                    bool_value(false),
                                    bool_value(false),
                                    bool_value(false),
                                    Datum::Null,
                                    Datum::Null,
                                    Datum::Null,
                                    Datum::Null,
                                    bool_value(true),
                                    Datum::Null,
                                    bool_value(false),
                                ]
                            })
                        })
                })
                .collect(),
            CatalogKind::ForeignKeys => tables
                .iter()
                .flat_map(|table| {
                    table.foreign_keys.iter().flat_map(move |foreign| {
                        foreign.columns.iter().zip(&foreign.ref_columns).map(
                            move |(column, reference)| {
                                let referenced = tables
                                    .iter()
                                    .find(|candidate| candidate.id == foreign.ref_table_id)
                                    .map(|candidate| candidate.name.as_str())
                                    .unwrap_or("");
                                vec![
                                    text(&foreign.name),
                                    text(&table.name),
                                    text(column),
                                    text("public"),
                                    text(referenced),
                                    text(reference),
                                    text("public"),
                                    text("a"),
                                    text("a"),
                                ]
                            },
                        )
                    })
                })
                .collect(),
            CatalogKind::Empty => Vec::new(),
        };
        Ok(CatalogResult {
            columns: self.columns.clone(),
            rows,
        })
    }
}

fn fields(columns: Vec<ResultColumn>, format: Option<&Format>) -> Vec<FieldInfo> {
    columns
        .into_iter()
        .enumerate()
        .map(|(index, column)| {
            FieldInfo::new(
                column.name,
                None,
                None,
                pg_type_with_format(column.scalar_type, &column.format),
                format.map_or(FieldFormat::Text, |format| format.format_for(index)),
            )
        })
        .collect()
}

fn query_response(
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<Datum>>,
    kind: CommandKind,
    format: &Format,
) -> PgWireResult<Response> {
    let fields = Arc::new(fields(columns, Some(format)));
    let mut encoder = DataRowEncoder::new(fields.clone());
    let mut encoded = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() != fields.len() {
            return Err(user_error(
                "XX000",
                format!(
                    "result row has {} values for {} fields",
                    row.len(),
                    fields.len()
                ),
            ));
        }
        for (datum, field) in row.iter().zip(fields.iter()) {
            encode_datum(&mut encoder, datum, field.datatype())?;
        }
        encoded.push(Ok(encoder.take_row()));
    }
    let mut response = QueryResponse::new(fields, stream::iter(encoded));
    response.set_command_tag(match kind {
        CommandKind::Insert => "INSERT 0",
        other => other.tag(),
    });
    Ok(Response::Query(response))
}

fn datum_rows(datum: Datum) -> PgWireResult<Vec<Vec<Datum>>> {
    match datum {
        Datum::Array(rows) => rows.into_iter().map(object_values).collect(),
        Datum::Object(fields) => Ok(vec![fields.into_iter().map(|field| field.datum).collect()]),
        Datum::Null => Ok(Vec::new()),
        Datum::Scalar(value) => Ok(vec![vec![Datum::scalar(value)]]),
    }
}

fn object_values(datum: Datum) -> PgWireResult<Vec<Datum>> {
    match datum {
        Datum::Object(fields) => Ok(fields.into_iter().map(|field| field.datum).collect()),
        other => Err(user_error(
            "XX000",
            format!("expected object row from Rad, got {other:?}"),
        )),
    }
}

fn encode_datum(encoder: &mut DataRowEncoder, datum: &Datum, pg_type: &Type) -> PgWireResult<()> {
    match datum {
        Datum::Null => match *pg_type {
            Type::BOOL => encoder.encode_field(&None::<bool>),
            Type::INT2 => encoder.encode_field(&None::<i16>),
            Type::INT4 => encoder.encode_field(&None::<i32>),
            Type::INT8 => encoder.encode_field(&None::<i64>),
            Type::TIMESTAMP => encoder.encode_field(&None::<chrono::NaiveDateTime>),
            Type::TIMESTAMPTZ => encoder.encode_field(&None::<chrono::DateTime<Utc>>),
            Type::FLOAT4 => encoder.encode_field(&None::<f32>),
            Type::FLOAT8 => encoder.encode_field(&None::<f64>),
            _ => encoder.encode_field(&None::<String>),
        },
        Datum::Scalar(Value::Text(value)) => encoder.encode_field(value),
        Datum::Scalar(Value::Int64(value)) => match *pg_type {
            Type::TIMESTAMP => encoder.encode_field(
                &Utc.timestamp_millis_opt(*value)
                    .single()
                    .ok_or_else(|| user_error("22008", "timestamp is outside PostgreSQL range"))?
                    .naive_utc(),
            ),
            Type::TIMESTAMPTZ => encoder.encode_field(
                &Utc.timestamp_millis_opt(*value)
                    .single()
                    .ok_or_else(|| user_error("22008", "timestamp is outside PostgreSQL range"))?,
            ),
            _ => encoder.encode_field(value),
        },
        Datum::Scalar(Value::Float64(value)) => encoder.encode_field(value),
        Datum::Scalar(Value::Bool(value)) => encoder.encode_field(value),
        Datum::Scalar(Value::Null(_)) => encode_datum(encoder, &Datum::Null, pg_type),
        other => Err(user_error(
            "0A000",
            format!("nested result values are not supported by pgwire: {other:?}"),
        )),
    }
}

fn decode_parameters(portal: &Portal<PgPrepared>) -> PgWireResult<Vec<Parameter>> {
    decode_parameters_inner(portal).map_err(|error| {
        let sql = portal
            .statement
            .statement
            .sql()
            .unwrap_or_else(|| "<catalog or transaction statement>".into());
        user_error(
            "22P02",
            format!(
                "{error}; expected {:?}; SQL: {sql}",
                portal.statement.statement.parameter_types()
            ),
        )
    })
}

fn decode_parameters_inner(portal: &Portal<PgPrepared>) -> PgWireResult<Vec<Parameter>> {
    let types = portal.statement.statement.parameter_types();
    if portal.parameter_len() != types.len() {
        return Err(user_error(
            "08P01",
            format!(
                "expected {} parameters, got {}",
                types.len(),
                portal.parameter_len()
            ),
        ));
    }
    types
        .into_iter()
        .enumerate()
        .map(|(index, scalar_type)| {
            let pg_type = portal
                .statement
                .parameter_types
                .get(index)
                .and_then(|value| value.as_ref())
                .cloned()
                .unwrap_or_else(|| pg_type(scalar_type));
            let value = match scalar_type {
                ScalarType::Text => portal
                    .parameter::<String>(index, &pg_type)?
                    .map(RawScalar::Text)
                    .unwrap_or(RawScalar::Null),
                ScalarType::Int64 => match pg_type {
                    Type::INT2 => portal.parameter::<i16>(index, &pg_type)?.map(i64::from),
                    Type::INT4 => portal.parameter::<i32>(index, &pg_type)?.map(i64::from),
                    Type::INT8 if portal.parameter_format.is_binary(index) => {
                        portal.parameter::<i64>(index, &pg_type)?
                    }
                    Type::INT8 => portal
                        .parameter::<String>(index, &Type::TEXT)?
                        .map(|value| parse_int64_parameter(&value))
                        .transpose()?,
                    Type::TIMESTAMP => portal
                        .parameter::<chrono::NaiveDateTime>(index, &pg_type)?
                        .map(|value| value.and_utc().timestamp_millis()),
                    Type::TIMESTAMPTZ => portal
                        .parameter::<chrono::DateTime<chrono::FixedOffset>>(index, &pg_type)?
                        .map(|value| value.timestamp_millis()),
                    _ => portal
                        .parameter::<String>(index, &pg_type)?
                        .map(|value| parse_int64_parameter(&value))
                        .transpose()?,
                }
                .map(|value| RawScalar::Number(value.to_string()))
                .unwrap_or(RawScalar::Null),
                ScalarType::Float64 => match pg_type {
                    Type::FLOAT4 => portal.parameter::<f32>(index, &pg_type)?.map(f64::from),
                    _ => portal.parameter::<f64>(index, &pg_type)?,
                }
                .map(|value| RawScalar::Number(value.to_string()))
                .unwrap_or(RawScalar::Null),
                ScalarType::Bool => portal
                    .parameter::<bool>(index, &pg_type)?
                    .map(RawScalar::Bool)
                    .unwrap_or(RawScalar::Null),
            };
            Ok(Parameter { scalar_type, value })
        })
        .collect()
}

fn parse_int64_parameter(value: &str) -> PgWireResult<i64> {
    if let Ok(value) = value.parse() {
        return Ok(value);
    }
    if let Ok(value) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(value.timestamp_millis());
    }
    let go_time = value
        .split_whitespace()
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");
    if let Ok(value) = chrono::DateTime::parse_from_str(&go_time, "%Y-%m-%d %H:%M:%S%.f %z") {
        return Ok(value.timestamp_millis());
    }
    if let Ok(value) = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f") {
        return Ok(value.and_utc().timestamp_millis());
    }
    Err(user_error(
        "22P02",
        format!("invalid int64 or timestamp parameter {value:?}"),
    ))
}

fn pg_type(value: ScalarType) -> Type {
    match value {
        ScalarType::Text => Type::TEXT,
        ScalarType::Int64 => Type::INT8,
        ScalarType::Float64 => Type::FLOAT8,
        ScalarType::Bool => Type::BOOL,
    }
}

fn pg_type_with_format(value: ScalarType, format: &str) -> Type {
    match format {
        "uuid" => Type::UUID,
        "json" => Type::JSON,
        "jsonb" => Type::JSONB,
        "bytea" => Type::BYTEA,
        "timestamp" => Type::TIMESTAMP,
        "timestamptz" => Type::TIMESTAMPTZ,
        _ => pg_type(value),
    }
}

fn scalar_from_pg(value: &Type) -> Option<ScalarType> {
    match *value {
        Type::BOOL => Some(ScalarType::Bool),
        Type::INT2 | Type::INT4 | Type::INT8 | Type::TIMESTAMP | Type::TIMESTAMPTZ => {
            Some(ScalarType::Int64)
        }
        Type::FLOAT4 | Type::FLOAT8 | Type::NUMERIC => Some(ScalarType::Float64),
        Type::TEXT
        | Type::VARCHAR
        | Type::BPCHAR
        | Type::NAME
        | Type::UUID
        | Type::JSON
        | Type::JSONB
        | Type::BYTEA => Some(ScalarType::Text),
        _ => None,
    }
}

fn sql_error(error: sql::Error) -> PgWireError {
    user_error("0A000", error.to_string())
}

fn engine_error(error: crate::engine::exec::Error) -> PgWireError {
    engine_error_message(error, None)
}

fn engine_error_with_sql(error: crate::engine::exec::Error, sql: &str) -> PgWireError {
    engine_error_message(error, Some(sql))
}

fn engine_error_message(error: crate::engine::exec::Error, sql: Option<&str>) -> PgWireError {
    let code = match error.reason() {
        ErrorReason::ReadOnly => "25006",
        ErrorReason::ConstraintViolation => "23505",
        ErrorReason::SerializableConflict => "40001",
        ErrorReason::UnknownTable | ErrorReason::UnknownColumn => "42P01",
        ErrorReason::TypeMismatch => "42804",
        ErrorReason::DivisionByZero => "22012",
        _ => "XX000",
    };
    let message = match sql {
        Some(sql) => format!("{error}; SQL: {sql}"),
        None => error.to_string(),
    };
    user_error(code, message)
}

fn api_error(error: impl std::error::Error + Send + Sync + 'static) -> PgWireError {
    PgWireError::ApiError(Box::new(error))
}

fn user_error(code: &str, message: impl Into<String>) -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".into(),
        code.into(),
        message.into(),
    )))
}

fn parameter_count(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let mut maximum = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'$' {
            let mut end = index + 1;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > index + 1
                && let Ok(value) = sql[index + 1..end].parse::<usize>()
            {
                maximum = maximum.max(value);
            }
            index = end;
        } else {
            index += 1;
        }
    }
    maximum
}

fn catalog_select_columns(sql: &str) -> Vec<ResultColumn> {
    let Ok(mut statements) = sql::parse(sql) else {
        return Vec::new();
    };
    let Some(sqlparser::ast::Statement::Query(query)) = statements.pop() else {
        return Vec::new();
    };
    let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() else {
        return Vec::new();
    };
    select
        .projection
        .iter()
        .map(|item| {
            let name = match item {
                sqlparser::ast::SelectItem::ExprWithAlias { alias, .. } => alias.value.clone(),
                sqlparser::ast::SelectItem::UnnamedExpr(expression) => expression.to_string(),
                _ => "column".into(),
            };
            text_column(&name, true)
        })
        .collect()
}

fn atlas_column_columns() -> Vec<ResultColumn> {
    [
        "table_name",
        "column_name",
        "data_type",
        "format_type",
        "is_nullable",
        "column_default",
        "character_maximum_length",
        "numeric_precision",
        "datetime_precision",
        "numeric_scale",
        "interval_type",
        "character_set_name",
        "collation_name",
        "is_identity",
        "identity_start",
        "identity_increment",
        "identity_last",
        "identity_generation",
        "generation_expression",
        "comment",
        "typtype",
        "typelem",
        "oid",
        "attnum",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, name)| {
        if matches!(index, 6..=9 | 14..=16 | 21..=23) {
            int_column(name, true)
        } else {
            text_column(name, true)
        }
    })
    .collect()
}

fn atlas_index_columns() -> Vec<ResultColumn> {
    [
        text_column("table_name", false),
        text_column("index_name", false),
        text_column("index_type", false),
        text_column("column_name", true),
        bool_column("included", false),
        bool_column("primary", false),
        bool_column("unique", false),
        text_column("excoper", true),
        text_column("constraints", true),
        text_column("predicate", true),
        text_column("expression", true),
        bool_column("isdesc", true),
        bool_column("nulls_first", true),
        bool_column("nulls_last", true),
        text_column("comment", true),
        text_column("options", true),
        text_column("opclass_name", true),
        text_column("opclass_schema", true),
        bool_column("opclass_default", true),
        text_column("opclass_params", true),
        bool_column("indnullsnotdistinct", false),
    ]
    .into()
}

fn atlas_foreign_key_columns() -> Vec<ResultColumn> {
    [
        "constraint_name",
        "table_name",
        "column_name",
        "schema_name",
        "referenced_table_name",
        "referenced_column_name",
        "referenced_schema_name",
        "confupdtype",
        "confdeltype",
    ]
    .into_iter()
    .map(|name| text_column(name, false))
    .collect()
}

fn text_column(name: &str, nullable: bool) -> ResultColumn {
    ResultColumn {
        name: name.into(),
        field: name.into(),
        scalar_type: ScalarType::Text,
        nullable,
        format: String::new(),
    }
}

fn int_column(name: &str, nullable: bool) -> ResultColumn {
    ResultColumn {
        name: name.into(),
        field: name.into(),
        scalar_type: ScalarType::Int64,
        nullable,
        format: String::new(),
    }
}

fn bool_column(name: &str, nullable: bool) -> ResultColumn {
    ResultColumn {
        name: name.into(),
        field: name.into(),
        scalar_type: ScalarType::Bool,
        nullable,
        format: String::new(),
    }
}

fn text(value: &str) -> Datum {
    Datum::Scalar(Value::Text(value.into()))
}

fn int(value: i64) -> Datum {
    Datum::Scalar(Value::Int64(value))
}

fn bool_value(value: bool) -> Datum {
    Datum::Scalar(Value::Bool(value))
}

fn catalog_type(scalar_type: ScalarType, format: &str) -> (&'static str, &'static str, i64) {
    match format {
        "text" => ("text", "text", 25),
        "uuid" => ("uuid", "uuid", 2950),
        "json" => ("json", "json", 114),
        "jsonb" => ("jsonb", "jsonb", 3802),
        "bytea" => ("bytea", "bytea", 17),
        "timestamp" => (
            "timestamp without time zone",
            "timestamp without time zone",
            1114,
        ),
        "timestamptz" => ("timestamp with time zone", "timestamp with time zone", 1184),
        _ => match scalar_type {
            ScalarType::Text => ("character varying", "character varying", 1043),
            ScalarType::Int64 => ("bigint", "bigint", 20),
            ScalarType::Float64 => ("double precision", "double precision", 701),
            ScalarType::Bool => ("boolean", "boolean", 16),
        },
    }
}

fn default_datum(
    default: Option<&crate::engine::catalog::model::DefaultValue>,
    scalar_type: ScalarType,
    _format: &str,
) -> Datum {
    let Some(default) = default else {
        return Datum::Null;
    };
    if let Some(function) = default.function {
        return text(match function {
            crate::engine::catalog::model::DefaultFunction::Uuid => "gen_random_uuid()",
            crate::engine::catalog::model::DefaultFunction::NowMs => "CURRENT_TIMESTAMP",
        });
    }
    match scalar_type {
        ScalarType::Text => text(&format!(
            "'{}'::character varying",
            default.text.replace('\'', "''")
        )),
        ScalarType::Int64 => text(&default.int64.to_string()),
        ScalarType::Float64 if default.float64.fract() == 0.0 => {
            text(&format!("{:.1}", default.float64))
        }
        ScalarType::Float64 => text(&default.float64.to_string()),
        ScalarType::Bool => text(if default.bool_value { "true" } else { "false" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::kv::TransactionalKv;
    use crate::engine::kv::slatedb::Store;

    #[test]
    fn recognizes_atlas_catalog_queries_without_a_generic_sql_lowering() {
        let tables = CatalogPlan::recognize(
            "SELECT t3.oid, t1.table_schema, t1.table_name FROM INFORMATION_SCHEMA.TABLES AS t1 JOIN pg_catalog.pg_class AS t3 ON true WHERE t1.table_schema IN ($1)",
        )
        .unwrap();
        assert!(matches!(tables.kind, CatalogKind::Tables));
        assert_eq!(tables.parameter_count, 1);
        assert_eq!(tables.columns[2].name, "table_name");
    }

    #[tokio::test]
    async fn read_only_errors_use_postgres_sqlstate() {
        let store = Arc::new(Store::memory("postgres-read-only").await.unwrap());
        let engine = Engine::read_only(store.clone());
        let error = engine_error(engine.require_write().unwrap_err());
        let PgWireError::UserError(error) = error else {
            panic!("expected user error");
        };
        assert_eq!(error.code, "25006");
        store.close().await.unwrap();
    }
}
