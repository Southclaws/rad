//! Data-driven production/reference differential over the shared E2E corpus.
//!
//! The fixtures remain language-neutral and live in `tests/e2e`. This runner
//! discovers them in place; it does not copy or translate their schemas,
//! programs, seeds, errors, assertions, or expected results.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rad::engine::catalog;
use rad::engine::catalog::model::ScalarType;
use rad::engine::exec::{self, CatalogPolicy, Engine, ProgramOptions};
use rad::engine::kv::TransactionalKv;
use rad::engine::kv::slatedb::Store;
use rad::engine::lir::{Row, Value};
use rad::protocol::generated::{lir as lir_wire, pir};
use rad::service::error::Failure;
use rad::service::result_json;
use serde::{Deserialize, Deserializer};

type CaseResult<T> = Result<T, String>;

#[derive(Deserialize)]
struct Fixture {
    program: serde_json::Value,
    #[serde(default)]
    result: ExpectedResult,
    #[serde(default)]
    error: Option<ErrorExpectation>,
    #[serde(default)]
    assertions: Vec<Assertion>,
}

#[derive(Default)]
enum ExpectedResult {
    #[default]
    Missing,
    Present(serde_json::Value),
}

impl<'de> Deserialize<'de> for ExpectedResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        serde_json::Value::deserialize(deserializer).map(Self::Present)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ErrorExpectation {
    Contains(String),
    Fields {
        #[serde(default)]
        code: String,
        #[serde(default)]
        reason: String,
        #[serde(default)]
        contains: String,
    },
}

impl ErrorExpectation {
    fn code(&self) -> &str {
        match self {
            Self::Contains(_) => "",
            Self::Fields { code, .. } => code,
        }
    }

    fn reason(&self) -> &str {
        match self {
            Self::Contains(_) => "",
            Self::Fields { reason, .. } => reason,
        }
    }

    fn contains(&self) -> &str {
        match self {
            Self::Contains(contains) | Self::Fields { contains, .. } => contains,
        }
    }
}

#[derive(Deserialize)]
struct Assertion {
    name: String,
    query: serde_json::Value,
    expect: serde_json::Value,
}

#[derive(Deserialize)]
struct SeedGroup {
    table: String,
    rows: Vec<HashMap<String, serde_json::Value>>,
}

struct FixtureDb {
    engine: Engine,
    store: Arc<Store>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ObservedError {
    code: &'static str,
    reason: Option<&'static str>,
    detail: String,
}

impl ObservedError {
    fn invalid(detail: impl Into<String>) -> Self {
        Self {
            code: "invalid",
            reason: None,
            detail: detail.into(),
        }
    }

    fn lowering(error: rad::protocol::LowerError) -> Self {
        Self {
            code: "invalid",
            reason: Some(error.reason().as_str()),
            detail: error.to_string(),
        }
    }

