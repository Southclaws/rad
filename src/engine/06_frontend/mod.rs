//! Transport-neutral native product boundary.

pub mod migration;
pub mod schema_transitions;

use std::sync::Arc;
use std::time::Instant;
use tracing::Instrument as _;

use crate::engine::catalog;
use crate::engine::catalog::model::Table;
use crate::engine::exec::{
    CatalogPolicy, Engine, Error, ErrorKind, Program, ProgramOptions, ProgramResult,
};
use crate::engine::kv::{Transaction, TransactionView};
use crate::protocol::generated::pir;

/// One frontend session transaction backed by one storage transaction.
///
/// Transport adapters own this handle. Programs remain independently valid
/// PIR units while reads, writes, and catalog changes share one snapshot until
/// the frontend commits or rolls back the handle.
pub struct Tx {
    id: String,
    engine: Arc<Engine>,
    transaction: Option<Box<dyn Transaction>>,
    catalog_statements: Vec<String>,
    dirty: bool,
    span: tracing::Span,
    started: Instant,
}

impl Tx {
    pub async fn begin(engine: Arc<Engine>) -> crate::engine::exec::Result<Self> {
        let transaction = engine.begin_frontend_transaction().await?;
        let id = uuid::Uuid::new_v4().to_string();
        let span = tracing::info_span!(
            target: "rad::telemetry",
            "rad.transaction",
            otel.kind = "internal",
            transaction_id = id,
            rad.transaction.outcome = tracing::field::Empty,
            rad.status = tracing::field::Empty,
            error.type = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        );
        Ok(Self {
            id,
            engine,
            transaction: Some(transaction),
            catalog_statements: Vec::new(),
            dirty: false,
            span,
            started: Instant::now(),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn span(&self) -> tracing::Span {
        self.span.clone()
    }

    pub async fn execute_program(
        &mut self,
        program: Program,
        catalog_policy: CatalogPolicy,
    ) -> crate::engine::exec::Result<ProgramResult> {
        let effectful = program
            .statements
            .iter()
            .any(|statement| statement.effectful());
        if effectful {
            self.dirty = true;
        }
        let relation_cache_eligible = !self.dirty;
        let catalog_statements = program
            .statements
            .iter()
            .filter(|statement| !statement.relational())
            .map(|statement| statement.name().to_owned())
            .collect::<Vec<_>>();
        let transaction = self
            .transaction
            .as_deref_mut()
            .expect("frontend transaction remains open while executing");
        let options = ProgramOptions {
            catalog: catalog_policy,
            ..ProgramOptions::default()
        };
        let program_log = ProgramLog::new(&program, &options);
        let result = self
            .engine
            .execute_program_in_transaction(
                transaction,
                &program,
                catalog_policy,
                relation_cache_eligible,
            )
            .instrument(program_log.span.clone())
            .await;
        program_log.finish(&result);
        let result = result?;
        self.catalog_statements.extend(catalog_statements);
        Ok(result)
    }

    pub async fn list_tables(&mut self) -> crate::engine::exec::Result<Vec<Table>> {
        let transaction = self
            .transaction
            .as_deref_mut()
            .expect("frontend transaction remains open while reading catalog");
        let mut view = TransactionView(&*transaction);
        catalog::store::list_tables(&mut view)
            .await
            .map_err(Into::into)
    }

    pub async fn commit(mut self) -> crate::engine::exec::Result<()> {
        let transaction = self
            .transaction
            .take()
            .expect("frontend transaction commits once");
        let result = self
            .engine
            .commit_frontend_transaction(transaction, std::mem::take(&mut self.catalog_statements))
            .instrument(self.span.clone())
            .await;
        self.span.record("rad.transaction.outcome", "commit");
        self.span.record(
            "rad.status",
            if result.is_ok() { "success" } else { "error" },
        );
        if let Err(error) = &result {
            self.span.record("error.type", error.kind().as_str());
            self.span.record("otel.status_code", "ERROR");
        }
        crate::telemetry::transaction_finished(
            "commit",
            if result.is_ok() { "success" } else { "error" },
            self.started.elapsed(),
            result.as_ref().err().and_then(|error| {
                (error.kind() == ErrorKind::Conflict).then(|| error.reason().as_str())
            }),
        );
        result
    }

    pub fn rollback(mut self) {
        if let Some(transaction) = self.transaction.take() {
            transaction.rollback();
        }
        self.span.record("rad.transaction.outcome", "rollback");
        self.span.record("rad.status", "success");
        crate::telemetry::transaction_finished("rollback", "success", self.started.elapsed(), None);
    }
}

impl Drop for Tx {
    fn drop(&mut self) {
        if let Some(transaction) = self.transaction.take() {
            transaction.rollback();
            self.span.record("rad.transaction.outcome", "abandoned");
            self.span.record("rad.status", "error");
            self.span.record("error.type", "abandoned");
            self.span.record("otel.status_code", "ERROR");
            crate::telemetry::transaction_finished(
                "abandoned",
                "error",
                self.started.elapsed(),
                None,
            );
        }
    }
}

/// Validate/lower the generated PIR envelope and execute it through the
/// numbered engine layers. Transport adapters can call this without inventing
/// a parallel request model.
pub async fn execute_pir(
    engine: &Engine,
    program: pir::Program,
    catalog_policy: CatalogPolicy,
) -> crate::engine::exec::Result<ProgramResult> {
    if let Some(diagnostics) = crate::logging::request_context().diagnostics {
        diagnostics.submitted(&program);
    }
    let capture = captured_program(engine, &program);
    let program = match lower_pir(program) {
        Ok(program) => program,
        Err(error) => {
            record_corpus_program(engine, capture, &std::collections::HashSet::new(), None);
            let result = Err(error);
            if let Some(diagnostics) = crate::logging::request_context().diagnostics {
                diagnostics.finish(&result);
            }
            return result;
        }
    };
    let relational = relational_statement_names(&program);
    let options = ProgramOptions {
        catalog: catalog_policy,
        ..ProgramOptions::default()
    };
    let result = execute_program_with_options(engine, program, options).await;
    record_corpus_program(engine, capture, &relational, result.as_ref().ok());
    result
}

pub async fn execute_pir_with_options(
    engine: &Engine,
    program: pir::Program,
    options: ProgramOptions,
) -> crate::engine::exec::Result<ProgramResult> {
    if let Some(diagnostics) = crate::logging::request_context().diagnostics {
        diagnostics.submitted(&program);
    }
    let capture = (!options.dry_run)
        .then(|| captured_program(engine, &program))
        .flatten();
    let program = match lower_pir(program) {
        Ok(program) => program,
        Err(error) => {
            record_corpus_program(engine, capture, &std::collections::HashSet::new(), None);
            let result = Err(error);
            if let Some(diagnostics) = crate::logging::request_context().diagnostics {
                diagnostics.finish(&result);
            }
            return result;
        }
    };
    let relational = relational_statement_names(&program);
    let result = execute_program_with_options(engine, program, options).await;
    record_corpus_program(engine, capture, &relational, result.as_ref().ok());
    result
}

pub async fn execute_program_with_options(
    engine: &Engine,
    program: Program,
    options: ProgramOptions,
) -> crate::engine::exec::Result<ProgramResult> {
    let program_log = ProgramLog::new(&program, &options);
    let result = engine
        .execute_program_with_options(program, options)
        .instrument(program_log.span.clone())
        .await;
    program_log.finish(&result);
    result
}

struct ProgramLog {
    enabled: bool,
    started: Option<Instant>,
    fingerprint: String,
    statements: usize,
    mutation_statements: usize,
    mutation_names: Vec<String>,
    dry_run: bool,
    context: crate::logging::RequestContext,
    span: tracing::Span,
    trace_id: String,
    span_id: String,
    metrics: bool,
    diagnostics: Option<crate::diagnostics::ProgramDiagnosticRecorder>,
}

impl ProgramLog {
    fn new(program: &Program, options: &ProgramOptions) -> Self {
        let context = crate::logging::request_context();
        let transport = if context.transport.is_empty() {
            "direct"
        } else {
            context.transport
        };
        let parent = context
            .parent_span
            .clone()
            .unwrap_or_else(tracing::Span::current);
        let span = tracing::info_span!(
            target: "rad::telemetry",
            parent: &parent,
            "rad.program.execute",
            otel.kind = "internal",
            request_id = context.request_id,
            rad.transport = transport,
            rad.program.fingerprint = tracing::field::Empty,
            rad.program.statement_count = program.statements.len(),
            rad.program.mutation_statement_count = program.statements.iter().filter(|statement| statement.effectful()).count(),
            rad.program.dry_run = options.dry_run,
            rad.status = tracing::field::Empty,
            rad.program.result_rows = tracing::field::Empty,
            rad.program.affected_rows = tracing::field::Empty,
            error.type = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
        );
        let enabled = tracing::event_enabled!(target: "rad::program", tracing::Level::INFO);
        let metrics = crate::telemetry::enabled();
        let diagnostics = context.diagnostics.clone();
        let fingerprint_enabled = enabled || !span.is_disabled() || diagnostics.is_some();
        if !fingerprint_enabled && !metrics {
            return Self {
                enabled,
                started: None,
                fingerprint: String::new(),
                statements: 0,
                mutation_statements: 0,
                mutation_names: Vec::new(),
                dry_run: options.dry_run,
                context,
                span,
                trace_id: String::new(),
                span_id: String::new(),
                metrics,
                diagnostics,
            };
        }
        let fingerprints = fingerprint_enabled
            .then(|| crate::engine::exec::diagnostic::program_fingerprints(program));
        if let Some(fingerprints) = &fingerprints {
            span.record("rad.program.fingerprint", fingerprints.family.as_str());
        }
        let (trace_id, span_id) = crate::telemetry::span_ids(&span);
        if let Some(diagnostics) = &diagnostics
            && let Some(fingerprints) = &fingerprints
        {
            diagnostics.start(program, options, fingerprints);
        }
        if metrics {
            crate::telemetry::program_started(transport);
        }
        if enabled
            && tracing::event_enabled!(target: "rad::program", tracing::Level::DEBUG)
            && let Some(fingerprints) = &fingerprints
        {
            let document = crate::engine::exec::diagnostic::program_document(program);
            if document.len() <= crate::engine::exec::diagnostic::MAX_PROGRAM_DOCUMENT_BYTES {
                tracing::debug!(
                    target: "rad::program",
                    event = "program.diagnostic",
                    component = "frontend",
                    program_fingerprint = fingerprints.family,
                    exact_program_fingerprint = fingerprints.exact,
                    transport = context.transport,
                    request_id = context.request_id,
                    trace_id,
                    span_id,
                    transaction_id = context.transaction_id,
                    client_ip = context.client_ip,
                    diagnostic = %String::from_utf8_lossy(&document),
                    diagnostic_bytes = document.len(),
                    diagnostic_omitted = false,
                    message = "program diagnostic is available"
                );
            } else {
                tracing::debug!(
                    target: "rad::program",
                    event = "program.diagnostic",
                    component = "frontend",
                    program_fingerprint = fingerprints.family,
                    exact_program_fingerprint = fingerprints.exact,
                    transport = context.transport,
                    request_id = context.request_id,
                    trace_id,
                    span_id,
                    transaction_id = context.transaction_id,
                    client_ip = context.client_ip,
                    diagnostic_bytes = document.len(),
                    diagnostic_omitted = true,
                    message = "program diagnostic is omitted"
                );
            }
        }
        let mutation_names = program
            .statements
            .iter()
            .filter(|statement| {
                matches!(
                    statement,
                    crate::engine::exec::Statement::Create { .. }
                        | crate::engine::exec::Statement::Update { .. }
                        | crate::engine::exec::Statement::Delete { .. }
                )
            })
            .map(|statement| statement.name().to_owned())
            .collect::<Vec<_>>();
        Self {
            enabled,
            started: Some(Instant::now()),
            fingerprint: fingerprints.map_or_else(String::new, |value| value.family),
            statements: program.statements.len(),
            mutation_statements: mutation_names.len(),
            mutation_names,
            dry_run: options.dry_run,
            context,
            span,
            trace_id,
            span_id,
            metrics,
            diagnostics,
        }
    }

    fn finish(&self, result: &crate::engine::exec::Result<ProgramResult>) {
        let duration = self
            .started
            .map_or(std::time::Duration::ZERO, |started| started.elapsed());
        let duration_ms = duration.as_millis() as u64;
        let transport = if self.context.transport.is_empty() {
            "direct"
        } else {
            self.context.transport
        };
        let (status, rows, affected) = match result {
            Ok(result) => (
                "success",
                result_rows(&result.result),
                result
                    .statements
                    .iter()
                    .filter(|statement| self.mutation_names.contains(&statement.name))
                    .map(|statement| statement.affected as u64)
                    .sum(),
            ),
            Err(_) => ("error", 0, 0),
        };
        self.span.record("rad.status", status);
        self.span.record("rad.program.result_rows", rows);
        self.span.record("rad.program.affected_rows", affected);
        if let Err(error) = result {
            self.span.record("error.type", error.kind().as_str());
            self.span.record("otel.status_code", "ERROR");
        }
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics.finish(result);
        }
        if self.metrics {
            crate::telemetry::program_finished(crate::telemetry::ProgramMeasurement {
                transport,
                status,
                duration,
                result_rows: rows,
                affected_rows: affected,
                statements: self.statements as u64,
                dry_run: self.dry_run,
            });
        }
        if !self.enabled {
            return;
        }
        match result {
            Ok(_) => tracing::info!(
                target: "rad::program",
                event = "program.executed",
                component = "frontend",
                program_fingerprint = self.fingerprint,
                statements = self.statements,
                mutation_statements = self.mutation_statements,
                transport,
                request_id = self.context.request_id,
                trace_id = self.trace_id,
                span_id = self.span_id,
                transaction_id = self.context.transaction_id,
                client_ip = self.context.client_ip,
                duration_ms,
                status = "success",
                result_rows = rows,
                affected_rows = affected,
                dry_run = self.dry_run,
                transaction_state = self.context.transaction_state,
                message = "program executed"
            ),
            Err(error) => tracing::info!(
                target: "rad::program",
                event = "program.executed",
                component = "frontend",
                program_fingerprint = self.fingerprint,
                statements = self.statements,
                mutation_statements = self.mutation_statements,
                transport,
                request_id = self.context.request_id,
                trace_id = self.trace_id,
                span_id = self.span_id,
                transaction_id = self.context.transaction_id,
                client_ip = self.context.client_ip,
                duration_ms,
                status = "error",
                error_kind = error.kind().as_str(),
                error_reason = error.reason().as_str(),
                dry_run = self.dry_run,
                transaction_state = self.context.transaction_state,
                message = "program execution failed"
            ),
        }
    }
}

fn result_rows(result: &crate::engine::lir::Datum) -> u64 {
    match result {
        crate::engine::lir::Datum::Null => 0,
        crate::engine::lir::Datum::Array(rows) => rows.len() as u64,
        crate::engine::lir::Datum::Scalar(_) | crate::engine::lir::Datum::Object(_) => 1,
    }
}

/// Capture the submitted wire program for the workload corpus. Failed
/// executions are workload too, so this runs before lowering; a document too
/// broken to serialize is simply not recorded.
struct CapturedProgram {
    canonical: Vec<u8>,
    content_hash: [u8; 16],
    at_unix_micros: u64,
    statements: u32,
}

fn captured_program(engine: &Engine, program: &pir::Program) -> Option<CapturedProgram> {
    let observer = engine.observer()?;
    if !observer.captures_programs() {
        return None;
    }
    let (canonical, content_hash) = match canonical_program(program) {
        Ok(captured) => captured,
        Err(CanonicalProgramError::Oversize) => {
            observer.program_skipped_oversize();
            return None;
        }
        Err(CanonicalProgramError::Invalid) => return None,
    };
    Some(CapturedProgram {
        canonical,
        content_hash,
        at_unix_micros: engine.now_unix_micros(),
        statements: program.statements.len() as u32,
    })
}

fn relational_statement_names(program: &Program) -> std::collections::HashSet<String> {
    program
        .statements
        .iter()
        .filter(|statement| statement.relational())
        .map(|statement| statement.name().to_owned())
        .collect()
}

fn record_corpus_program(
    engine: &Engine,
    capture: Option<CapturedProgram>,
    relational: &std::collections::HashSet<String>,
    result: Option<&ProgramResult>,
) {
    let Some(capture) = capture else {
        return;
    };
    let outcomes = result
        .into_iter()
        .flat_map(|result| &result.statements)
        .filter(|statement| relational.contains(&statement.name))
        .map(
            |statement| crate::engine::exec::observe::ProgramStatementOutcome {
                name: statement.name.clone(),
                rows: statement.affected as u64,
            },
        )
        .collect();
    engine
        .observer()
        .expect("corpus capture has an observer")
        .program(crate::engine::exec::observe::ProgramRecord {
            canonical: capture.canonical,
            content_hash: capture.content_hash,
            at_unix_micros: capture.at_unix_micros,
            statements: capture.statements,
            outcomes,
        });
}

/// Documents above this size are not captured for the corpus. Bulk-load
/// programs embed their row payloads; archiving them grows the store and
/// slows foreground reads without telling the estimator anything. Query
/// documents sit well under this bound; row-batch documents sit above it.
pub const MAX_CORPUS_DOCUMENT_BYTES: usize = 4 * 1024;

/// Canonical JSON bytes of a wire program: parse/serialize round-trip through
/// `serde_json::Value`, whose object representation is key-sorted, so two
/// documents differing only in formatting or key order produce identical
/// bytes — including inside the embedded raw LIR documents. The content hash
/// is the truncated SHA-256 of those bytes.
pub fn canonical_program_bytes(program: &pir::Program) -> Option<(Vec<u8>, [u8; 16])> {
    canonical_program(program).ok()
}

enum CanonicalProgramError {
    Invalid,
    Oversize,
}

fn canonical_program(program: &pir::Program) -> Result<(Vec<u8>, [u8; 16]), CanonicalProgramError> {
    use sha2::{Digest as _, Sha256};

    let serialized = serde_json::to_vec(program).map_err(|_| CanonicalProgramError::Invalid)?;
    if serialized.len() > MAX_CORPUS_DOCUMENT_BYTES {
        return Err(CanonicalProgramError::Oversize);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&serialized).map_err(|_| CanonicalProgramError::Invalid)?;
    let canonical = serde_json::to_vec(&value).map_err(|_| CanonicalProgramError::Invalid)?;
    let mut hasher = Sha256::new();
    hasher.update(&canonical);
    let full = hasher.finalize();
    let mut content_hash = [0u8; 16];
    content_hash.copy_from_slice(&full[..16]);
    Ok((canonical, content_hash))
}

fn lower_pir(program: pir::Program) -> crate::engine::exec::Result<Program> {
    crate::protocol::lower_pir(program).map_err(|error| {
        let reason = error.reason();
        Error::source_with_reason(ErrorKind::InvalidInput, reason, error.to_string(), error)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::engine::catalog::identity::SchemaId;
    use crate::engine::catalog::model::{ColumnDef, ScalarType, TableDef};
    use crate::engine::kv::slatedb::Store;
    use crate::engine::lir::{
        Datum, Kind, ObjectField, RawScalar, Relation, RootCardinality, RowsColumn, Value,
    };
    use tracing_subscriber::prelude::*;

    use super::*;

    #[derive(Default)]
    struct ProgramObserver {
        programs: Mutex<Vec<crate::engine::exec::observe::ProgramRecord>>,
        statements: Mutex<Vec<crate::engine::exec::observe::StatementObservation>>,
        skipped_oversize: AtomicU64,
    }

    impl crate::engine::exec::observe::ExecutionObserver for ProgramObserver {
        fn statement(&self, observation: crate::engine::exec::observe::StatementObservation) {
            self.statements
                .lock()
                .expect("statement observations")
                .push(observation);
        }

        fn captures_programs(&self) -> bool {
            true
        }

        fn program(&self, record: crate::engine::exec::observe::ProgramRecord) {
            self.programs.lock().expect("program records").push(record);
        }

        fn program_skipped_oversize(&self) {
            self.skipped_oversize.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn program_span_uses_the_current_otel_trace() {
        use opentelemetry::trace::TracerProvider as _;
        use tracing_subscriber::filter::{LevelFilter, Targets};

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let tracer = provider.tracer("rad-test");
        let subscriber = tracing_subscriber::registry()
            .with(
                tracing_opentelemetry::layer()
                    .with_tracer(tracer)
                    .with_filter(
                        Targets::new()
                            .with_default(LevelFilter::OFF)
                            .with_target("rad::telemetry", LevelFilter::DEBUG),
                    ),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::sink)
                    .with_filter(
                        Targets::new()
                            .with_default(LevelFilter::OFF)
                            .with_target("rad", LevelFilter::DEBUG)
                            .with_target("rad::program", LevelFilter::DEBUG),
                    ),
            );
        tracing::subscriber::with_default(subscriber, || {
            let parent = tracing::info_span!(target: "rad::telemetry", "test.request");
            let (parent_trace_id, parent_span_id) = crate::telemetry::span_ids(&parent);
            parent.in_scope(|| {
                let program = Program {
                    statements: Vec::new(),
                    result: None,
                };
                let program_log = ProgramLog::new(&program, &ProgramOptions::default());
                assert_eq!(program_log.trace_id, parent_trace_id);
                assert_ne!(program_log.span_id, parent_span_id);
                assert!(!program_log.span_id.is_empty());
            });
        });
    }

    #[tokio::test]
    async fn explicit_transaction_bypasses_cached_relations_after_a_write() {
        let store = Arc::new(
            Store::memory("frontend-relation-cache-dirty")
                .await
                .unwrap(),
        );
        let catalog = catalog::Catalog::new(store.clone());
        catalog
            .create_table(TableDef {
                id: SchemaId::new(1).unwrap(),
                name: "tasks".into(),
                columns: vec![
                    ColumnDef {
                        id: SchemaId::new(1).unwrap(),
                        name: "id".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                    ColumnDef {
                        id: SchemaId::new(2).unwrap(),
                        name: "status".into(),
                        scalar_type: ScalarType::Text,
                        nullable: false,
                        format: String::new(),
                        default: None,
                    },
                ],
                primary_key: vec!["id".into()],
                indexes: Vec::new(),
                foreign_keys: Vec::new(),
            })
            .await
            .unwrap();
        let observer = Arc::new(ProgramObserver::default());
        let engine = Arc::new(Engine::new(store).with_observer(observer.clone()));
        engine
            .create(
                "tasks",
                crate::engine::lir::Row::from([
                    ("id".into(), Value::Text("a".into())),
                    ("status".into(), Value::Text("open".into())),
                ]),
            )
            .await
            .unwrap();
        let read = Program {
            statements: vec![crate::engine::exec::Statement::Query {
                name: "read".into(),
                relation: crate::engine::lir::Query {
                    root: Relation::Order {
                        input: Box::new(Relation::Scan {
                            table: "tasks".into(),
                            scope: "task".into(),
                        }),
                        terms: vec![crate::engine::lir::OrderTerm {
                            expression: crate::engine::lir::Expr::Column {
                                scope: "task".into(),
                                name: "id".into(),
                            },
                            descending: false,
                        }],
                    },
                    cardinality: RootCardinality::Many,
                    bindings: HashMap::new(),
                },
            }],
            result: None,
        };
        let create = Program {
            statements: vec![crate::engine::exec::Statement::Create {
                name: "create".into(),
                relation: crate::engine::lir::Query {
                    root: Relation::Rows {
                        scope: "input".into(),
                        columns: vec![
                            RowsColumn {
                                name: "id".into(),
                                kind: Kind::Text,
                                nullable: false,
                            },
                            RowsColumn {
                                name: "status".into(),
                                kind: Kind::Text,
                                nullable: false,
                            },
                        ],
                        values: vec![vec![
                            RawScalar::Text("b".into()),
                            RawScalar::Text("open".into()),
                        ]],
                    },
                    cardinality: RootCardinality::Many,
                    bindings: HashMap::new(),
                },
                table: "tasks".into(),
            }],
            result: None,
        };
        let mut transaction = Tx::begin(engine).await.unwrap();

        transaction
            .execute_program(read.clone(), CatalogPolicy::Forbidden)
            .await
            .unwrap();
        transaction
            .execute_program(read.clone(), CatalogPolicy::Forbidden)
            .await
            .unwrap();
        transaction
            .execute_program(create, CatalogPolicy::Forbidden)
            .await
            .unwrap();
        let after_write = transaction
            .execute_program(read, CatalogPolicy::Forbidden)
            .await
            .unwrap();
        assert!(matches!(after_write.result, Datum::Array(ref rows) if rows.len() == 2));

        let statements = observer.statements.lock().expect("statement observations");
        assert_eq!(statements.len(), 4);
        assert_eq!(
            statements[0].source,
            crate::engine::exec::observe::StatementSource::Executed
        );
        assert_eq!(
            statements[1].source,
            crate::engine::exec::observe::StatementSource::RelationCache
        );
        assert_eq!(
            statements[3].source,
            crate::engine::exec::observe::StatementSource::Executed
        );
        drop(statements);
        transaction.rollback();
    }

    #[test]
    fn normal_program_event_does_not_contain_program_literals() {
        let program = lower_pir(
            serde_json::from_value(serde_json::json!({
                "statements": [{
                    "kind": "query",
                    "name": "read",
                    "relation": {
                        "nodes": {
                            "rows": {
                                "kind": "rows",
                                "scope": "r",
                                "columns": [{"name": "value", "type": "text"}],
                                "rows": [["private-value"]]
                            }
                        },
                        "root": {"node": "rows", "cardinality": "many"}
                    }
                }],
                "result": "read"
            }))
            .unwrap(),
        )
        .unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::INFO)
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_target(false)
                    .with_writer(ProgramCapture(output.clone())),
            );
        tracing::subscriber::with_default(subscriber, || {
            ProgramLog::new(&program, &ProgramOptions::default()).finish(&Ok(ProgramResult {
                result: Datum::Array(Vec::new()),
                statements: vec![crate::engine::exec::StatementResult {
                    name: "read".to_owned(),
                    affected: 0,
                    control: None,
                }],
                plans: Vec::new(),
            }));
        });
        let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(output.contains("program.executed"));
        assert!(output.contains("program_fingerprint"));
        assert!(!output.contains("private-value"));
        assert!(!output.contains("diagnostic"));
    }

    #[derive(Clone)]
    struct ProgramCapture(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ProgramCapture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl std::io::Write for ProgramCapture {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write_all(buffer)?;
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn canonical_program_bytes_ignore_formatting_and_key_order() {
        let compact: pir::Program = serde_json::from_str(
            r#"{"statements":[{"name":"read","kind":"query","relation":{"nodes":{"input":{"kind":"rows","scope":"input","columns":[{"name":"id","type":"text"}],"rows":[["a"]]}},"root":{"node":"input","cardinality":"many"}}}],"result":"read"}"#,
        )
        .unwrap();
        let reordered: pir::Program = serde_json::from_str(
            r#"{
                "result": "read",
                "statements": [ {
                    "kind": "query",
                    "name": "read",
                    "relation": {
                        "root": { "cardinality": "many", "node": "input" },
                        "nodes": { "input": {
                            "rows": [["a"]],
                            "columns": [{"type": "text", "name": "id"}],
                            "scope": "input",
                            "kind": "rows"
                        } }
                    }
                } ]
            }"#,
        )
        .unwrap();

        let (canonical, hash) = canonical_program_bytes(&compact).unwrap();
        let (reordered_canonical, reordered_hash) = canonical_program_bytes(&reordered).unwrap();
        assert_eq!(canonical, reordered_canonical);
        assert_eq!(hash, reordered_hash);
        let text = String::from_utf8(canonical).unwrap();
        assert!(!text.contains('\n'));
    }

    #[tokio::test]
    async fn generated_pir_runs_without_a_parallel_transport_model() {
        let wire = serde_json::from_str::<pir::Program>(
            r#"{
            "statements": [{
                "kind": "query",
                "name": "answer",
                "relation": {
                    "nodes": {
                        "row": {
                            "kind": "rows",
                            "scope": "literal",
                            "columns": [{"name": "value", "type": "int64"}],
                            "rows": [["9007199254740993"]]
                        }
                    },
                    "root": {"node": "row", "cardinality": "exactly_one"}
                }
            }]
        }"#,
        )
        .unwrap();
        let store = Arc::new(Store::memory("pir-wire-execution").await.unwrap());
        let result = execute_pir(&Engine::new(store), wire, CatalogPolicy::Forbidden)
            .await
            .unwrap();
        assert_eq!(
            result.result,
            Datum::Object(vec![ObjectField {
                name: "value".into(),
                datum: Datum::Scalar(Value::Int64(9_007_199_254_740_993)),
            }])
        );
    }

    #[tokio::test]
    async fn corpus_records_successful_statement_actuals() {
        let wire = serde_json::from_str::<pir::Program>(
            r#"{
                "statements": [{
                    "kind": "query",
                    "name": "read",
                    "relation": {
                        "nodes": {
                            "rows": {
                                "kind": "rows",
                                "scope": "r",
                                "columns": [{"name": "id", "type": "int64"}],
                                "rows": [["1"], ["2"]]
                            },
                            "ordered": {
                                "kind": "order",
                                "input": "rows",
                                "terms": [{
                                    "expr": {"kind": "col", "scope": "r", "column": "id"}
                                }]
                            }
                        },
                        "root": {"node": "ordered", "cardinality": "many"}
                    }
                }],
                "result": "read"
            }"#,
        )
        .unwrap();
        let store = Arc::new(Store::memory("pir-corpus-actuals").await.unwrap());
        let observer = Arc::new(ProgramObserver::default());
        let engine = Engine::new(store).with_observer(observer.clone());

        execute_pir(&engine, wire, CatalogPolicy::Forbidden)
            .await
            .unwrap();

        let programs = observer.programs.lock().expect("program records");
        assert_eq!(programs.len(), 1);
        assert_eq!(
            programs[0].outcomes,
            vec![crate::engine::exec::observe::ProgramStatementOutcome {
                name: "read".into(),
                rows: 2,
            }]
        );
    }

    #[tokio::test]
    async fn corpus_reports_a_program_over_the_capture_limit() {
        let wire = serde_json::from_value::<pir::Program>(serde_json::json!({
            "statements": [{
                "kind": "query",
                "name": "read",
                "relation": {
                    "nodes": {
                        "rows": {
                            "kind": "rows",
                            "scope": "r",
                            "columns": [{"name": "value", "type": "text"}],
                            "rows": [["x".repeat(MAX_CORPUS_DOCUMENT_BYTES)]]
                        },
                        "ordered": {
                            "kind": "order",
                            "input": "rows",
                            "terms": [{
                                "expr": {"kind": "col", "scope": "r", "column": "value"}
                            }]
                        }
                    },
                    "root": {"node": "ordered", "cardinality": "many"}
                }
            }],
            "result": "read"
        }))
        .unwrap();
        let store = Arc::new(Store::memory("pir-corpus-oversize").await.unwrap());
        let observer = Arc::new(ProgramObserver::default());
        let engine = Engine::new(store).with_observer(observer.clone());

        execute_pir(&engine, wire, CatalogPolicy::Forbidden)
            .await
            .unwrap();

        assert_eq!(observer.skipped_oversize.load(Ordering::Relaxed), 1);
        assert!(
            observer
                .programs
                .lock()
                .expect("program records")
                .is_empty()
        );
    }
}
