use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rad::engine::catalog;
use rad::engine::catalog::identity::SchemaId;
use rad::engine::catalog::model::{ColumnDef, IndexDef, ScalarType, TableDef};
use rad::engine::exec::{
    CatalogPolicy, Engine, ErrorKind, ErrorReason, Program, ProgramOptions, Statement,
};
use rad::engine::kv::TransactionalKv;
use rad::engine::kv::slatedb::Store;
use rad::engine::lir::{
    Expr, Kind, Literal, OrderTerm, ProjectField, Query, RawScalar, Relation, RootCardinality, Row,
    RowsColumn, Value,
};
use rad::service::result_json;

use super::{Choices, TestResult, decisions_from_seed};

static STORE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct ProgramCase {
    pub decisions: Vec<u64>,
    table: TableDef,
    initial: Vec<Row>,
    program: Program,
    expected_rows: Vec<Row>,
    expected_statements: Vec<(String, usize)>,
    expected_error: Option<(ErrorKind, ErrorReason)>,
}

#[derive(Clone, Debug, PartialEq)]
struct Observation {
    outcome: ProgramOutcome,
    rows: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq)]
enum ProgramOutcome {
    Value(serde_json::Value),
    Error(ErrorKind, ErrorReason, String),
}

impl ProgramCase {
    pub fn from_seed(seed: u64) -> Self {
        Self::generate(decisions_from_seed(seed))
    }

    pub fn generate(decisions: Vec<u64>) -> Self {
        let mut choices = Choices::new(&decisions);
        let table = table();
        let initial_count = choices.range(0, 3);
        let initial = (0..initial_count)
            .map(|index| generated_row(&format!("seed{index}"), &mut choices))
            .collect::<Vec<_>>();
        let create_count = choices.range(1, 3);
        let created = (0..create_count)
            .map(|index| generated_row(&format!("new{index}"), &mut choices))
            .collect::<Vec<_>>();

        let mut expected_rows = initial.clone();
        expected_rows.extend(created.clone());
        let mut statements = vec![Statement::Create {
            name: "created".into(),
            relation: rows_query(&table.columns, &created),
            table: table.name.clone(),
        }];
        let mut expected_statements = vec![("created".into(), created.len())];

        let changed_column = &table.columns[choices.range(1, table.columns.len() - 1)];
        let changed_value = generated_value(
            changed_column.scalar_type,
            changed_column.nullable,
            &mut choices,
        );
        let updated_relation = project_ref(
            "created",
            &["id", &changed_column.name],
            Some((&changed_column.name, changed_value.clone())),
        );
        statements.push(Statement::Update {
            name: "updated".into(),
            relation: updated_relation,
            table: table.name.clone(),
        });
        expected_statements.push(("updated".into(), created.len()));
        for row in expected_rows.iter_mut().filter(|row| is_new(row)) {
            row.insert(changed_column.name.clone(), changed_value.clone());
        }

        if choices.coin() {
            statements.push(Statement::Delete {
                name: "deleted".into(),
                relation: project_ref("updated", &["id"], None),
                table: table.name.clone(),
            });
            expected_statements.push(("deleted".into(), created.len()));
            expected_rows.retain(|row| !is_new(row));
        }

        let fail_late = choices.chance(5);
        let (result, expected_error) = if fail_late {
            let missing = generated_row("missing", &mut choices);
            statements.push(Statement::Update {
                name: "missing".into(),
                relation: rows_query(&table.columns, &[missing]),
                table: table.name.clone(),
            });
            expected_rows = initial.clone();
            (
                Some("missing".into()),
                Some((
                    ErrorKind::MutationNotFound,
                    ErrorReason::MutationTargetNotFound,
                )),
            )
        } else {
            statements.push(Statement::Query {
                name: "final".into(),
                relation: scan_query(&table),
            });
            expected_rows.sort_by(|left, right| left["id"].compare(&right["id"]).unwrap());
            expected_statements.push(("final".into(), expected_rows.len()));
            (Some("final".into()), None)
        };

        Self {
            decisions,
            table,
            initial,
            program: Program { statements, result },
            expected_rows,
            expected_statements,
            expected_error,
        }
    }
}

