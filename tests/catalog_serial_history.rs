//! Independent bounded serial-history oracle for overlapping catalog, data,
//! and online-schema operations.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rad::engine::catalog::Catalog;
use rad::engine::catalog::identity::{SchemaId, TransitionId};
use rad::engine::catalog::model::{
    ColumnConversion, ColumnDef, ColumnDraft, ColumnReplacementDef, ConstraintDef, ConstraintKind,
    IndexDef, ScalarType, TableDef, TransitionState,
};
use rad::engine::exec::{CatalogPolicy, Engine, ErrorKind, Program, Statement};
use rad::engine::kv::slatedb::Store;
use rad::engine::lir::{Datum, Expr, OrderTerm, Relation, RootCardinality, Row, Value};

#[derive(Clone, Debug, Eq, PartialEq)]
struct LogicalState {
    table_name: String,
    columns: BTreeSet<String>,
    rows: BTreeMap<i64, String>,
    build_started: bool,
    index_ready: bool,
    value_type: ScalarType,
    value_nullable: bool,
    replacement_started: bool,
    replacement_ready: bool,
    constraint_started: bool,
    constraint_ready: bool,
}

#[derive(Clone, Debug)]
enum OperationKind {
    RenameTable(String),
    AddColumn(String),
    DeleteColumn(String),
    PutRow(i64, String),
    StartBuild,
    PublishBuild,
    StartReplacement,
    PublishReplacement(ScalarType),
    StartConstraint,
    PublishConstraint,
}

#[derive(Clone, Debug)]
struct Operation {
    id: &'static str,
    invoke: u64,
    complete: u64,
    success: bool,
    kind: OperationKind,
}

fn apply(state: &LogicalState, operation: &Operation) -> Option<LogicalState> {
    let mut next = state.clone();
    if !operation.success {
        return Some(next);
    }
    match &operation.kind {
        OperationKind::RenameTable(name) => next.table_name.clone_from(name),
        OperationKind::AddColumn(name) => {
            if !next.columns.insert(name.clone()) {
                return None;
            }
        }
        OperationKind::DeleteColumn(name) => {
            if !next.columns.remove(name) {
                return None;
            }
        }
        OperationKind::PutRow(id, value) => {
            if value == "<null>" && (next.constraint_started || !next.value_nullable) {
                return None;
            }
            if next.rows.insert(*id, value.clone()).is_some() {
                return None;
            }
        }
        OperationKind::StartBuild => {
            if next.build_started {
                return None;
            }
            next.build_started = true;
        }
        OperationKind::PublishBuild => {
            if !next.build_started || next.index_ready {
                return None;
            }
            next.index_ready = true;
        }
        OperationKind::StartReplacement => {
            if next.replacement_started || next.replacement_ready {
                return None;
            }
            next.replacement_started = true;
        }
        OperationKind::PublishReplacement(scalar_type) => {
            if !next.replacement_started || next.replacement_ready {
                return None;
            }
            next.replacement_ready = true;
            next.value_type = *scalar_type;
        }
        OperationKind::StartConstraint => {
            if next.constraint_started || next.constraint_ready || !next.value_nullable {
                return None;
            }
            next.constraint_started = true;
        }
        OperationKind::PublishConstraint => {
            if !next.constraint_started || next.constraint_ready {
                return None;
            }
            next.constraint_ready = true;
            next.value_nullable = false;
        }
    }
    Some(next)
}