    fn execution(error: &exec::Error) -> Self {
        let (code, reason) = match Failure::from_exec(error) {
            Failure::Invalid(failure) => ("invalid", Some(failure.reason.as_str())),
            Failure::ExecutionFailed(failure) => {
                ("execution_failed", Some(failure.reason.as_str()))
            }
            Failure::Conflict(failure) => ("conflict", Some(failure.reason.as_str())),
            Failure::NotFound(failure) => ("not_found", Some(failure.reason.as_str())),
            Failure::Internal(_) => ("internal", None),
        };
        Self {
            code,
            reason,
            detail: error.to_string(),
        }
    }
}

#[tokio::test]
async fn shared_e2e_cases_match_production_reference_and_expected_results() {
    let cases = discover_cases().expect("discover shared E2E corpus");
    let mut failures = Vec::new();
    for (name, fixture_path) in &cases {
        if let Err(error) = run_case(name, fixture_path).await {
            failures.push((name, error));
        }
    }

    if failures.is_empty() {
        println!("all {} shared E2E fixtures passed", cases.len());
        return;
    }
    let mut report = format!(
        "{} of {} shared E2E fixtures failed:\n",
        failures.len(),
        cases.len()
    );
    for (name, error) in failures {
        let _ = writeln!(report, "\n{name}:\n  {}", error.replace('\n', "\n  "));
    }
    panic!("{report}");
}

#[tokio::test]
async fn reference_mutation_smoke() {
    let selected = [
        "agg_count_arg_skips_null",
        "agg_empty_set",
        "agg_float_sum_avg",
        "agg_group_by_single",
        "agg_min_max",
        "agg_sum_avg",
        "binding_derived",
        "bug_signed_zero_group_key",
        "concatenate_distinct_union",
        "cross_array_to_many",
        "cross_exists",
        "cross_first_to_one",
        "cross_scalar_count",
        "corr_two_crossings_compared",
        "except_quantifiers",
        "intersect_quantifiers",
        "join_left_unmatched",
        "ord_slice_offset_limit",
        "pir_ref_chain",
        "recursive_depth",
        "recursive_diamond",
        "recursive_reachability",
    ];
    let cases = discover_cases()
        .expect("discover shared E2E corpus")
        .into_iter()
        .collect::<HashMap<_, _>>();
    for name in selected {
        let path = cases
            .get(name)
            .unwrap_or_else(|| panic!("mutation-smoke fixture {name:?} is missing"));
        run_case(name, path)
            .await
            .unwrap_or_else(|error| panic!("mutation-smoke fixture {name:?}: {error}"));
    }
}

#[test]
fn authored_corpus_reaches_every_public_lir_feature() {
    let cases = discover_cases().expect("discover shared E2E corpus");
    let mut features = BTreeSet::new();
    for (_, path) in cases {
        let value: serde_json::Value = serde_json::from_slice(
            &fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
        )
        .unwrap_or_else(|error| panic!("decode {}: {error}", path.display()));
        collect_contract_features(&value, &mut features);
    }
    let expected = [
        "accumulation.all",
        "accumulation.new",
        "cardinality.exactly_one",
        "cardinality.first",
        "cardinality.many",
        "cardinality.scalar",
        "comparison.exact",
        "comparison.unicode_simple_fold",
        "fn.avg",
        "fn.count",
        "fn.max",
        "fn.min",
        "fn.sum",
        "join.inner",
        "join.left",
        "kind.aggregate",
        "kind.any_many",
        "kind.array",
        "kind.binary",
        "kind.branch",
        "kind.cast",
        "kind.col",
        "kind.concatenate",
        "kind.derived",
        "kind.distinct",
        "kind.except",
        "kind.exists",
        "kind.filter",
        "kind.first",
        "kind.intersect",
        "kind.join",
        "kind.lit",
        "kind.literal",
        "kind.order",
        "kind.project",
        "kind.recursive",
        "kind.recursive_ref",
        "kind.ref",
        "kind.rows",
        "kind.scalar",
        "kind.scan",
        "kind.slice",
        "kind.text_match",
        "kind.unary",
        "op.add",
        "op.and",
        "op.div",
        "op.eq",
        "op.gt",
        "op.gte",
        "op.is_not_null",
        "op.is_null",
        "op.lt",
        "op.lte",
        "op.mul",
        "op.ne",
        "op.negate",
        "op.not",
        "op.or",
        "op.sub",
        "quantifier.all",
        "quantifier.distinct",
        "spread.non_empty",
        "type.bool",
        "type.float64",
        "type.int64",
        "type.text",
    ]
    .into_iter()
    .map(String::from)
    .collect::<BTreeSet<_>>();
    let missing = expected.difference(&features).collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "authored expected-result corpus lacks {missing:?}; reached {features:?}"
    );
}

fn collect_contract_features(value: &serde_json::Value, features: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect_contract_features(value, features);
            }
        }
        serde_json::Value::Object(object) => {
            for key in [
                "kind",
                "op",
                "join",
                "quantifier",
                "cardinality",
                "accumulation",
                "comparison",
                "fn",
                "type",
            ] {
                if let Some(value) = object.get(key).and_then(serde_json::Value::as_str) {
                    features.insert(format!("{key}.{value}"));
                }
            }
            if object
                .get("spread")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|spread| !spread.is_empty())
            {
                features.insert("spread.non_empty".into());
            }
            for value in object.values() {
                collect_contract_features(value, features);
            }
        }
        _ => {}
    }
}

async fn run_case(name: &str, fixture_path: &Path) -> CaseResult<()> {
    let fixture: Fixture = serde_json::from_slice(
        &fs::read(fixture_path)
            .map_err(|error| format!("read {}: {error}", fixture_path.display()))?,
    )
    .map_err(|error| format!("decode {}: {error}", fixture_path.display()))?;
    let directory = fixture_path
        .parent()
        .ok_or_else(|| format!("fixture {} has no parent", fixture_path.display()))?;
    let production = fixture_db(directory, name, "production").await?;
    let reference = match fixture_db(directory, name, "reference").await {
        Ok(reference) => reference,
        Err(error) => {
            let _ = production.store.close().await;
            return Err(error);
        }
    };

    let result = execute_case(&fixture, &production.engine, &reference.engine).await;
    let production_close = production
        .store
        .close()
        .await
        .map_err(|error| format!("close production store: {error}"));
    let reference_close = reference
        .store
        .close()
        .await
        .map_err(|error| format!("close reference store: {error}"));
    result?;
    production_close?;
    reference_close
}