pub async fn check_program(case: &ProgramCase) -> TestResult<()> {
    let production = observe(case, false).await?;
    let reference = observe(case, true).await?;
    if production != reference {
        return Err(format!(
            "PIR production/reference mismatch\nproduction: {production:#?}\nreference: {reference:#?}\nprogram: {:#?}",
            case.program
        ));
    }

    let expected_rows = rows_json(&case.expected_rows)?;
    if production.rows != expected_rows {
        return Err(format!(
            "PIR state-model mismatch\nactual: {}\nexpected: {}\nprogram: {:#?}",
            production.rows, expected_rows, case.program
        ));
    }
    match (&production.outcome, case.expected_error) {
        (ProgramOutcome::Error(kind, reason, _), Some(expected))
            if (*kind, *reason) == expected => {}
        (ProgramOutcome::Value(value), None) => {
            let statements = value["statements"]
                .as_array()
                .ok_or_else(|| "encoded PIR result lacks statement summaries".to_string())?;
            let actual = statements
                .iter()
                .map(|statement| {
                    Ok((
                        statement["name"]
                            .as_str()
                            .ok_or_else(|| "statement name is not text".to_string())?
                            .to_owned(),
                        statement["affected"].as_u64().ok_or_else(|| {
                            "statement affected count is not an integer".to_string()
                        })? as usize,
                    ))
                })
                .collect::<TestResult<Vec<_>>>()?;
            if actual != case.expected_statements {
                return Err(format!(
                    "PIR statement-model mismatch\nactual: {actual:?}\nexpected: {:?}",
                    case.expected_statements
                ));
            }
            if value["result"] != expected_rows {
                return Err(format!(
                    "PIR selected-result mismatch\nactual: {}\nexpected: {}",
                    value["result"], expected_rows
                ));
            }
        }
        (actual, expected) => {
            return Err(format!(
                "PIR outcome-model mismatch\nactual: {actual:?}\nexpected error: {expected:?}\nprogram: {:#?}",
                case.program
            ));
        }
    }
    Ok(())
}

/// Exercise one-edit-near-valid PIR programs against both executors. Every
/// rejection is checked by semantic identity, and every case scans the table
/// afterward to prove that preflight and late runtime failures preserve
/// whole-program atomicity.
pub async fn check_invalid_program(case: &ProgramCase) -> TestResult<()> {
    let mut initial = case.initial.clone();
    initial.sort_by(|left, right| left["id"].compare(&right["id"]).unwrap());
    let expected_rows = rows_json(&initial)?;

    for variant in invalid_program_variants(case) {
        let mut candidate = case.clone();
        candidate.program = variant.program;
        let production = observe(&candidate, false).await?;
        let reference = observe(&candidate, true).await?;
        if production != reference {
            return Err(format!(
                "invalid PIR production/reference mismatch for {}\nproduction: {production:#?}\nreference: {reference:#?}\nprogram: {:#?}",
                variant.name, candidate.program
            ));
        }
        if production.rows != expected_rows {
            return Err(format!(
                "invalid PIR changed state for {}\nactual: {}\nexpected: {}\nprogram: {:#?}",
                variant.name, production.rows, expected_rows, candidate.program
            ));
        }
        match production.outcome {
            ProgramOutcome::Error(kind, reason, _) if (kind, reason) == variant.expected => {}
            actual => {
                return Err(format!(
                    "invalid PIR outcome mismatch for {}\nactual: {actual:?}\nexpected: {:?}\nprogram: {:#?}",
                    variant.name, variant.expected, candidate.program
                ));
            }
        }
    }
    Ok(())
}

struct InvalidProgramVariant {
    name: &'static str,
    program: Program,
    expected: (ErrorKind, ErrorReason),
}

