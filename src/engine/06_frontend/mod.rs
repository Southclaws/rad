//! Transport-neutral native product boundary.

pub mod migration;
pub mod schema_transitions;

use crate::engine::exec::{CatalogPolicy, Engine, Error, ErrorKind, ProgramOptions, ProgramResult};
use crate::protocol::generated::pir;

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