fn bounded_serial_order(
    initial: &LogicalState,
    history: &[Operation],
    final_state: &LogicalState,
) -> Option<Vec<&'static str>> {
    fn search(
        state: LogicalState,
        history: &[Operation],
        remaining: &mut BTreeSet<usize>,
        order: &mut Vec<&'static str>,
        final_state: &LogicalState,
    ) -> bool {
        if remaining.is_empty() {
            return state == *final_state;
        }
        let candidates = remaining.iter().copied().collect::<Vec<_>>();
        for candidate in candidates {
            let operation = &history[candidate];
            let has_unfinished_predecessor = remaining
                .iter()
                .copied()
                .any(|other| other != candidate && history[other].complete < operation.invoke);
            if has_unfinished_predecessor {
                continue;
            }
            let Some(next) = apply(&state, operation) else {
                continue;
            };
            remaining.remove(&candidate);
            order.push(operation.id);
            if search(next, history, remaining, order, final_state) {
                return true;
            }
            order.pop();
            remaining.insert(candidate);
        }
        false
    }

    let mut remaining = (0..history.len()).collect::<BTreeSet<_>>();
    let mut order = Vec::with_capacity(history.len());
    search(
        initial.clone(),
        history,
        &mut remaining,
        &mut order,
        final_state,
    )
    .then_some(order)
}

fn id(value: u32) -> SchemaId {
    SchemaId::new(value).unwrap()
}

fn table_definition() -> TableDef {
    TableDef {
        id: id(51_000),
        name: "oracle_items".into(),
        columns: vec![
            ColumnDef {
                id: id(1),
                name: "id".into(),
                scalar_type: ScalarType::Int64,
                nullable: false,
                format: String::new(),
                default: None,
            },
            ColumnDef {
                id: id(2),
                name: "name".into(),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                default: None,
            },
        ],
        primary_key: vec!["id".into()],
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
    }
}

async fn execute_catalog_until_committed(engine: &Engine, statement: Statement) {
    for _ in 0..1_000 {
        let result = engine
            .execute_program(
                Program {
                    statements: vec![statement.clone()],
                    result: None,
                },
                CatalogPolicy::RevisionPerStatement,
            )
            .await;
        match result {
            Ok(_) => return,
            Err(error) if error.is_conflict() => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(error) => panic!("catalog operation failed: {error}"),
        }
    }
    panic!("catalog operation exhausted its conflict retry budget");
}

