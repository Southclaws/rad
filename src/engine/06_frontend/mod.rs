//! Transport-neutral native product boundary.

pub mod migration;
pub mod schema_transitions;

use std::sync::Arc;

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
    engine: Arc<Engine>,
    transaction: Option<Box<dyn Transaction>>,
    catalog_statements: Vec<String>,
}

impl Tx {
    pub async fn begin(engine: Arc<Engine>) -> crate::engine::exec::Result<Self> {
        let transaction = engine.begin_frontend_transaction().await?;
        Ok(Self {
            engine,
            transaction: Some(transaction),
            catalog_statements: Vec::new(),
        })
    }

    pub async fn execute_program(
        &mut self,
        program: Program,
        catalog_policy: CatalogPolicy,
    ) -> crate::engine::exec::Result<ProgramResult> {
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
        let result = self
            .engine
            .execute_program_in_transaction(transaction, &program, catalog_policy)
            .await?;
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
        self.engine
            .commit_frontend_transaction(transaction, std::mem::take(&mut self.catalog_statements))
            .await
    }

    pub fn rollback(mut self) {
        if let Some(transaction) = self.transaction.take() {
            transaction.rollback();
        }
    }
}

impl Drop for Tx {
    fn drop(&mut self) {
        if let Some(transaction) = self.transaction.take() {
            transaction.rollback();
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
    let capture = captured_program(engine, &program);
    let program = match lower_pir(program) {
        Ok(program) => program,
        Err(error) => {
            record_corpus_program(engine, capture, &std::collections::HashSet::new(), None);
            return Err(error);
        }
    };
    let relational = relational_statement_names(&program);
    let result = engine.execute_program(program, catalog_policy).await;
    record_corpus_program(engine, capture, &relational, result.as_ref().ok());
    result
}

pub async fn execute_pir_with_options(
    engine: &Engine,
    program: pir::Program,
    options: ProgramOptions,
) -> crate::engine::exec::Result<ProgramResult> {
    let capture = (!options.dry_run)
        .then(|| captured_program(engine, &program))
        .flatten();
    let program = match lower_pir(program) {
        Ok(program) => program,
        Err(error) => {
            record_corpus_program(engine, capture, &std::collections::HashSet::new(), None);
            return Err(error);
        }
    };
    let relational = relational_statement_names(&program);
    let result = engine.execute_program_with_options(program, options).await;
    record_corpus_program(engine, capture, &relational, result.as_ref().ok());
    result
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
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::engine::kv::slatedb::Store;
    use crate::engine::lir::{Datum, ObjectField, Value};

    use super::*;

    #[derive(Default)]
    struct ProgramObserver {
        programs: Mutex<Vec<crate::engine::exec::observe::ProgramRecord>>,
        skipped_oversize: AtomicU64,
    }

    impl crate::engine::exec::observe::ExecutionObserver for ProgramObserver {
        fn statement(&self, _observation: crate::engine::exec::observe::StatementObservation) {}

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
