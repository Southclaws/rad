#[path = "catalog.rs"]
mod synth_catalog;
#[path = "data.rs"]
mod synth_data;
mod fixture;
mod invalid;
mod metamorphic;
mod model;
mod nested_identity;
mod program;
mod query;
mod recursive;
mod semantic_model;
mod shrink;
#[cfg(test)]
mod coverage;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rad::engine::catalog::{self, model::TableDef};
use rad::engine::exec::{Engine, ErrorKind, ErrorReason};
use rad::engine::kv::TransactionalKv;
use rad::engine::kv::slatedb::Store;
use rad::engine::lir::{Datum, Query, Row};
use rad::service::result_json;

pub use fixture::emit_fixture;
pub use model::{ModelCase, check_model};
pub use nested_identity::nested_identity_case;
pub use program::{ProgramCase, check_invalid_program, check_program};
pub use recursive::{generate as recursive_from_decisions, recursive_case};
pub use semantic_model::{SemanticModelCase, check_semantic_model};
pub use shrink::{
    minimize, minimize_invalid, minimize_invalid_program, minimize_metamorphic, minimize_model,
    minimize_program, minimize_semantic_model,
};

pub type TestResult<T> = Result<T, String>;

#[derive(Clone, Debug)]
pub struct Case {
    pub kind: CaseKind,
    pub decisions: Vec<u64>,
    pub catalog: catalog::model::Schema,
    pub data: BTreeMap<String, Vec<Row>>,
    pub query: Query,
    pub ordered: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaseKind {
    Relational,
    Recursive,
    NestedIdentity,
}

impl Case {
    pub fn generate(decisions: Vec<u64>) -> Self {
        let mut choices = Choices::new(&decisions);
        let catalog = synth_catalog::generate(&mut choices);
        let data = synth_data::generate(&catalog, &mut choices);
        let (query, ordered) = query::generate(&catalog, &mut choices);
        Self {
            kind: CaseKind::Relational,
            decisions,
            catalog,
            data,
            query,
            ordered,
        }
    }

    pub fn from_seed(seed: u64) -> Self {
        Self::generate(decisions_from_seed(seed))
    }

    pub fn regenerate(&self, decisions: Vec<u64>) -> Self {
        match self.kind {
            CaseKind::Relational => Self::generate(decisions),
            CaseKind::Recursive => recursive::generate(decisions),
            CaseKind::NestedIdentity => nested_identity::generate(decisions),
        }
    }

    pub fn table_defs(&self) -> Vec<TableDef> {
        self.catalog.tables.clone()
    }
}

pub fn decisions_from_seed(seed: u64) -> Vec<u64> {
    let mut state = seed;
    (0..512)
        .map(|_| {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut value = state;
            value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            value ^ (value >> 31)
        })
        .collect()
}

pub struct Choices<'a> {
    values: &'a [u64],
    cursor: usize,
}

impl<'a> Choices<'a> {
    fn new(values: &'a [u64]) -> Self {
        Self { values, cursor: 0 }
    }

    pub fn index(&mut self, length: usize) -> usize {
        if length <= 1 {
            return 0;
        }
        let value = self.values.get(self.cursor).copied().unwrap_or_default();
        self.cursor += 1;
        (value % length as u64) as usize
    }

    pub fn range(&mut self, minimum: usize, maximum: usize) -> usize {
        minimum + self.index(maximum.saturating_sub(minimum) + 1)
    }

    pub fn coin(&mut self) -> bool {
        self.index(2) == 1
    }

