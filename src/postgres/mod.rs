//! PostgreSQL wire-protocol compatibility frontend.

use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

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
    ContextModifier, Expr as SqlExpr, Reset, Set as SqlSet, Statement as SqlStatement,
    TransactionAccessMode, TransactionIsolationLevel, TransactionMode, Value as SqlValue,
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
                transaction_admission: Arc::new(Mutex::new(())),
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
    transaction_admission: Arc<Mutex<()>>,
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
        let application_name = client
            .metadata()
            .get("application_name")
            .cloned()
            .unwrap_or_default();
        client.session_extensions().get_or_insert_with(|| {
            Mutex::new(Session {
                application_name,
                ..Session::default()
            })
        });
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
            statement.statement.result_fields(None),
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
        Ok(DescribePortalResponse::new(
            portal
                .statement
                .statement
                .result_fields(Some(&portal.result_column_format)),
        ))
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
                PgPrepared::Session(command) => self.execute_session(client, command, format).await,
                PgPrepared::Catalog(plan) => {
                    self.reject_failed_transaction(client).await?;
                    let tables = self.parser.tables(client).await?;
                    let result = plan.execute(&tables, parameters)?;
                    query_response(
                        result.columns,
                        result.rows,
                        CommandKind::Select,
                        format,
                        None,
                    )
                }
                PgPrepared::Sql { prepared, origins } => {
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
                    let application_name = state.application_name.clone();
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
                            application_name,
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
                        let effectful = program
                            .statements
                            .iter()
                            .any(|statement| statement.effectful());
                        let _transaction_admission = if effectful {
                            Some(self.transaction_admission.lock().await)
                        } else {
                            None
                        };
                        let current_span = tracing::Span::current();
                        let (trace_id, span_id) = crate::telemetry::span_ids(&current_span);
                        let context = crate::logging::RequestContext {
                            transport: "postgres",
                            request_id,
                            transaction_id: String::new(),
                            client_ip,
                            application_name,
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
                        Box::pin(crate::logging::with_request_context(context, async {
                            retry_implicit_transaction(|| {
                                crate::engine::frontend::execute_program_with_options(
                                    &self.engine,
                                    program.clone(),
                                    options.clone(),
                                )
                            })
                            .await
                        }))
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
                            Some(origins),
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
                {
                    let state = session.lock().await;
                    if state.transaction.is_some() {
                        return Ok(Response::TransactionStart(Tag::new("BEGIN")));
                    }
                }
                // Rad transactions use serializable snapshots. Admit explicit
                // transactions before opening the snapshot so ordinary PostgreSQL
                // clients wait instead of surfacing avoidable serialization errors.
                let admission = self.transaction_admission.clone().lock_owned().await;
                let mut state = session.lock().await;
                if state.transaction.is_none() {
                    state.transaction =
                        Some(Tx::begin(self.engine.clone()).await.map_err(engine_error)?);
                    state.transaction_admission = Some(admission);
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
                let (transaction, failed, transaction_admission) = {
                    let mut state = session.lock().await;
                    let failed =
                        state.failed || client.transaction_status() == TransactionStatus::Error;
                    state.failed = false;
                    (
                        state.transaction.take(),
                        failed,
                        state.transaction_admission.take(),
                    )
                };
                let _transaction_admission = transaction_admission;
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
                let (transaction, transaction_admission) = {
                    let mut state = session.lock().await;
                    state.failed = false;
                    (state.transaction.take(), state.transaction_admission.take())
                };
                let _transaction_admission = transaction_admission;
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

    async fn execute_session<C>(
        &self,
        client: &C,
        command: &SessionCommand,
        format: &Format,
    ) -> PgWireResult<Response>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        self.reject_failed_transaction(client).await?;
        match command {
            SessionCommand::SetApplicationName(value) => {
                let connection = session(client);
                connection.lock().await.application_name = value.clone();
                log_session_setting(client, "application_name", value, "accepted");
                Ok(Response::Execution(Tag::new("SET")))
            }
            SessionCommand::SetExtraFloatDigits => {
                log_session_setting(client, "extra_float_digits", "3", "ignored");
                Ok(Response::Execution(Tag::new("SET")))
            }
            SessionCommand::SetSearchPath => {
                log_session_setting(client, "search_path", "public", "ignored");
                Ok(Response::Execution(Tag::new("SET")))
            }
            SessionCommand::SetTransactionCharacteristics { read_only } => {
                match (*read_only, self.engine.is_read_only()) {
                    (Some(true), false) => {
                        return Err(user_error(
                            "0A000",
                            "per-session read-only transactions are not supported",
                        ));
                    }
                    (Some(false), true) => {
                        return Err(user_error("25006", "database is read-only"));
                    }
                    _ => {}
                }
                let value = match read_only {
                    Some(true) => "serializable, read only",
                    Some(false) => "serializable, read write",
                    None => "serializable",
                };
                log_session_setting(
                    client,
                    "default_transaction_characteristics",
                    value,
                    "accepted",
                );
                Ok(Response::Execution(Tag::new("SET")))
            }
            SessionCommand::Reset(setting) => {
                if setting.is_none_or(|setting| setting == SessionSetting::ApplicationName) {
                    session(client).lock().await.application_name.clear();
                }
                log_session_setting(
                    client,
                    setting.map_or("all", SessionSetting::name),
                    "default",
                    "accepted",
                );
                Ok(Response::Execution(Tag::new("RESET")))
            }
            SessionCommand::Show(setting) => {
                let value = match setting {
                    SessionSetting::ApplicationName => {
                        session(client).lock().await.application_name.clone()
                    }
                    SessionSetting::ExtraFloatDigits => "3".into(),
                    SessionSetting::SearchPath => "public".into(),
                    SessionSetting::TransactionIsolation => "serializable".into(),
                    SessionSetting::ServerVersionNum => "170000".into(),
                };
                query_response_with_tag(
                    vec![text_column(setting.name(), false)],
                    vec![vec![text(&value)]],
                    "SHOW",
                    format,
                )
            }
        }
    }
}

const IMPLICIT_TRANSACTION_ATTEMPTS: usize = 32;

async fn retry_implicit_transaction<T, F, Fut>(mut operation: F) -> crate::engine::exec::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = crate::engine::exec::Result<T>>,
{
    for attempt in 0..IMPLICIT_TRANSACTION_ATTEMPTS {
        match operation().await {
            Err(error)
                if error.reason() == ErrorReason::SerializableConflict
                    && attempt + 1 < IMPLICIT_TRANSACTION_ATTEMPTS =>
            {
                let delay_ms = 1_u64 << attempt.min(4);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            result => return result,
        }
    }
    unreachable!("the final implicit transaction attempt always returns")
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
        Ok(statement.result_fields(format))
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
        if let Some(command) = session_command(sql)? {
            return Ok(PgPrepared::Session(command));
        }
        if let Some(plan) = CatalogPlan::recognize(sql) {
            return Ok(PgPrepared::Catalog(plan));
        }
        let tables = self.tables(client).await?;
        let hints = types
            .iter()
            .map(|value| value.as_ref().and_then(scalar_from_pg))
            .collect::<Vec<_>>();
        let prepared = sql::prepare(sql, &tables, &hints).map_err(sql_error)?;
        let origins = result_origins(&prepared, &tables);
        Ok(PgPrepared::Sql {
            prepared: Box::new(prepared),
            origins,
        })
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
    Sql {
        prepared: Box<Prepared>,
        origins: Vec<Option<FieldOrigin>>,
    },
    Catalog(CatalogPlan),
    Control(TransactionControl),
    Session(SessionCommand),
}

impl PgPrepared {
    fn parameter_types(&self) -> Vec<ScalarType> {
        match self {
            Self::Sql { prepared, .. } => prepared.parameter_types().to_vec(),
            Self::Catalog(plan) => plan.parameter_types.clone(),
            Self::Control(_) | Self::Session(_) => Vec::new(),
        }
    }

    fn result_columns(&self) -> Vec<ResultColumn> {
        match self {
            Self::Sql { prepared, .. } => prepared.result_columns().to_vec(),
            Self::Catalog(plan) => plan.columns.clone(),
            Self::Session(command) => command.result_columns(),
            Self::Control(_) => Vec::new(),
        }
    }

    fn sql(&self) -> Option<String> {
        match self {
            Self::Sql { prepared, .. } => Some(prepared.sql()),
            Self::Catalog(_) | Self::Control(_) | Self::Session(_) => None,
        }
    }

    fn result_fields(&self, format: Option<&Format>) -> Vec<FieldInfo> {
        match self {
            Self::Sql { prepared, origins } => {
                fields(prepared.result_columns().to_vec(), format, Some(origins))
            }
            _ => fields(self.result_columns(), format, None),
        }
    }
}

#[derive(Clone, Copy)]
struct FieldOrigin {
    table_oid: i32,
    column_id: i16,
}

#[derive(Clone, Copy)]
enum TransactionControl {
    Begin,
    Commit,
    Rollback,
}

#[derive(Clone, Debug)]
enum SessionCommand {
    SetApplicationName(String),
    SetExtraFloatDigits,
    SetSearchPath,
    SetTransactionCharacteristics { read_only: Option<bool> },
    Reset(Option<SessionSetting>),
    Show(SessionSetting),
}

impl SessionCommand {
    fn result_columns(&self) -> Vec<ResultColumn> {
        match self {
            Self::Show(setting) => vec![text_column(setting.name(), false)],
            _ => Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionSetting {
    ApplicationName,
    ExtraFloatDigits,
    SearchPath,
    TransactionIsolation,
    ServerVersionNum,
}

impl SessionSetting {
    fn name(self) -> &'static str {
        match self {
            Self::ApplicationName => "application_name",
            Self::ExtraFloatDigits => "extra_float_digits",
            Self::SearchPath => "search_path",
            Self::TransactionIsolation => "transaction_isolation",
            Self::ServerVersionNum => "server_version_num",
        }
    }
}

#[derive(Default)]
struct Session {
    transaction: Option<Tx>,
    transaction_admission: Option<tokio::sync::OwnedMutexGuard<()>>,
    failed: bool,
    application_name: String,
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

fn session_command(sql: &str) -> PgWireResult<Option<SessionCommand>> {
    let mut statements = sql::parse(sql).map_err(sql_error)?;
    if statements.len() != 1 {
        return Ok(None);
    }
    let command = match statements.remove(0) {
        SqlStatement::Set(SqlSet::SingleAssignment {
            scope,
            hivevar: false,
            variable,
            values,
        }) => {
            if matches!(
                scope,
                Some(ContextModifier::Local | ContextModifier::Global)
            ) {
                return Err(user_error("0A000", "only session-scoped SET is supported"));
            }
            let name = variable.to_string().to_ascii_lowercase();
            match name.as_str() {
                "application_name" => {
                    let [value] = values.as_slice() else {
                        return Err(unsupported_setting(&name));
                    };
                    let Some(value) = sql_string(value) else {
                        return Err(unsupported_setting(&name));
                    };
                    SessionCommand::SetApplicationName(value)
                }
                "extra_float_digits" if sql_number(&values) == Some("3") => {
                    SessionCommand::SetExtraFloatDigits
                }
                "search_path" if sql_identifier(&values).is_some_and(is_public_search_path) => {
                    SessionCommand::SetSearchPath
                }
                _ => return Err(unsupported_setting(&name)),
            }
        }
        SqlStatement::Set(SqlSet::SetTransaction {
            modes,
            snapshot: None,
            session: true,
        }) => {
            let mut read_only = None;
            for mode in modes {
                match mode {
                    TransactionMode::IsolationLevel(TransactionIsolationLevel::Serializable) => {}
                    TransactionMode::AccessMode(TransactionAccessMode::ReadOnly) => {
                        read_only = Some(true);
                    }
                    TransactionMode::AccessMode(TransactionAccessMode::ReadWrite) => {
                        read_only = Some(false);
                    }
                    _ => {
                        return Err(user_error(
                            "0A000",
                            "Rad supports serializable transaction characteristics matching the server role",
                        ));
                    }
                }
            }
            SessionCommand::SetTransactionCharacteristics { read_only }
        }
        SqlStatement::Set(_) => {
            return Err(user_error(
                "0A000",
                "session command is not supported by the PostgreSQL frontend yet",
            ));
        }
        SqlStatement::Reset(statement) => match statement.reset {
            Reset::ALL => SessionCommand::Reset(None),
            Reset::ConfigurationParameter(name) => {
                let name = name.to_string().to_ascii_lowercase();
                let Some(setting) = resettable_setting(&name) else {
                    return Err(unsupported_setting(&name));
                };
                SessionCommand::Reset(Some(setting))
            }
        },
        SqlStatement::ShowVariable { variable } => {
            let name = variable
                .iter()
                .map(|part| part.value.to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join("_");
            let Some(setting) = showable_setting(&name) else {
                return Err(unsupported_setting(&name));
            };
            SessionCommand::Show(setting)
        }
        _ => return Ok(None),
    };
    Ok(Some(command))
}

fn sql_string(expression: &SqlExpr) -> Option<String> {
    match expression {
        SqlExpr::Value(value) => value.value.clone().into_string(),
        _ => None,
    }
}

fn sql_number(values: &[SqlExpr]) -> Option<&str> {
    let [SqlExpr::Value(value)] = values else {
        return None;
    };
    match &value.value {
        SqlValue::Number(value, _) => Some(value.as_str()),
        _ => None,
    }
}

fn sql_identifier(values: &[SqlExpr]) -> Option<&str> {
    let [SqlExpr::Identifier(value)] = values else {
        return None;
    };
    Some(&value.value)
}

fn is_public_search_path(value: &str) -> bool {
    value.eq_ignore_ascii_case("public") || value.eq_ignore_ascii_case("default")
}

fn resettable_setting(name: &str) -> Option<SessionSetting> {
    match name {
        "application_name" => Some(SessionSetting::ApplicationName),
        "extra_float_digits" => Some(SessionSetting::ExtraFloatDigits),
        "search_path" => Some(SessionSetting::SearchPath),
        _ => None,
    }
}

fn showable_setting(name: &str) -> Option<SessionSetting> {
    if name == "server_version_num" {
        return Some(SessionSetting::ServerVersionNum);
    }
    resettable_setting(name).or_else(|| {
        matches!(
            name,
            "transaction_isolation" | "transaction_isolation_level"
        )
        .then_some(SessionSetting::TransactionIsolation)
    })
}

fn unsupported_setting(name: &str) -> PgWireError {
    user_error(
        "0A000",
        format!("session setting {name:?} is not supported by the PostgreSQL frontend yet"),
    )
}

fn log_session_setting<C: ClientInfo>(
    client: &C,
    setting: &str,
    value: &str,
    outcome: &'static str,
) {
    tracing::debug!(
        target: "rad",
        event = if outcome == "ignored" {
            "postgres.session_setting_ignored"
        } else {
            "postgres.session_setting_accepted"
        },
        component = "postgres",
        client_ip = %client.socket_addr().ip(),
        setting,
        value,
        outcome,
        message = "PostgreSQL session setting handled"
    );
}

#[derive(Clone)]
struct CatalogPlan {
    kind: CatalogKind,
    parameter_count: usize,
    parameter_types: Vec<ScalarType>,
    columns: Vec<ResultColumn>,
    field_metadata: Vec<(i64, i64)>,
}

#[derive(Clone, Copy)]
enum CatalogKind {
    Version,
    VersionText,
    Settings,
    SessionIdentity,
    CurrentDatabase,
    DbeaverDatabases,
    Schemas,
    DbeaverSchemas,
    DbeaverTables,
    DbeaverColumns,
    DbeaverConstraints,
    DbeaverTypes,
    DbeaverType,
    PgJdbcFieldMetadata,
    TableWithoutColumn,
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
        let field_metadata = if is_pgjdbc_field_metadata_query(&normalized) {
            pgjdbc_field_metadata_pairs(&normalized)
        } else {
            Vec::new()
        };
        let (kind, columns) = if !field_metadata.is_empty() {
            (
                CatalogKind::PgJdbcFieldMetadata,
                pgjdbc_field_metadata_columns(),
            )
        } else if normalized.starts_with("show server_version_num") {
            (
                CatalogKind::Version,
                vec![text_column("server_version_num", false)],
            )
        } else if normalized.starts_with("select version()") {
            (
                CatalogKind::VersionText,
                vec![text_column("version", false)],
            )
        } else if normalized.starts_with("select current_schema(),session_user")
            || normalized.starts_with("select current_schema(), session_user")
        {
            (
                CatalogKind::SessionIdentity,
                vec![
                    text_column("current_schema", false),
                    text_column("session_user", false),
                ],
            )
        } else if normalized.starts_with("select current_database()") {
            (
                CatalogKind::CurrentDatabase,
                vec![text_column("current_database", false)],
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
        } else if normalized.contains("from pg_catalog.pg_database db")
            && normalized.contains("db.oid,db.*")
        {
            (CatalogKind::DbeaverDatabases, dbeaver_database_columns())
        } else if normalized.contains("from pg_catalog.pg_namespace n")
            && normalized.contains("n.oid,n.*")
        {
            (CatalogKind::DbeaverSchemas, dbeaver_schema_columns())
        } else if normalized.contains("from pg_catalog.pg_class c")
            && normalized.contains("c.oid,c.*")
            && normalized.contains("c.relnamespace")
        {
            (CatalogKind::DbeaverTables, dbeaver_table_columns())
        } else if normalized.contains("from pg_catalog.pg_attribute a")
            && normalized.contains("a.attrelid")
        {
            (CatalogKind::DbeaverColumns, dbeaver_column_columns())
        } else if normalized.contains("from pg_catalog.pg_constraint c")
            && normalized.contains("tabrelname")
        {
            (
                CatalogKind::DbeaverConstraints,
                dbeaver_constraint_columns(),
            )
        } else if normalized.contains("from pg_catalog.pg_type t")
            && normalized.contains("where t.oid")
        {
            (CatalogKind::DbeaverType, dbeaver_type_columns())
        } else if normalized.contains("from pg_catalog.pg_type t") {
            (CatalogKind::DbeaverTypes, dbeaver_type_columns())
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
        } else if normalized.starts_with("select exists")
            && normalized.contains("information_schema.tables")
            && normalized.contains("and not exists")
            && normalized.contains("information_schema.columns")
        {
            (
                CatalogKind::TableWithoutColumn,
                vec![bool_column("?column?", false)],
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
        let parameter_types = match kind {
            CatalogKind::DbeaverTables if parameter_count > 0 => std::iter::once(ScalarType::Int64)
                .chain(std::iter::repeat_n(ScalarType::Text, parameter_count - 1))
                .collect(),
            CatalogKind::DbeaverColumns
            | CatalogKind::DbeaverConstraints
            | CatalogKind::DbeaverTypes
            | CatalogKind::DbeaverType => {
                vec![ScalarType::Int64; parameter_count]
            }
            _ => vec![ScalarType::Text; parameter_count],
        };
        Some(Self {
            kind,
            parameter_count,
            parameter_types,
            columns,
            field_metadata,
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
            CatalogKind::VersionText => vec![vec![text("PostgreSQL 16.6 compatible Rad frontend")]],
            CatalogKind::Settings => vec![vec![text("170000"), text("heap"), Datum::Null]],
            CatalogKind::SessionIdentity => vec![vec![text("public"), text("default")]],
            CatalogKind::CurrentDatabase => vec![vec![text("default")]],
            CatalogKind::DbeaverDatabases => vec![vec![
                int(1),
                text("default"),
                int(0),
                int(6),
                text("C"),
                text("C"),
                bool_value(false),
                bool_value(true),
                int(-1),
                int(0),
            ]],
            CatalogKind::Schemas => vec![vec![text("public"), Datum::Null]],
            CatalogKind::DbeaverSchemas => vec![vec![
                int(2_200),
                text("public"),
                int(0),
                Datum::Null,
                Datum::Null,
            ]],
            CatalogKind::DbeaverTables => tables
                .iter()
                .enumerate()
                .map(|(index, table)| {
                    vec![
                        int(dbeaver_table_oid(index)),
                        text(&table.name),
                        int(2_200),
                        int(0),
                        Datum::Null,
                        text("r"),
                        bool_value(false),
                        Datum::Null,
                        Datum::Null,
                        text("p"),
                        bool_value(false),
                        int(0),
                        bool_value(false),
                        Datum::Null,
                        Datum::Null,
                        bool_value(false),
                    ]
                })
                .collect(),
            CatalogKind::DbeaverColumns => {
                let requested_oid = parameter_int64(parameters, 0);
                tables
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| {
                        requested_oid.is_none_or(|oid| oid == dbeaver_table_oid(*index))
                    })
                    .flat_map(|(_, table)| {
                        table
                            .columns
                            .iter()
                            .enumerate()
                            .map(move |(position, column)| {
                                let (_, _, oid) = catalog_type(column.scalar_type, &column.format);
                                vec![
                                    text(&table.name),
                                    text(&column.name),
                                    int((position + 1) as i64),
                                    bool_value(!column.nullable),
                                    int(oid),
                                    default_datum(
                                        column.insert_default.as_ref(),
                                        column.scalar_type,
                                        &column.format,
                                    ),
                                    Datum::Null,
                                    int(-1),
                                    int(0),
                                    int(0),
                                    bool_value(true),
                                    text("p"),
                                    text(""),
                                    int(0),
                                    Datum::Null,
                                    text(""),
                                    Datum::Null,
                                ]
                            })
                    })
                    .collect()
            }
            CatalogKind::DbeaverConstraints => {
                let requested_oid = parameter_int64(parameters, 0);
                tables
                    .iter()
                    .enumerate()
                    .filter(|(index, table)| {
                        !table.primary_key.is_empty()
                            && requested_oid
                                .is_none_or(|oid| oid == 2_200 || oid == dbeaver_table_oid(*index))
                    })
                    .map(|(index, table)| {
                        let key = table
                            .primary_key
                            .iter()
                            .filter_map(|name| {
                                table.columns.iter().position(|column| column.name == *name)
                            })
                            .map(|position| Datum::Scalar(Value::Int64((position + 1) as i64)))
                            .collect();
                        vec![
                            int(32_768 + index as i64),
                            text(&format!("{}_pkey", table.name)),
                            text(&table.name),
                            int(0),
                            Datum::Null,
                            text("p"),
                            Datum::Array(key),
                            int(32_768 + index as i64),
                            bool_value(true),
                            bool_value(false),
                            bool_value(false),
                            Datum::Null,
                            Datum::Null,
                        ]
                    })
                    .collect()
            }
            CatalogKind::DbeaverTypes => dbeaver_type_rows(),
            CatalogKind::DbeaverType => {
                let requested_oid = parameter_int64(parameters, 0);
                dbeaver_type_rows()
                    .into_iter()
                    .filter(|row| {
                        requested_oid.is_none_or(|requested| {
                            matches!(row.first(), Some(Datum::Scalar(Value::Int64(oid))) if *oid == requested)
                        })
                    })
                    .collect()
            }
            CatalogKind::PgJdbcFieldMetadata => self
                .field_metadata
                .iter()
                .filter_map(|(oid, attribute_number)| {
                    let table_index = usize::try_from(*oid - 16_384).ok()?;
                    let table = tables.get(table_index)?;
                    let column_index = usize::try_from(*attribute_number - 1).ok()?;
                    let column = table.columns.get(column_index)?;
                    Some(vec![
                        int(*oid),
                        int(*attribute_number),
                        text(&column.name),
                        text(&table.name),
                        text("public"),
                        bool_value(!column.nullable),
                        bool_value(false),
                    ])
                })
                .collect(),
            CatalogKind::TableWithoutColumn => {
                let table_name = parameter_text(parameters, 0);
                let column_name = parameter_text(parameters, 1);
                let required = table_name.is_some_and(|table_name| {
                    tables
                        .iter()
                        .find(|table| table.name == table_name)
                        .is_some_and(|table| {
                            column_name
                                .is_none_or(|column_name| table.column(column_name).is_none())
                        })
                });
                vec![vec![Datum::Scalar(Value::Bool(required))]]
            }
            CatalogKind::TableCount => {
                let requested = parameters
                    .len()
                    .checked_sub(1)
                    .and_then(|index| parameter_text(parameters, index));
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

fn parameter_text(parameters: &[Parameter], index: usize) -> Option<&str> {
    parameters
        .get(index)
        .and_then(|parameter| match &parameter.value {
            RawScalar::Text(value) => Some(value.as_str()),
            _ => None,
        })
}

fn parameter_int64(parameters: &[Parameter], index: usize) -> Option<i64> {
    parameters
        .get(index)
        .and_then(|parameter| match &parameter.value {
            RawScalar::Number(value) | RawScalar::Text(value) => value.parse().ok(),
            _ => None,
        })
}

fn result_origins(prepared: &Prepared, tables: &[Table]) -> Vec<Option<FieldOrigin>> {
    let unknown = || vec![None; prepared.result_columns().len()];
    let Ok(mut statements) = sql::parse(&prepared.sql()) else {
        return unknown();
    };
    let Some(SqlStatement::Query(query)) = statements.pop() else {
        return unknown();
    };
    let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() else {
        return unknown();
    };
    let [source] = select.from.as_slice() else {
        return unknown();
    };
    if !source.joins.is_empty() {
        return unknown();
    }
    let sqlparser::ast::TableFactor::Table { name, .. } = &source.relation else {
        return unknown();
    };
    let Some(table_name) = name.0.last().and_then(|part| part.as_ident()) else {
        return unknown();
    };
    let Some((table_index, table)) = tables
        .iter()
        .enumerate()
        .find(|(_, table)| table.name == table_name.value)
    else {
        return unknown();
    };
    let Ok(table_oid) = i32::try_from(dbeaver_table_oid(table_index)) else {
        return unknown();
    };
    prepared
        .result_columns()
        .iter()
        .map(|result| {
            table
                .columns
                .iter()
                .position(|column| column.name == result.field)
                .and_then(|position| i16::try_from(position + 1).ok())
                .map(|column_id| FieldOrigin {
                    table_oid,
                    column_id,
                })
        })
        .collect()
}

fn fields(
    columns: Vec<ResultColumn>,
    format: Option<&Format>,
    origins: Option<&[Option<FieldOrigin>]>,
) -> Vec<FieldInfo> {
    columns
        .into_iter()
        .enumerate()
        .map(|(index, column)| {
            let origin = origins
                .and_then(|origins| origins.get(index))
                .copied()
                .flatten();
            FieldInfo::new(
                column.name,
                origin.map(|origin| origin.table_oid),
                origin.map(|origin| origin.column_id),
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
    origins: Option<&[Option<FieldOrigin>]>,
) -> PgWireResult<Response> {
    let tag = match kind {
        CommandKind::Insert => "INSERT 0",
        other => other.tag(),
    };
    query_response_with_tag_and_origins(columns, rows, tag, format, origins)
}

fn query_response_with_tag(
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<Datum>>,
    tag: &str,
    format: &Format,
) -> PgWireResult<Response> {
    query_response_with_tag_and_origins(columns, rows, tag, format, None)
}

fn query_response_with_tag_and_origins(
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<Datum>>,
    tag: &str,
    format: &Format,
    origins: Option<&[Option<FieldOrigin>]>,
) -> PgWireResult<Response> {
    let fields = Arc::new(fields(columns, Some(format), origins));
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
    response.set_command_tag(tag);
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
        Datum::Array(values) if *pg_type == Type::INT2_ARRAY => {
            let values = values
                .iter()
                .map(|value| match value {
                    Datum::Scalar(Value::Int64(value)) => i16::try_from(*value).map_err(|_| {
                        user_error("22003", "smallint array value is outside PostgreSQL range")
                    }),
                    _ => Err(user_error("42804", "expected a smallint array value")),
                })
                .collect::<PgWireResult<Vec<_>>>()?;
            encoder.encode_field(&values)
        }
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
        "int2_array" => Type::INT2_ARRAY,
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

fn dbeaver_table_oid(index: usize) -> i64 {
    16_384 + index as i64
}

fn is_pgjdbc_field_metadata_query(sql: &str) -> bool {
    sql.contains("from pg_catalog.pg_class c")
        && sql.contains("join pg_catalog.pg_namespace n")
        && sql.contains("join pg_catalog.pg_attribute a")
        && sql.contains(") vals on")
}

fn pgjdbc_field_metadata_pairs(sql: &str) -> Vec<(i64, i64)> {
    let Some(values) = sql
        .split_once(" join (")
        .and_then(|(_, tail)| tail.split_once(") vals on"))
        .map(|(values, _)| values)
    else {
        return Vec::new();
    };
    values
        .split(" union all ")
        .filter_map(|row| {
            let row = row.strip_prefix("select ")?;
            let (oid, attribute_number) = row.split_once(',')?;
            Some((
                oid.split_whitespace().next()?.parse().ok()?,
                attribute_number.split_whitespace().next()?.parse().ok()?,
            ))
        })
        .collect()
}

fn pgjdbc_field_metadata_columns() -> Vec<ResultColumn> {
    vec![
        int_column("oid", false),
        int_column("attnum", false),
        text_column("attname", false),
        text_column("relname", false),
        text_column("nspname", false),
        bool_column("attnotnull", false),
        bool_column("auto_increment", false),
    ]
}

fn dbeaver_database_columns() -> Vec<ResultColumn> {
    vec![
        int_column("oid", false),
        text_column("datname", false),
        int_column("datdba", false),
        int_column("encoding", false),
        text_column("datcollate", false),
        text_column("datctype", false),
        bool_column("datistemplate", false),
        bool_column("datallowconn", false),
        int_column("datconnlimit", false),
        int_column("dattablespace", false),
    ]
}

fn dbeaver_schema_columns() -> Vec<ResultColumn> {
    vec![
        int_column("oid", false),
        text_column("nspname", false),
        int_column("nspowner", false),
        text_column("description", true),
        text_column("nspacl", true),
    ]
}

fn dbeaver_table_columns() -> Vec<ResultColumn> {
    vec![
        int_column("oid", false),
        text_column("relname", false),
        int_column("relnamespace", false),
        int_column("relowner", false),
        text_column("description", true),
        text_column("relkind", false),
        bool_column("relispartition", false),
        text_column("relacl", true),
        text_column("reloptions", true),
        text_column("relpersistence", false),
        bool_column("relhasoids", false),
        int_column("reltablespace", false),
        bool_column("relhassubclass", false),
        text_column("partition_expr", true),
        text_column("partition_key", true),
        bool_column("relrowsecurity", false),
    ]
}

fn dbeaver_column_columns() -> Vec<ResultColumn> {
    vec![
        text_column("relname", false),
        text_column("attname", false),
        int_column("attnum", false),
        bool_column("attnotnull", false),
        int_column("atttypid", false),
        text_column("def_value", true),
        text_column("description", true),
        int_column("atttypmod", false),
        int_column("attndims", false),
        int_column("attinhcount", false),
        bool_column("attislocal", false),
        text_column("attstorage", false),
        text_column("attidentity", false),
        int_column("attcollation", false),
        text_column("attacl", true),
        text_column("attgenerated", false),
        int_column("objid", true),
    ]
}

fn dbeaver_constraint_columns() -> Vec<ResultColumn> {
    vec![
        int_column("oid", false),
        text_column("conname", false),
        text_column("tabrelname", false),
        int_column("refnamespace", false),
        text_column("description", true),
        text_column("contype", false),
        ResultColumn {
            name: "conkey".into(),
            field: "conkey".into(),
            scalar_type: ScalarType::Text,
            nullable: false,
            format: "int2_array".into(),
        },
        int_column("conindid", false),
        bool_column("conislocal", false),
        bool_column("condeferrable", false),
        bool_column("condeferred", false),
        text_column("consrc_copy", true),
        text_column("consrc", true),
    ]
}

fn dbeaver_type_columns() -> Vec<ResultColumn> {
    vec![
        int_column("oid", false),
        text_column("typname", false),
        int_column("typnamespace", false),
        int_column("typowner", false),
        int_column("typlen", false),
        bool_column("typbyval", false),
        text_column("typtype", false),
        text_column("typcategory", false),
        bool_column("typispreferred", false),
        text_column("typdelim", false),
        int_column("typrelid", false),
        int_column("typelem", false),
        int_column("typarray", false),
        text_column("typinput", false),
        text_column("typoutput", false),
        text_column("typreceive", false),
        text_column("typsend", false),
        text_column("typmodin", false),
        text_column("typmodout", false),
        text_column("typanalyze", false),
        text_column("typalign", false),
        text_column("typstorage", false),
        bool_column("typnotnull", false),
        int_column("typbasetype", false),
        int_column("typtypmod", false),
        int_column("typndims", false),
        int_column("typcollation", false),
        text_column("typdefault", true),
        text_column("typacl", true),
        text_column("relkind", true),
        text_column("base_type_name", true),
        text_column("description", true),
    ]
}

fn dbeaver_type_rows() -> Vec<Vec<Datum>> {
    [
        (16, "bool", 1, true, "B", "bool"),
        (17, "bytea", -1, false, "U", "bytea"),
        (20, "int8", 8, true, "N", "int8"),
        (21, "int2", 2, true, "N", "int2"),
        (23, "int4", 4, true, "N", "int4"),
        (25, "text", -1, false, "S", "text"),
        (114, "json", -1, false, "U", "json"),
        (701, "float8", 8, true, "N", "float8"),
        (1_005, "_int2", -1, false, "A", "array"),
        (1_043, "varchar", -1, false, "S", "varchar"),
        (1_114, "timestamp", 8, true, "D", "timestamp"),
        (1_184, "timestamptz", 8, true, "D", "timestamptz"),
        (2_950, "uuid", 16, false, "U", "uuid"),
        (3_802, "jsonb", -1, false, "U", "jsonb"),
    ]
    .into_iter()
    .map(|(oid, name, length, by_value, category, function_prefix)| {
        let element = if oid == 1_005 { 21 } else { 0 };
        vec![
            int(oid),
            text(name),
            int(2_200),
            int(0),
            int(length),
            bool_value(by_value),
            text("b"),
            text(category),
            bool_value(false),
            text(","),
            int(0),
            int(element),
            int(0),
            text(&format!("{function_prefix}in")),
            text(&format!("{function_prefix}out")),
            text(&format!("{function_prefix}recv")),
            text(&format!("{function_prefix}send")),
            text("-"),
            text("-"),
            text("-"),
            text(if length >= 8 { "d" } else { "i" }),
            text(if length < 0 { "x" } else { "p" }),
            bool_value(false),
            int(0),
            int(-1),
            int(0),
            int(0),
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
            Datum::Null,
        ]
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
    use crate::engine::catalog::identity::{
        DefinitionGeneration, ExistenceGeneration, SchemaId, StorageGeneration, ValueGeneration,
        WriteProtocolGeneration,
    };
    use crate::engine::catalog::model::Column;
    use crate::engine::kv::TransactionalKv;
    use crate::engine::kv::slatedb::Store;

    fn table_with_columns(name: &str, columns: &[&str]) -> Table {
        Table {
            id: format!("{name}-table").into(),
            schema_id: SchemaId::new(1).unwrap(),
            name: name.into(),
            definition_generation: DefinitionGeneration::ZERO,
            existence_generation: ExistenceGeneration::from(1),
            write_protocol_generation: WriteProtocolGeneration::from(1),
            storage_generation: StorageGeneration::INITIAL,
            columns: columns
                .iter()
                .enumerate()
                .map(|(index, name)| Column {
                    id: format!("column-{index}").into(),
                    schema_id: SchemaId::new(1).unwrap(),
                    name: (*name).into(),
                    value_generation: ValueGeneration::from(1),
                    scalar_type: ScalarType::Int64,
                    nullable: false,
                    format: String::new(),
                    insert_default: None,
                    missing_value: None,
                })
                .collect(),
            primary_key: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

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

    #[test]
    fn exposes_the_minimum_dbeaver_navigation_catalog() {
        let schemas = CatalogPlan::recognize(
            "SELECT n.oid,n.*,d.description FROM pg_catalog.pg_namespace n LEFT OUTER JOIN pg_catalog.pg_description d ON d.objoid=n.oid ORDER BY nspname",
        )
        .unwrap()
        .execute(&[], &[])
        .unwrap();
        assert_eq!(schemas.columns[1].name, "nspname");
        assert_eq!(
            schemas.rows,
            vec![vec![
                int(2_200),
                text("public"),
                int(0),
                Datum::Null,
                Datum::Null,
            ]]
        );

        let mut table = table_with_columns("people", &["id", "name"]);
        table.primary_key = vec!["id".into()];
        let prepared = sql::prepare("SELECT id, name FROM people", &[table.clone()], &[]).unwrap();
        let origins = result_origins(&prepared, std::slice::from_ref(&table));
        assert_eq!(origins.len(), 2);
        assert_eq!(origins[0].unwrap().table_oid, 16_384);
        assert_eq!(origins[0].unwrap().column_id, 1);
        assert_eq!(origins[1].unwrap().column_id, 2);

        let jdbc_metadata = CatalogPlan::recognize(
            "SELECT c.oid, a.attnum, a.attname, c.relname, n.nspname, a.attnotnull, false FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON (c.relnamespace = n.oid) JOIN pg_catalog.pg_attribute a ON (c.oid = a.attrelid) JOIN (SELECT 16384 AS oid, 1 AS attnum UNION ALL SELECT 16384, 2) vals ON (c.oid = vals.oid AND a.attnum = vals.attnum)",
        )
        .unwrap()
        .execute(std::slice::from_ref(&table), &[])
        .unwrap();
        assert_eq!(jdbc_metadata.rows.len(), 2);
        assert_eq!(jdbc_metadata.rows[0][2], text("id"));
        assert_eq!(jdbc_metadata.rows[1][2], text("name"));
        assert_eq!(jdbc_metadata.rows[0][3], text("people"));
        assert!(
            CatalogPlan::recognize(
                "SELECT vals.oid FROM people JOIN (SELECT 16384 AS oid, 1 AS attnum) vals ON true"
            )
            .is_none()
        );

        let table_plan = CatalogPlan::recognize(
            "SELECT c.oid,c.*,d.description FROM pg_catalog.pg_class c WHERE c.relnamespace=$1 AND c.relkind not in ('i','I','c')",
        )
        .unwrap();
        assert_eq!(table_plan.parameter_types, vec![ScalarType::Int64]);
        let tables = table_plan
            .execute(
                std::slice::from_ref(&table),
                &[Parameter {
                    scalar_type: ScalarType::Int64,
                    value: RawScalar::Number("2200".into()),
                }],
            )
            .unwrap();
        assert_eq!(tables.columns[1].name, "relname");
        assert_eq!(tables.rows[0][0], int(16_384));
        assert_eq!(tables.rows[0][1], text("people"));
        let table_lookup = CatalogPlan::recognize(
            "SELECT c.oid,c.*,d.description FROM pg_catalog.pg_class c WHERE c.relnamespace=$1 AND relname=$2",
        )
        .unwrap();
        assert_eq!(
            table_lookup.parameter_types,
            vec![ScalarType::Int64, ScalarType::Text]
        );

        let columns = CatalogPlan::recognize(
            "SELECT c.relname,a.*,pg_catalog.pg_get_expr(ad.adbin, ad.adrelid, true) as def_value,dsc.description FROM pg_catalog.pg_attribute a INNER JOIN pg_catalog.pg_class c ON (a.attrelid=c.oid) WHERE c.oid=$1",
        )
        .unwrap()
        .execute(
            std::slice::from_ref(&table),
            &[Parameter {
                scalar_type: ScalarType::Int64,
                value: RawScalar::Number("16384".into()),
            }],
        )
        .unwrap();
        assert_eq!(columns.columns[1].name, "attname");
        assert_eq!(columns.rows.len(), 2);
        assert_eq!(columns.rows[1][1], text("name"));

        let types = CatalogPlan::recognize(
            "SELECT t.oid,t.*,c.relkind,format_type(nullif(t.typbasetype, 0), t.typtypmod) as base_type_name FROM pg_catalog.pg_type t WHERE t.typnamespace=$1",
        )
        .unwrap()
        .execute(
            &[],
            &[Parameter {
                scalar_type: ScalarType::Int64,
                value: RawScalar::Number("2200".into()),
            }],
        )
        .unwrap();
        assert!(types.rows.iter().any(|row| row[1] == text("int8")));

        let int8 = CatalogPlan::recognize(
            "SELECT t.oid,t.*,c.relkind,format_type(nullif(t.typbasetype, 0), t.typtypmod) as base_type_name FROM pg_catalog.pg_type t WHERE t.oid=$1",
        )
        .unwrap()
        .execute(
            &[],
            &[Parameter {
                scalar_type: ScalarType::Int64,
                value: RawScalar::Number("20".into()),
            }],
        )
        .unwrap();
        assert_eq!(int8.rows.len(), 1);
        assert_eq!(int8.rows[0][1], text("int8"));

        let constraints = CatalogPlan::recognize(
            "SELECT c.oid,c.*,t.relname as tabrelname,rt.relnamespace as refnamespace,d.description FROM pg_catalog.pg_constraint c INNER JOIN pg_catalog.pg_class t ON t.oid=c.conrelid WHERE c.conrelid=$1",
        )
        .unwrap()
        .execute(
            &[table],
            &[Parameter {
                scalar_type: ScalarType::Int64,
                value: RawScalar::Number("16384".into()),
            }],
        )
        .unwrap();
        assert_eq!(constraints.columns[6].format, "int2_array");
        assert_eq!(constraints.rows[0][1], text("people_pkey"));
        assert_eq!(
            constraints.rows[0][6],
            Datum::Array(vec![Datum::Scalar(Value::Int64(1))])
        );
    }

    #[test]
    fn accepts_the_jdbc_session_settings() {
        assert!(matches!(
            session_command("SET extra_float_digits = 3").unwrap(),
            Some(SessionCommand::SetExtraFloatDigits)
        ));
        assert!(matches!(
            session_command("SET application_name = 'DBeaver 25.3.1 - Main'").unwrap(),
            Some(SessionCommand::SetApplicationName(value)) if value == "DBeaver 25.3.1 - Main"
        ));
        assert!(matches!(
            session_command("SET search_path TO public").unwrap(),
            Some(SessionCommand::SetSearchPath)
        ));
        assert!(matches!(
            session_command("SET search_path TO DEFAULT").unwrap(),
            Some(SessionCommand::SetSearchPath)
        ));
    }

    #[test]
    fn exposes_only_truthful_session_values() {
        assert!(matches!(
            session_command("SHOW search_path").unwrap(),
            Some(SessionCommand::Show(SessionSetting::SearchPath))
        ));
        assert!(matches!(
            session_command("SHOW TRANSACTION ISOLATION LEVEL").unwrap(),
            Some(SessionCommand::Show(SessionSetting::TransactionIsolation))
        ));
        assert!(matches!(
            session_command("SHOW server_version_num").unwrap(),
            Some(SessionCommand::Show(SessionSetting::ServerVersionNum))
        ));
        assert!(matches!(
            session_command(
                "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE READ WRITE"
            )
            .unwrap(),
            Some(SessionCommand::SetTransactionCharacteristics {
                read_only: Some(false)
            })
        ));
    }

    #[test]
    fn rejects_unimplemented_or_misleading_session_settings() {
        for sql in [
            "SET extra_float_digits = 2",
            "SET ROLE admin",
            "SET search_path TO private",
            "SET LOCAL application_name = 'transaction-local'",
            "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL READ COMMITTED",
        ] {
            let PgWireError::UserError(error) = session_command(sql).unwrap_err() else {
                panic!("expected user error for {sql}");
            };
            assert_eq!(error.code, "0A000", "{sql}");
        }
    }

    #[test]
    fn storyden_catalog_probe_checks_for_a_missing_column() {
        let probe = CatalogPlan::recognize(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = $1) AND NOT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2)",
        )
        .unwrap();
        let parameters = [
            Parameter {
                scalar_type: ScalarType::Text,
                value: RawScalar::Text("robot_session_messages".into()),
            },
            Parameter {
                scalar_type: ScalarType::Text,
                value: RawScalar::Text("sequence".into()),
            },
        ];

        let empty = probe.execute(&[], &parameters).unwrap();
        let missing = probe
            .execute(
                &[table_with_columns("robot_session_messages", &["id"])],
                &parameters,
            )
            .unwrap();
        let present = probe
            .execute(
                &[table_with_columns(
                    "robot_session_messages",
                    &["id", "sequence"],
                )],
                &parameters,
            )
            .unwrap();

        assert_eq!(empty.columns, vec![bool_column("?column?", false)]);
        assert_eq!(empty.rows, vec![vec![Datum::Scalar(Value::Bool(false))]]);
        assert_eq!(missing.rows, vec![vec![Datum::Scalar(Value::Bool(true))]]);
        assert_eq!(present.rows, vec![vec![Datum::Scalar(Value::Bool(false))]]);
    }

    #[tokio::test]
    async fn implicit_transactions_retry_serializable_conflicts() {
        let mut attempts = 0;
        let result = retry_implicit_transaction(|| {
            attempts += 1;
            let attempt = attempts;
            async move {
                if attempt < 3 {
                    Err(crate::engine::exec::Error::message(
                        crate::engine::exec::ErrorKind::Conflict,
                        "retry",
                    ))
                } else {
                    Ok(7)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 7);
        assert_eq!(attempts, 3);
    }

    #[tokio::test]
    async fn implicit_transactions_do_not_retry_other_errors() {
        let mut attempts = 0;
        let error = retry_implicit_transaction(|| {
            attempts += 1;
            async {
                Err::<(), _>(crate::engine::exec::Error::message(
                    crate::engine::exec::ErrorKind::ConstraintViolation,
                    "constraint",
                ))
            }
        })
        .await
        .unwrap_err();

        assert_eq!(error.reason(), ErrorReason::ConstraintViolation);
        assert_eq!(attempts, 1);
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