async fn execute_case(
    fixture: &Fixture,
    production_engine: &Engine,
    reference_engine: &Engine,
) -> CaseResult<()> {
    let prepared = serde_json::from_value::<pir::Program>(fixture.program.clone())
        .map_err(|error| ObservedError::invalid(format!("protocol: invalid PIR program: {error}")))
        .and_then(|program| rad::protocol::lower_pir(program).map_err(ObservedError::lowering));
    let (production, reference) = match prepared {
        Ok(program) => {
            let production = production_engine
                .execute_program(program.clone(), CatalogPolicy::Forbidden)
                .await
                .map_err(|error| ObservedError::execution(&error));
            let reference = reference_engine
                .execute_program_reference_with_options(program, ProgramOptions::default())
                .await
                .map_err(|error| ObservedError::execution(&error));
            (production, reference)
        }
        Err(error) => (Err(error.clone()), Err(error)),
    };
    check_program_outcome(fixture, production, reference)?;

    for assertion in &fixture.assertions {
        let wire = serde_json::from_value::<lir_wire::Query>(assertion.query.clone())
            .map_err(|error| format!("assertion {:?}: decode LIR: {error}", assertion.name))?;
        let query = rad::protocol::lower_lir(wire)
            .map_err(|error| format!("assertion {:?}: lower LIR: {error}", assertion.name))?;
        let production = production_engine
            .execute(query.clone())
            .await
            .map_err(|error| format!("assertion {:?}: production: {error}", assertion.name))?;
        let reference = reference_engine
            .execute_reference(query)
            .await
            .map_err(|error| format!("assertion {:?}: reference: {error}", assertion.name))?;
        if production != reference {
            return Err(format!(
                "assertion {:?}: production/reference mismatch\nproduction: {production:?}\n reference: {reference:?}",
                assertion.name
            ));
        }
        let actual = result_json::datum(&production)
            .map_err(|error| format!("assertion {:?}: encode result: {error}", assertion.name))?;
        if actual != assertion.expect {
            return Err(format!(
                "assertion {:?}: shared expected-result mismatch\nactual: {}\n  want: {}",
                assertion.name, actual, assertion.expect
            ));
        }
    }
    Ok(())
}

fn check_program_outcome(
    fixture: &Fixture,
    production: Result<exec::ProgramResult, ObservedError>,
    reference: Result<exec::ProgramResult, ObservedError>,
) -> CaseResult<()> {
    match (&fixture.error, production, reference) {
        (Some(expectation), Err(production), Err(reference)) => {
            validate_error("production", expectation, &production)?;
            validate_error("reference", expectation, &reference)?;
            if production.code != reference.code || production.reason != reference.reason {
                return Err(format!(
                    "production/reference error mismatch\nproduction: {production:?}\n reference: {reference:?}"
                ));
            }
            Ok(())
        }
        (Some(_), production, reference) => Err(format!(
            "expected program failure\nproduction: {}\n reference: {}",
            outcome_summary(&production),
            outcome_summary(&reference)
        )),
        (None, Ok(production), Ok(reference)) => {
            if production.result != reference.result {
                return Err(format!(
                    "production/reference result mismatch\nproduction: {:?}\n reference: {:?}",
                    production.result, reference.result
                ));
            }
            if production.statements != reference.statements {
                return Err(format!(
                    "production/reference statement-summary mismatch\nproduction: {:?}\n reference: {:?}",
                    production.statements, reference.statements
                ));
            }
            if let ExpectedResult::Present(expected) = &fixture.result {
                let actual = result_json::datum(&production.result)
                    .map_err(|error| format!("encode program result: {error}"))?;
                if &actual != expected {
                    return Err(format!(
                        "shared expected-result mismatch\nactual: {actual}\n  want: {expected}"
                    ));
                }
            }
            Ok(())
        }
        (None, production, reference) => Err(format!(
            "unexpected program failure\nproduction: {}\n reference: {}",
            outcome_summary(&production),
            outcome_summary(&reference)
        )),
    }
}

fn validate_error(
    label: &str,
    expectation: &ErrorExpectation,
    observed: &ObservedError,
) -> CaseResult<()> {
    if !expectation.code().is_empty() && observed.code != expectation.code() {
        return Err(format!(
            "{label} error code {:?}, want {:?}: {}",
            observed.code,
            expectation.code(),
            observed.detail
        ));
    }
    if !expectation.reason().is_empty() && observed.reason != Some(expectation.reason()) {
        return Err(format!(
            "{label} error reason {:?}, want {:?}: {}",
            observed.reason,
            expectation.reason(),
            observed.detail
        ));
    }
    if !expectation.contains().is_empty() && !observed.detail.contains(expectation.contains()) {
        return Err(format!(
            "{label} error detail {:?} does not contain {:?}",
            observed.detail,
            expectation.contains()
        ));
    }
    Ok(())
}