    pub fn chance(&mut self, denominator: usize) -> bool {
        self.index(denominator) == 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Outcome {
    Value(serde_json::Value),
    Error(ErrorKind, ErrorReason),
}

static STORE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub async fn check(case: &Case) -> TestResult<()> {
    let (store, engine) = database(case).await?;
    let result = check_in(&engine, case).await;
    let close = store.close().await.map_err(|error| error.to_string());
    result?;
    close
}

pub async fn check_metamorphic(case: &Case) -> TestResult<()> {
    let (store, engine) = database(case).await?;
    let result = check_metamorphic_in(&engine, case).await;
    let close = store.close().await.map_err(|error| error.to_string());
    result?;
    close
}

pub async fn check_invalid(case: &Case) -> TestResult<()> {
    let (store, engine) = database(case).await?;
    let result = check_invalid_in(&engine, case).await;
    let close = store.close().await.map_err(|error| error.to_string());
    result?;
    close
}

async fn check_expected(case: &Case, expected: &serde_json::Value) -> TestResult<()> {
    let (store, engine) = database(case).await?;
    let chosen = observe(engine.execute(case.query.clone()).await)?;
    let forced = observe(engine.execute_forced(case.query.clone()).await)?;
    let nested = observe(engine.execute_nested(case.query.clone()).await)?;
    let reference = observe(engine.execute_reference(case.query.clone()).await)?;
    let expected = Outcome::Value(expected.clone());
    let close = store.close().await.map_err(|error| error.to_string());
    if chosen != expected || forced != expected || nested != expected || reference != expected {
        return Err(format!(
            "independent model mismatch\nexpected: {expected:?}\nchosen: {chosen:?}\nforced: {forced:?}\nnested: {nested:?}\nreference: {reference:?}\nquery: {:#?}",
            case.query
        ));
    }
    close
}

async fn database(case: &Case) -> TestResult<(Arc<Store>, Engine)> {
    let name = format!(
        "generative-{}",
        STORE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let store = Arc::new(
        Store::memory(&name)
            .await
            .map_err(|error| error.to_string())?,
    );
    let engine = Engine::new(store.clone());
    let catalog = catalog::Catalog::new(store.clone());
    for table in case.table_defs() {
        catalog
            .create_table(table)
            .await
            .map_err(|error| format!("create generated table: {error}"))?;
    }
    for table in &case.catalog.tables {
        if let Some(rows) = case.data.get(&table.name)
            && !rows.is_empty()
        {
            engine
                .create_many(&table.name, rows.clone())
                .await
                .map_err(|error| format!("seed generated table {:?}: {error}", table.name))?;
        }
    }
    Ok((store, engine))
}

async fn check_in(engine: &Engine, case: &Case) -> TestResult<()> {
    let chosen = observe(engine.execute(case.query.clone()).await)?;
    let forced = observe(engine.execute_forced(case.query.clone()).await)?;
    let nested = observe(engine.execute_nested(case.query.clone()).await)?;
    let reference = observe(engine.execute_reference(case.query.clone()).await)?;

    if matches!(chosen, Outcome::Error(ErrorKind::InvalidInput, _))
        || matches!(forced, Outcome::Error(ErrorKind::InvalidInput, _))
        || matches!(nested, Outcome::Error(ErrorKind::InvalidInput, _))
        || matches!(reference, Outcome::Error(ErrorKind::InvalidInput, _))
    {
        return Err(format!(
            "generator produced an invalid query\nchosen: {chosen:?}\nforced: {forced:?}\nnested: {nested:?}\nreference: {reference:?}\nquery: {:#?}",
            case.query
        ));
    }

    let chosen = comparable(chosen, case.ordered);
    let forced = comparable(forced, case.ordered);
    let nested = comparable(nested, case.ordered);
    let reference = comparable(reference, case.ordered);
    if chosen != reference || chosen != forced || chosen != nested {
        return Err(format!(
            "four-way differential mismatch\nchosen: {chosen:?}\nforced: {forced:?}\nnested: {nested:?}\nreference: {reference:?}\nquery: {:#?}",
            case.query
        ));
    }
    Ok(())
}

async fn check_metamorphic_in(engine: &Engine, case: &Case) -> TestResult<()> {
    check_in(engine, case).await?;
    let baseline = comparable(
        observe(engine.execute_reference(case.query.clone()).await)?,
        case.ordered,
    );
    for variant in metamorphic::variants(&case.query) {
        let chosen = comparable(
            observe(engine.execute(variant.query.clone()).await)?,
            case.ordered,
        );
        let forced = comparable(
            observe(engine.execute_forced(variant.query.clone()).await)?,
            case.ordered,
        );
        let nested = comparable(
            observe(engine.execute_nested(variant.query.clone()).await)?,
            case.ordered,
        );
        let reference = comparable(
            observe(engine.execute_reference(variant.query.clone()).await)?,
            case.ordered,
        );
        if chosen != baseline || forced != baseline || nested != baseline || reference != baseline {
            return Err(format!(
                "metamorphic mismatch for {}\nbaseline: {baseline:?}\nchosen: {chosen:?}\nforced: {forced:?}\nnested: {nested:?}\nreference: {reference:?}\noriginal: {:#?}\nvariant: {:#?}",
                variant.name, case.query, variant.query
            ));
        }
    }
    Ok(())
}

async fn check_invalid_in(engine: &Engine, case: &Case) -> TestResult<()> {
    for variant in invalid::variants(case) {
        assert_rejection(
            variant.name,
            "chosen",
            engine.execute(variant.query.clone()).await,
            variant.reason,
        )?;
        assert_rejection(
            variant.name,
            "forced",
            engine.execute_forced(variant.query.clone()).await,
            variant.reason,
        )?;
        assert_rejection(
            variant.name,
            "nested",
            engine.execute_nested(variant.query.clone()).await,
            variant.reason,
        )?;
        assert_rejection(
            variant.name,
            "reference",
            engine.execute_reference(variant.query).await,
            variant.reason,
        )?;
    }
    Ok(())
}

fn assert_rejection(
    variant: &str,
    executor: &str,
    result: rad::engine::exec::Result<Datum>,
    expected: ErrorReason,
) -> TestResult<()> {
    match result {
        Err(error) if error.kind() == ErrorKind::InvalidInput && error.reason() == expected => {
            Ok(())
        }
        Err(error) => Err(format!(
            "invalid variant {variant:?} through {executor} returned {:?}/{:?}, want InvalidInput/{expected:?}: {error}",
            error.kind(),
            error.reason()
        )),
        Ok(value) => Err(format!(
            "invalid variant {variant:?} unexpectedly succeeded through {executor}: {value:?}"
        )),
    }
}

pub async fn oracle_json(case: &Case) -> TestResult<serde_json::Value> {
    let (store, engine) = database(case).await?;
    let result = engine
        .execute_reference(case.query.clone())
        .await
        .map_err(|error| format!("reference outcome is an error: {error}"))
        .and_then(|value| {
            result_json::datum(&value).map_err(|error| format!("encode reference result: {error}"))
        });
    let close = store.close().await.map_err(|error| error.to_string());
    let value = result?;
    close?;
    Ok(value)
}

fn observe(result: rad::engine::exec::Result<Datum>) -> TestResult<Outcome> {
    match result {
        Ok(value) => result_json::datum(&value)
            .map(Outcome::Value)
            .map_err(|error| format!("encode differential result: {error}")),
        Err(error) => Ok(Outcome::Error(error.kind(), error.reason())),
    }
}

fn comparable(outcome: Outcome, ordered: bool) -> Outcome {
    if ordered {
        return outcome;
    }
    let Outcome::Value(serde_json::Value::Array(values)) = outcome else {
        return outcome;
    };
    let mut counts = BTreeMap::<String, usize>::new();
    for value in values {
        *counts.entry(value.to_string()).or_default() += 1;
    }
    Outcome::Value(serde_json::to_value(counts).expect("multiset is serializable"))
}