async fn start_transition_until_committed(engine: &Engine, statement: Statement) -> TransitionId {
    for _ in 0..1_000 {
        match engine
            .execute_program(
                Program {
                    statements: vec![statement.clone()],
                    result: None,
                },
                CatalogPolicy::RevisionPerStatement,
            )
            .await
        {
            Ok(result) => {
                return result.statements[0]
                    .control
                    .as_ref()
                    .expect("transition statement returned no control")
                    .transition_id
                    .clone();
            }
            Err(error) if error.is_conflict() => {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(error) => panic!("transition start failed: {error}"),
        }
    }
    panic!("transition start exhausted its conflict retry budget");
}

async fn insert_by_stable_table_id(catalog: &Catalog, engine: &Engine) {
    let mut last_error = String::new();
    for _ in 0..1_000 {
        let table = catalog
            .get_table_by_schema_id(id(51_000))
            .await
            .unwrap()
            .unwrap();
        match engine
            .create(
                &table.name,
                Row::from([
                    ("id".into(), Value::Int64(1)),
                    ("name".into(), Value::Text("one".into())),
                ]),
            )
            .await
        {
            Ok(_) => return,
            Err(error)
                if error.is_conflict()
                    || matches!(error.reason(), rad::engine::exec::ErrorReason::UnknownTable) =>
            {
                last_error = error.to_string();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(error) => panic!("stable-ID insert failed: {error}"),
        }
    }
    panic!("stable-ID insert exhausted its retry budget: {last_error}");
}

async fn finish_transition(engine: &Engine, transition: &TransitionId) {
    let owner = engine.claim_schema_transition(transition).await.unwrap();
    for _ in 0..64 {
        let step = engine
            .step_schema_transition(transition, owner, 1)
            .await
            .unwrap();
        if step.transition.state.is_terminal() {
            assert_eq!(step.transition.state, TransitionState::Ready);
            return;
        }
    }
    panic!("transition did not finish within the bounded test budget");
}

async fn observe_text_rows(engine: &Engine, table: &str) -> BTreeMap<i64, String> {
    let result = engine
        .execute(rad::engine::lir::Query {
            root: Relation::Order {
                input: Box::new(Relation::Scan {
                    table: table.into(),
                    scope: "items".into(),
                }),
                terms: vec![OrderTerm {
                    expression: Expr::Column {
                        scope: "items".into(),
                        name: "id".into(),
                    },
                    descending: false,
                }],
            },
            cardinality: RootCardinality::Many,
            bindings: Default::default(),
        })
        .await
        .unwrap();
    let Datum::Array(rows) = result else {
        panic!("scan result was not an array")
    };
    rows.into_iter()
        .map(|row| {
            let Datum::Object(fields) = row else {
                panic!("scan row was not an object")
            };
            let mut row_id = None;
            let mut name = None;
            for field in fields {
                match (field.name.as_str(), field.datum) {
                    ("id", Datum::Scalar(Value::Int64(value))) => row_id = Some(value),
                    ("name", Datum::Scalar(Value::Text(value))) => name = Some(value),
                    _ => {}
                }
            }
            (
                row_id.expect("row has an int64 id"),
                name.expect("row has a text name"),
            )
        })
        .collect()
}

#[tokio::test]
async fn overlapping_catalog_data_and_schema_work_has_a_legal_serial_history() {
    let store = Arc::new(Store::memory("bounded-serial-history").await.unwrap());
    let catalog = Arc::new(Catalog::new(store.clone()));
    let engine = Arc::new(Engine::new(store));
    catalog.create_table(table_definition()).await.unwrap();

    let initial = LogicalState {
        table_name: "oracle_items".into(),
        columns: ["id".into(), "name".into()].into_iter().collect(),
        rows: BTreeMap::new(),
        build_started: false,
        index_ready: false,
        value_type: ScalarType::Text,
        value_nullable: false,
        replacement_started: false,
        replacement_ready: false,
        constraint_started: false,
        constraint_ready: false,
    };
    let clock = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(4));

    let write = {
        let catalog = catalog.clone();
        let engine = engine.clone();
        let clock = clock.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            let invoke = clock.fetch_add(1, Ordering::SeqCst) + 1;
            insert_by_stable_table_id(&catalog, &engine).await;
            let complete = clock.fetch_add(1, Ordering::SeqCst) + 1;
            Operation {
                id: "write-1",
                invoke,
                complete,
                success: true,
                kind: OperationKind::PutRow(1, "one".into()),
            }
        })
    };
    let rename = {
        let engine = engine.clone();
        let clock = clock.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            let invoke = clock.fetch_add(1, Ordering::SeqCst) + 1;
            execute_catalog_until_committed(
                &engine,
                Statement::RenameTable {
                    name: "rename".into(),
                    table_id: id(51_000),
                    to: "oracle_items_renamed".into(),
                },
            )
            .await;
            let complete = clock.fetch_add(1, Ordering::SeqCst) + 1;
            Operation {
                id: "rename",
                invoke,
                complete,
                success: true,
                kind: OperationKind::RenameTable("oracle_items_renamed".into()),
            }
        })
    };
    let add_column = {
        let engine = engine.clone();
        let clock = clock.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            let invoke = clock.fetch_add(1, Ordering::SeqCst) + 1;
            execute_catalog_until_committed(
                &engine,
                Statement::CreateColumn {
                    name: "add_scratch".into(),
                    table_id: id(51_000),
                    column: ColumnDraft {
                        id: Some(id(3)),
                        name: "scratch".into(),
                        scalar_type: ScalarType::Text,
                        nullable: true,
                        format: String::new(),
                        default: None,
                    },
                },
            )
            .await;
            let complete = clock.fetch_add(1, Ordering::SeqCst) + 1;
            Operation {
                id: "add-scratch",
                invoke,
                complete,
                success: true,
                kind: OperationKind::AddColumn("scratch".into()),
            }
        })
    };
    let start_build = {
        let engine = engine.clone();
        let clock = clock.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            let invoke = clock.fetch_add(1, Ordering::SeqCst) + 1;
            let transition = start_transition_until_committed(
                &engine,
                Statement::StartIndexBuild {
                    name: "start-index".into(),
                    table_id: id(51_000),
                    index: IndexDef {
                        name: "oracle_name_online".into(),
                        columns: vec!["name".into()],
                        unique: false,
                    },
                    prerequisites: Vec::new(),
                    after: Vec::new(),
                },
            )
            .await;
            let complete = clock.fetch_add(1, Ordering::SeqCst) + 1;
            (
                Operation {
                    id: "start-index",
                    invoke,
                    complete,
                    success: true,
                    kind: OperationKind::StartBuild,
                },
                transition,
            )
        })
    };

    let mut history = vec![
        write.await.unwrap(),
        rename.await.unwrap(),
        add_column.await.unwrap(),
    ];
    let (started, transition) = start_build.await.unwrap();
    history.push(started);

    let invoke = clock.fetch_add(1, Ordering::SeqCst) + 1;
    finish_transition(&engine, &transition).await;
    let complete = clock.fetch_add(1, Ordering::SeqCst) + 1;
    history.push(Operation {
        id: "publish-index",
        invoke,
        complete,
        success: true,
        kind: OperationKind::PublishBuild,
    });

    let invoke = clock.fetch_add(1, Ordering::SeqCst) + 1;
    execute_catalog_until_committed(
        &engine,
        Statement::DeleteColumn {
            name: "delete_scratch".into(),
            table_id: id(51_000),
            column_id: id(3),
        },
    )
    .await;
    let complete = clock.fetch_add(1, Ordering::SeqCst) + 1;
    history.push(Operation {
        id: "delete-scratch",
        invoke,
        complete,
        success: true,
        kind: OperationKind::DeleteColumn("scratch".into()),
    });

    let table = catalog
        .get_table_by_schema_id(id(51_000))
        .await
        .unwrap()
        .unwrap();
    let rows = observe_text_rows(&engine, &table.name).await;
    let final_state = LogicalState {
        table_name: table.name.clone(),
        columns: table
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect(),
        rows,
        build_started: true,
        index_ready: table
            .indexes
            .iter()
            .any(|index| index.name == "oracle_name_online" && index.is_ready()),
        value_type: ScalarType::Text,
        value_nullable: false,
        replacement_started: false,
        replacement_ready: false,
        constraint_started: false,
        constraint_ready: false,
    };

    let order = bounded_serial_order(&initial, &history, &final_state)
        .unwrap_or_else(|| panic!("no legal serialization for {history:#?} -> {final_state:#?}"));
    assert_eq!(order.len(), history.len());
}