fn invalid_program_variants(case: &ProgramCase) -> Vec<InvalidProgramVariant> {
    let valid = deterministic_row("invalid-target");
    let missing = deterministic_row("missing-target");
    let full_columns = case.table.columns.clone();
    let id = columns_named(&case.table, &["id"]);
    let id_counter = columns_named(&case.table, &["id", "counter"]);
    let without_score = columns_named(&case.table, &["id", "counter", "label", "active"]);
    let counter_only = columns_named(&case.table, &["counter"]);

    let query = |name: &str, relation: Query| Statement::Query {
        name: name.into(),
        relation,
    };
    let create = |name: &str, relation: Query, table: &str| Statement::Create {
        name: name.into(),
        relation,
        table: table.into(),
    };
    let update = |name: &str, relation: Query, table: &str| Statement::Update {
        name: name.into(),
        relation,
        table: table.into(),
    };
    let delete = |name: &str, relation: Query, table: &str| Statement::Delete {
        name: name.into(),
        relation,
        table: table.into(),
    };
    let program = |statements, result: Option<&str>| Program {
        statements,
        result: result.map(str::to_owned),
    };
    let staged_create = || {
        create(
            "staged",
            rows_query(&full_columns, std::slice::from_ref(&valid)),
            &case.table.name,
        )
    };

    vec![
        InvalidProgramVariant {
            name: "empty program",
            program: program(Vec::new(), None),
            expected: (ErrorKind::InvalidInput, ErrorReason::Invalid),
        },
        InvalidProgramVariant {
            name: "empty statement name",
            program: program(vec![query("", scan_query(&case.table))], Some("")),
            expected: (ErrorKind::InvalidInput, ErrorReason::Invalid),
        },
        InvalidProgramVariant {
            name: "duplicate statement name",
            program: program(
                vec![
                    query("duplicate", scan_query(&case.table)),
                    query("duplicate", scan_query(&case.table)),
                ],
                Some("duplicate"),
            ),
            expected: (ErrorKind::InvalidInput, ErrorReason::Invalid),
        },
        InvalidProgramVariant {
            name: "unknown selected result",
            program: program(
                vec![query("known", scan_query(&case.table))],
                Some("unknown"),
            ),
            expected: (ErrorKind::InvalidInput, ErrorReason::Invalid),
        },
        InvalidProgramVariant {
            name: "implicit result with multiple statements",
            program: program(
                vec![
                    query("first", scan_query(&case.table)),
                    query("second", scan_query(&case.table)),
                ],
                None,
            ),
            expected: (ErrorKind::InvalidInput, ErrorReason::Invalid),
        },
        InvalidProgramVariant {
            name: "unknown statement binding",
            program: program(
                vec![query("consumer", project_ref("missing", &["id"], None))],
                Some("consumer"),
            ),
            expected: (ErrorKind::InvalidInput, ErrorReason::UnknownBinding),
        },
        InvalidProgramVariant {
            name: "unknown mutation table",
            program: program(
                vec![create(
                    "created",
                    rows_query(&full_columns, std::slice::from_ref(&valid)),
                    "missing_table",
                )],
                Some("created"),
            ),
            expected: (ErrorKind::InvalidInput, ErrorReason::UnknownTable),
        },
        InvalidProgramVariant {
            name: "create omits required column",
            program: program(
                vec![create(
                    "created",
                    rows_query(&without_score, std::slice::from_ref(&valid)),
                    &case.table.name,
                )],
                Some("created"),
            ),
            expected: (ErrorKind::InvalidInput, ErrorReason::TypeMismatch),
        },
        InvalidProgramVariant {
            name: "update omits primary key",
            program: program(
                vec![update(
                    "updated",
                    rows_query(&counter_only, std::slice::from_ref(&valid)),
                    &case.table.name,
                )],
                Some("updated"),
            ),
            expected: (ErrorKind::InvalidInput, ErrorReason::TypeMismatch),
        },
        InvalidProgramVariant {
            name: "update assigns no columns",
            program: program(
                vec![update(
                    "updated",
                    rows_query(&id, std::slice::from_ref(&valid)),
                    &case.table.name,
                )],
                Some("updated"),
            ),
            expected: (ErrorKind::InvalidInput, ErrorReason::TypeMismatch),
        },
        InvalidProgramVariant {
            name: "delete includes a non-key column",
            program: program(
                vec![delete(
                    "deleted",
                    rows_query(&id_counter, std::slice::from_ref(&valid)),
                    &case.table.name,
                )],
                Some("deleted"),
            ),
            expected: (ErrorKind::InvalidInput, ErrorReason::TypeMismatch),
        },
        InvalidProgramVariant {
            name: "delete substitutes a non-key column for its key",
            program: program(
                vec![delete(
                    "deleted",
                    rows_query(&counter_only, std::slice::from_ref(&valid)),
                    &case.table.name,
                )],
                Some("deleted"),
            ),
            expected: (ErrorKind::InvalidInput, ErrorReason::TypeMismatch),
        },
        InvalidProgramVariant {
            name: "mutation references unknown column",
            program: program(
                vec![update(
                    "updated",
                    rows_query_with_columns(
                        vec![
                            RowsColumn {
                                name: "id".into(),
                                kind: Kind::Text,
                                nullable: false,
                            },
                            RowsColumn {
                                name: "unknown".into(),
                                kind: Kind::Text,
                                nullable: false,
                            },
                        ],
                        vec![vec![
                            RawScalar::Text("invalid-target".into()),
                            RawScalar::Text("x".into()),
                        ]],
                    ),
                    &case.table.name,
                )],
                Some("updated"),
            ),
            expected: (ErrorKind::InvalidInput, ErrorReason::UnknownColumn),
        },
        InvalidProgramVariant {
            name: "mutation column has wrong type",
            program: program(
                vec![update(
                    "updated",
                    rows_query_with_columns(
                        vec![
                            RowsColumn {
                                name: "id".into(),
                                kind: Kind::Text,
                                nullable: false,
                            },
                            RowsColumn {
                                name: "counter".into(),
                                kind: Kind::Text,
                                nullable: false,
                            },
                        ],
                        vec![vec![
                            RawScalar::Text("invalid-target".into()),
                            RawScalar::Text("wrong".into()),
                        ]],
                    ),
                    &case.table.name,
                )],
                Some("updated"),
            ),
            expected: (ErrorKind::InvalidInput, ErrorReason::TypeMismatch),
        },
        InvalidProgramVariant {
            name: "nullable mutation input targets non-null column",
            program: program(
                vec![update(
                    "updated",
                    rows_query_with_columns(
                        vec![
                            RowsColumn {
                                name: "id".into(),
                                kind: Kind::Text,
                                nullable: false,
                            },
                            RowsColumn {
                                name: "counter".into(),
                                kind: Kind::Int64,
                                nullable: true,
                            },
                        ],
                        vec![vec![
                            RawScalar::Text("invalid-target".into()),
                            RawScalar::Number("1".into()),
                        ]],
                    ),
                    &case.table.name,
                )],
                Some("updated"),
            ),
            expected: (ErrorKind::InvalidInput, ErrorReason::TypeMismatch),
        },
        InvalidProgramVariant {
            name: "late update target missing",
            program: program(
                vec![
                    staged_create(),
                    update(
                        "missing",
                        rows_query(&id_counter, std::slice::from_ref(&missing)),
                        &case.table.name,
                    ),
                ],
                Some("missing"),
            ),
            expected: (
                ErrorKind::MutationNotFound,
                ErrorReason::MutationTargetNotFound,
            ),
        },
        InvalidProgramVariant {
            name: "late update identifies one target twice",
            program: program(
                vec![
                    staged_create(),
                    update(
                        "ambiguous",
                        rows_query(&id_counter, &[valid.clone(), valid.clone()]),
                        &case.table.name,
                    ),
                ],
                Some("ambiguous"),
            ),
            expected: (
                ErrorKind::MutationAmbiguous,
                ErrorReason::MutationTargetAmbiguous,
            ),
        },
        InvalidProgramVariant {
            name: "late delete identifies one target twice",
            program: program(
                vec![
                    staged_create(),
                    delete(
                        "ambiguous",
                        rows_query(&id, &[valid.clone(), valid.clone()]),
                        &case.table.name,
                    ),
                ],
                Some("ambiguous"),
            ),
            expected: (
                ErrorKind::MutationAmbiguous,
                ErrorReason::MutationTargetAmbiguous,
            ),
        },
        InvalidProgramVariant {
            name: "late create repeats a primary key",
            program: program(
                vec![
                    staged_create(),
                    create(
                        "duplicate",
                        rows_query(&full_columns, &[missing.clone(), missing]),
                        &case.table.name,
                    ),
                ],
                Some("duplicate"),
            ),
            expected: (
                ErrorKind::ConstraintViolation,
                ErrorReason::ConstraintViolation,
            ),
        },
    ]
}