fn outcome_summary(outcome: &Result<exec::ProgramResult, ObservedError>) -> String {
    match outcome {
        Ok(result) => format!("success ({:?})", result.result),
        Err(error) => format!("{error:?}"),
    }
}

fn discover_cases() -> CaseResult<Vec<(String, PathBuf)>> {
    let root = corpus_root();
    let mut cases = Vec::new();
    for entry in fs::read_dir(&root).map_err(|error| format!("read {}: {error}", root.display()))? {
        let entry = entry.map_err(|error| format!("read {} entry: {error}", root.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("inspect {}: {error}", entry.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }
        let mut fixture_paths = fs::read_dir(entry.path())
            .map_err(|error| format!("read {}: {error}", entry.path().display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("test_") && name.ends_with(".json"))
            })
            .collect::<Vec<_>>();
        fixture_paths.sort();
        if fixture_paths.is_empty() {
            continue;
        }
        if fixture_paths.len() != 1 {
            return Err(format!(
                "{} contains {} test_*.json files, want exactly one",
                entry.path().display(),
                fixture_paths.len()
            ));
        }
        cases.push((
            entry.file_name().to_string_lossy().into_owned(),
            fixture_paths.remove(0),
        ));
    }
    cases.sort_by(|left, right| left.0.cmp(&right.0));
    if cases.is_empty() {
        return Err(format!("no fixtures found under {}", root.display()));
    }
    Ok(cases)
}

async fn fixture_db(directory: &Path, name: &str, role: &str) -> CaseResult<FixtureDb> {
    let store = Arc::new(
        Store::memory(&format!("rust-differential-{name}-{role}"))
            .await
            .map_err(|error| format!("open {role} store: {error}"))?,
    );
    let engine = Engine::new(store.clone());
    if let Err(error) = install_schema_and_seed(directory, store.clone(), &engine).await {
        let _ = store.close().await;
        return Err(format!("{role} setup: {error}"));
    }
    Ok(FixtureDb { engine, store })
}

async fn install_schema_and_seed(
    directory: &Path,
    store: Arc<Store>,
    engine: &Engine,
) -> CaseResult<()> {
    let schema_path = directory.join("rad.schema.yaml");
    let parsed = catalog::schema::parse(
        schema_path
            .to_str()
            .ok_or_else(|| format!("fixture path {} is not UTF-8", schema_path.display()))?,
        &fs::read(&schema_path)
            .map_err(|error| format!("read {}: {error}", schema_path.display()))?,
    )
    .map_err(|error| format!("parse {}: {error}", schema_path.display()))?;
    let catalog = catalog::Catalog::new(store);
    for table in parsed.tables {
        catalog
            .create_table(table.def)
            .await
            .map_err(|error| format!("create fixture table: {error}"))?;
    }

    let seed_path = directory.join("seed.json");
    if !seed_path.exists() {
        return Ok(());
    }
    let groups: Vec<SeedGroup> = serde_json::from_slice(
        &fs::read(&seed_path).map_err(|error| format!("read {}: {error}", seed_path.display()))?,
    )
    .map_err(|error| format!("decode {}: {error}", seed_path.display()))?;
    for group in groups {
        let table = catalog
            .get_table(&group.table)
            .await
            .map_err(|error| format!("get seed table {:?}: {error}", group.table))?
            .ok_or_else(|| format!("seed table {:?} does not exist", group.table))?;
        let rows = group
            .rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|(name, value)| {
                        let column = table.column(&name).ok_or_else(|| {
                            format!("seed column {:?}.{:?} does not exist", table.name, name)
                        })?;
                        Ok((name, scalar(value, column.scalar_type)?))
                    })
                    .collect::<CaseResult<Row>>()
            })
            .collect::<CaseResult<Vec<_>>>()?;
        engine
            .create_many(&group.table, rows)
            .await
            .map_err(|error| format!("seed {:?}: {error}", group.table))?;
    }
    Ok(())
}

fn scalar(value: serde_json::Value, scalar_type: ScalarType) -> CaseResult<Value> {
    match (value, scalar_type) {
        (serde_json::Value::Null, scalar_type) => Ok(Value::Null(scalar_type)),
        (serde_json::Value::String(value), ScalarType::Text) => Ok(Value::Text(value)),
        (serde_json::Value::Number(value), ScalarType::Int64) => value
            .as_i64()
            .map(Value::Int64)
            .ok_or_else(|| format!("fixture int64 {value} is out of range")),
        (serde_json::Value::Number(value), ScalarType::Float64) => value
            .as_f64()
            .map(Value::Float64)
            .ok_or_else(|| format!("fixture float64 {value} is not finite")),
        (serde_json::Value::Bool(value), ScalarType::Bool) => Ok(Value::Bool(value)),
        (value, scalar_type) => Err(format!("fixture value {value} is not a {scalar_type:?}")),
    }
}

fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/e2e")
}