#[tokio::test]
async fn replacement_and_constraint_work_has_a_legal_serial_history() {
    let store = Arc::new(Store::memory("bounded-transition-history").await.unwrap());
    let catalog = Arc::new(Catalog::new(store.clone()));
    let engine = Arc::new(Engine::new(store));
    catalog
        .create_table(TableDef {
            id: id(52_000),
            name: "transition_items".into(),
            columns: vec![
                ColumnDef {
                    id: id(1),
                    name: "id".into(),
                    scalar_type: ScalarType::Int64,
                    nullable: false,
                    format: String::new(),
                    default: None,
                },
                ColumnDef {
                    id: id(2),
                    name: "name".into(),
                    scalar_type: ScalarType::Int64,
                    nullable: true,
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
    let initial = LogicalState {
        table_name: "transition_items".into(),
        columns: ["id".into(), "name".into()].into_iter().collect(),
        rows: BTreeMap::new(),
        build_started: false,
        index_ready: false,
        value_type: ScalarType::Int64,
        value_nullable: true,
        replacement_started: false,
        replacement_ready: false,
        constraint_started: false,
        constraint_ready: false,
    };
    let clock = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    let write = {
        let engine = engine.clone();
        let clock = clock.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            let invoke = clock.fetch_add(1, Ordering::SeqCst) + 1;
            for attempt in 0..1_000 {
                match engine
                    .create(
                        "transition_items",
                        Row::from([
                            ("id".into(), Value::Int64(1)),
                            ("name".into(), Value::Int64(1)),
                        ]),
                    )
                    .await
                {
                    Ok(_) => break,
                    Err(error) if error.is_conflict() && attempt < 999 => {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    Err(error) => panic!("transition-history write failed: {error}"),
                }
            }
            let complete = clock.fetch_add(1, Ordering::SeqCst) + 1;
            Operation {
                id: "write-int",
                invoke,
                complete,
                success: true,
                kind: OperationKind::PutRow(1, "1".into()),
            }
        })
    };
    let replacement = {
        let engine = engine.clone();
        let clock = clock.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            let invoke = clock.fetch_add(1, Ordering::SeqCst) + 1;
            let transition = start_transition_until_committed(
                &engine,
                Statement::StartColumnReplacement {
                    name: "replace-name".into(),
                    table_id: id(52_000),
                    column_id: id(2),
                    replacement: ColumnReplacementDef {
                        scalar_type: ScalarType::Text,
                        nullable: true,
                        format: String::new(),
                        default: None,
                        conversion: ColumnConversion::StrictBuiltin,
                        prerequisites: Vec::new(),
                    },
                    after: Vec::new(),
                },
            )
            .await;
            let complete = clock.fetch_add(1, Ordering::SeqCst) + 1;
            (
                Operation {
                    id: "start-replacement",
                    invoke,
                    complete,
                    success: true,
                    kind: OperationKind::StartReplacement,
                },
                transition,
            )
        })
    };

    let mut history = vec![write.await.unwrap()];
    let (replacement_start, replacement_id) = replacement.await.unwrap();
    history.push(replacement_start);
    let invoke = clock.fetch_add(1, Ordering::SeqCst) + 1;
    finish_transition(&engine, &replacement_id).await;
    let complete = clock.fetch_add(1, Ordering::SeqCst) + 1;
    history.push(Operation {
        id: "publish-replacement",
        invoke,
        complete,
        success: true,
        kind: OperationKind::PublishReplacement(ScalarType::Text),
    });

    let invoke = clock.fetch_add(1, Ordering::SeqCst) + 1;
    let constraint_id = start_transition_until_committed(
        &engine,
        Statement::StartConstraintValidation {
            name: "validate-name".into(),
            table_id: id(52_000),
            constraint: ConstraintDef {
                name: "transition_items_name_required".into(),
                kind: ConstraintKind::NotNull,
                column_id: id(2),
                prerequisites: Vec::new(),
            },
            after: Vec::new(),
        },
    )
    .await;
    let complete = clock.fetch_add(1, Ordering::SeqCst) + 1;
    history.push(Operation {
        id: "start-constraint",
        invoke,
        complete,
        success: true,
        kind: OperationKind::StartConstraint,
    });

    let invoke = clock.fetch_add(1, Ordering::SeqCst) + 1;
    let null_error = engine
        .create(
            "transition_items",
            Row::from([("id".into(), Value::Int64(2))]),
        )
        .await
        .unwrap_err();
    let complete = clock.fetch_add(1, Ordering::SeqCst) + 1;
    assert_eq!(null_error.kind(), ErrorKind::ConstraintViolation);
    history.push(Operation {
        id: "write-null",
        invoke,
        complete,
        success: false,
        kind: OperationKind::PutRow(2, "<null>".into()),
    });

    let invoke = clock.fetch_add(1, Ordering::SeqCst) + 1;
    engine
        .create(
            "transition_items",
            Row::from([
                ("id".into(), Value::Int64(2)),
                ("name".into(), Value::Text("2".into())),
            ]),
        )
        .await
        .unwrap();
    let complete = clock.fetch_add(1, Ordering::SeqCst) + 1;
    history.push(Operation {
        id: "write-text",
        invoke,
        complete,
        success: true,
        kind: OperationKind::PutRow(2, "2".into()),
    });

    let invoke = clock.fetch_add(1, Ordering::SeqCst) + 1;
    finish_transition(&engine, &constraint_id).await;
    let complete = clock.fetch_add(1, Ordering::SeqCst) + 1;
    history.push(Operation {
        id: "publish-constraint",
        invoke,
        complete,
        success: true,
        kind: OperationKind::PublishConstraint,
    });

    let table = catalog
        .get_table("transition_items")
        .await
        .unwrap()
        .unwrap();
    let name = table.column("name").unwrap();
    let final_state = LogicalState {
        table_name: table.name.clone(),
        columns: table
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect(),
        rows: observe_text_rows(&engine, &table.name).await,
        build_started: false,
        index_ready: false,
        value_type: name.scalar_type,
        value_nullable: name.nullable,
        replacement_started: true,
        replacement_ready: true,
        constraint_started: true,
        constraint_ready: true,
    };
    let order = bounded_serial_order(&initial, &history, &final_state)
        .unwrap_or_else(|| panic!("no legal transition serialization: {history:#?}"));
    assert_eq!(order.len(), history.len());

    let mut impossible_history = history.clone();
    impossible_history
        .iter_mut()
        .find(|operation| operation.id == "write-null")
        .unwrap()
        .success = true;
    assert!(bounded_serial_order(&initial, &impossible_history, &final_state).is_none());
    let impossible_publication = LogicalState {
        constraint_started: false,
        ..final_state
    };
    assert!(bounded_serial_order(&initial, &history, &impossible_publication).is_none());
}

#[test]
fn bounded_oracle_rejects_impossible_outcomes() {
    let initial = LogicalState {
        table_name: "items".into(),
        columns: ["id".into(), "name".into()].into_iter().collect(),
        rows: BTreeMap::new(),
        build_started: false,
        index_ready: false,
        value_type: ScalarType::Text,
        value_nullable: false,
        replacement_started: false,
        replacement_ready: false,
        constraint_started: false,
        constraint_ready: false,
    };
    let history = vec![
        Operation {
            id: "write",
            invoke: 1,
            complete: 6,
            success: true,
            kind: OperationKind::PutRow(1, "one".into()),
        },
        Operation {
            id: "rename",
            invoke: 2,
            complete: 3,
            success: true,
            kind: OperationKind::RenameTable("renamed".into()),
        },
        Operation {
            id: "start",
            invoke: 4,
            complete: 5,
            success: true,
            kind: OperationKind::StartBuild,
        },
        Operation {
            id: "publish",
            invoke: 7,
            complete: 8,
            success: true,
            kind: OperationKind::PublishBuild,
        },
    ];
    let valid = LogicalState {
        table_name: "renamed".into(),
        columns: initial.columns.clone(),
        rows: BTreeMap::from([(1, "one".into())]),
        build_started: true,
        index_ready: true,
        value_type: ScalarType::Text,
        value_nullable: false,
        replacement_started: false,
        replacement_ready: false,
        constraint_started: false,
        constraint_ready: false,
    };
    assert!(bounded_serial_order(&initial, &history, &valid).is_some());

    for impossible in [
        LogicalState {
            rows: BTreeMap::new(),
            ..valid.clone()
        },
        LogicalState {
            build_started: false,
            ..valid.clone()
        },
        LogicalState {
            table_name: "items".into(),
            ..valid.clone()
        },
    ] {
        assert!(bounded_serial_order(&initial, &history, &impossible).is_none());
    }
}