async fn observe(case: &ProgramCase, reference: bool) -> TestResult<Observation> {
    let sequence = STORE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let store = Arc::new(
        Store::memory(&format!("program-generative-{sequence}"))
            .await
            .map_err(|error| error.to_string())?,
    );
    let engine = Engine::new(store.clone());
    let catalog = catalog::Catalog::new(store.clone());
    catalog
        .create_table(case.table.clone())
        .await
        .map_err(|error| format!("create generated PIR table: {error}"))?;
    if !case.initial.is_empty() {
        engine
            .create_many(&case.table.name, case.initial.clone())
            .await
            .map_err(|error| format!("seed generated PIR table: {error}"))?;
    }

    let options = ProgramOptions {
        catalog: CatalogPolicy::Forbidden,
        ..ProgramOptions::default()
    };
    let result = if reference {
        engine
            .execute_program_reference_with_options(case.program.clone(), options)
            .await
    } else {
        engine
            .execute_program_with_options(case.program.clone(), options)
            .await
    };
    let outcome = match result {
        Ok(result) => ProgramOutcome::Value(
            serde_json::to_value(result_json::program(&result).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?,
        ),
        Err(error) => ProgramOutcome::Error(error.kind(), error.reason(), error.to_string()),
    };
    let rows = result_json::datum(
        &engine
            .execute(scan_query(&case.table))
            .await
            .map_err(|error| format!("read generated PIR post-state: {error}"))?,
    )
    .map_err(|error| error.to_string())?;
    store.close().await.map_err(|error| error.to_string())?;
    Ok(Observation { outcome, rows })
}

fn table() -> TableDef {
    let columns = vec![
        column(1, "id", ScalarType::Text, false),
        column(2, "counter", ScalarType::Int64, false),
        column(3, "label", ScalarType::Text, true),
        column(4, "score", ScalarType::Float64, false),
        column(5, "active", ScalarType::Bool, false),
    ];
    TableDef {
        id: schema_id(1),
        name: "items".into(),
        columns,
        primary_key: vec!["id".into()],
        indexes: vec![IndexDef {
            name: "items_counter".into(),
            columns: vec!["counter".into()],
            unique: false,
        }],
        foreign_keys: Vec::new(),
    }
}

fn column(id: u32, name: &str, scalar_type: ScalarType, nullable: bool) -> ColumnDef {
    ColumnDef {
        id: schema_id(id),
        name: name.into(),
        scalar_type,
        nullable,
        format: String::new(),
        default: None,
    }
}

fn schema_id(value: u32) -> SchemaId {
    SchemaId::new(value).expect("generated schema IDs are positive")
}

fn generated_row(id: &str, choices: &mut Choices<'_>) -> Row {
    Row::from([
        ("id".into(), Value::Text(id.into())),
        (
            "counter".into(),
            generated_value(ScalarType::Int64, false, choices),
        ),
        (
            "label".into(),
            generated_value(ScalarType::Text, true, choices),
        ),
        (
            "score".into(),
            generated_value(ScalarType::Float64, false, choices),
        ),
        (
            "active".into(),
            generated_value(ScalarType::Bool, false, choices),
        ),
    ])
}

fn deterministic_row(id: &str) -> Row {
    Row::from([
        ("id".into(), Value::Text(id.into())),
        ("counter".into(), Value::Int64(1)),
        ("label".into(), Value::Text("generated".into())),
        ("score".into(), Value::Float64(1.5)),
        ("active".into(), Value::Bool(true)),
    ])
}

fn columns_named(table: &TableDef, names: &[&str]) -> Vec<ColumnDef> {
    names
        .iter()
        .map(|name| {
            table
                .columns
                .iter()
                .find(|column| column.name == *name)
                .unwrap_or_else(|| panic!("generated table lacks column {name:?}"))
                .clone()
        })
        .collect()
}

fn is_new(row: &Row) -> bool {
    matches!(row.get("id"), Some(Value::Text(id)) if id.starts_with("new"))
}

fn generated_value(scalar_type: ScalarType, nullable: bool, choices: &mut Choices<'_>) -> Value {
    if nullable && choices.chance(4) {
        return Value::Null(scalar_type);
    }
    match scalar_type {
        ScalarType::Text => Value::Text(["", "a", "rad", "z"][choices.index(4)].into()),
        ScalarType::Int64 => Value::Int64([-2, -1, 0, 1, 2][choices.index(5)]),
        ScalarType::Float64 => Value::Float64([-1.5, -0.0, 0.0, 1.5, 2.5][choices.index(5)]),
        ScalarType::Bool => Value::Bool(choices.coin()),
    }
}

fn rows_query(columns: &[ColumnDef], rows: &[Row]) -> Query {
    rows_query_with_columns(
        columns
            .iter()
            .map(|column| RowsColumn {
                name: column.name.clone(),
                kind: scalar_kind(column.scalar_type),
                nullable: column.nullable,
            })
            .collect(),
        rows.iter()
            .map(|row| {
                columns
                    .iter()
                    .map(|column| raw(&row[&column.name]))
                    .collect()
            })
            .collect(),
    )
}

fn rows_query_with_columns(columns: Vec<RowsColumn>, values: Vec<Vec<RawScalar>>) -> Query {
    Query {
        root: Relation::Rows {
            scope: "input".into(),
            columns,
            values,
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn project_ref(binding: &str, names: &[&str], replacement: Option<(&str, Value)>) -> Query {
    let source = "source";
    Query {
        root: Relation::Project {
            input: Box::new(Relation::Ref {
                binding: binding.into(),
                scope: source.into(),
            }),
            scope: Some("input".into()),
            spread: Vec::new(),
            fields: names
                .iter()
                .map(|name| ProjectField {
                    name: (*name).into(),
                    expression: match &replacement {
                        Some((target, value)) if name == target => literal(value),
                        _ => Expr::Column {
                            scope: source.into(),
                            name: (*name).into(),
                        },
                    },
                })
                .collect(),
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn scan_query(table: &TableDef) -> Query {
    let scope = "stored";
    Query {
        root: Relation::Order {
            input: Box::new(Relation::Scan {
                table: table.name.clone(),
                scope: scope.into(),
            }),
            terms: vec![OrderTerm {
                expression: Expr::Column {
                    scope: scope.into(),
                    name: "id".into(),
                },
                descending: false,
            }],
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn scalar_kind(value: ScalarType) -> Kind {
    match value {
        ScalarType::Text => Kind::Text,
        ScalarType::Int64 => Kind::Int64,
        ScalarType::Float64 => Kind::Float64,
        ScalarType::Bool => Kind::Bool,
    }
}

fn raw(value: &Value) -> RawScalar {
    match value {
        Value::Null(_) => RawScalar::Null,
        Value::Text(value) => RawScalar::Text(value.clone()),
        Value::Int64(value) => RawScalar::Number(value.to_string()),
        Value::Float64(value) => RawScalar::Number(value.to_string()),
        Value::Bool(value) => RawScalar::Bool(*value),
    }
}

fn literal(value: &Value) -> Expr {
    Expr::Literal(Literal {
        raw: raw(value),
        kind: Some(match value {
            Value::Null(kind) => scalar_kind(*kind),
            Value::Text(_) => Kind::Text,
            Value::Int64(_) => Kind::Int64,
            Value::Float64(_) => Kind::Float64,
            Value::Bool(_) => Kind::Bool,
        }),
    })
}

fn rows_json(rows: &[Row]) -> TestResult<serde_json::Value> {
    rows.iter()
        .map(|row| result_json::row(row).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()
        .map(serde_json::Value::Array)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn generated_programs_cover_commit_and_rollback_models() {
        let mut saw_commit = false;
        let mut saw_rollback = false;
        for seed in 0..16 {
            let case = ProgramCase::from_seed(seed);
            saw_commit |= case.expected_error.is_none();
            saw_rollback |= case.expected_error.is_some();
            check_program(&case)
                .await
                .unwrap_or_else(|error| panic!("seed {seed}: {error}"));
        }
        assert!(saw_commit && saw_rollback);
    }

    #[tokio::test]
    async fn generated_invalid_programs_cover_shape_reason_and_atomicity_contracts() {
        for seed in 0..1 {
            let case = ProgramCase::from_seed(seed);
            check_invalid_program(&case)
                .await
                .unwrap_or_else(|error| panic!("seed {seed}: {error}"));
        }
    }
}
