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
    let program = lower_pir(program)?;
    engine.execute_program(program, catalog_policy).await
}

pub async fn execute_pir_with_options(
    engine: &Engine,
    program: pir::Program,
    options: ProgramOptions,
) -> crate::engine::exec::Result<ProgramResult> {
    let program = lower_pir(program)?;
    engine.execute_program_with_options(program, options).await
}

fn lower_pir(program: pir::Program) -> crate::engine::exec::Result<crate::engine::exec::Program> {
    crate::protocol::lower_pir(program).map_err(|error| {
        let reason = error.reason();
        Error::source_with_reason(ErrorKind::InvalidInput, reason, error.to_string(), error)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::engine::kv::slatedb::Store;
    use crate::engine::lir::{Datum, ObjectField, Value};

    use super::*;

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
}
