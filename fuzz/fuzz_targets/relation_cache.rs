#![no_main]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use libfuzzer_sys::fuzz_target;
use rad::engine::catalog;
use rad::engine::catalog::identity::SchemaId;
use rad::engine::catalog::model::{ColumnDef, ScalarType, Table, TableDef};
use rad::engine::exec::{
    Engine, EngineEvent, EngineEventHook, RelationCacheLookupResult, RelationCacheMaterialization,
};
use rad::engine::kv::TransactionalKv;
use rad::engine::kv::slatedb::Store;
use rad::engine::lir::{
    self, BinaryOp, Datum, Expr, Literal, RawScalar, Relation, RootCardinality, Row, Value,
};
use rad::engine::planner::estimator::StatisticsProvider;
use rad::engine::planner::models::{
    ColumnSynopsis, PlannerStats, SynopsisCoverage, SynopsisModel,
};
use tokio::runtime::Runtime;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static CASE: AtomicU64 = AtomicU64::new(0);

fuzz_target!(|data: &[u8]| {
    if data.len() > 64 {
        return;
    }
    let runtime = RUNTIME.get_or_init(|| Runtime::new().expect("Tokio runtime creation failed"));
    runtime.block_on(run_case(data));
});

struct FixedPlannerStats(Arc<PlannerStats>);

impl StatisticsProvider for FixedPlannerStats {
    fn planning_stats(&self) -> Arc<PlannerStats> {
        self.0.clone()
    }
}

#[derive(Default)]
struct RecordingEvents(Mutex<Vec<EngineEvent>>);

impl RecordingEvents {
    fn take(&self) -> Vec<EngineEvent> {
        std::mem::take(&mut *self.0.lock().expect("cache event lock poisoned"))
    }
}

#[async_trait]
impl EngineEventHook for RecordingEvents {
    async fn reach(&self, event: EngineEvent) {
        self.0
            .lock()
            .expect("cache event lock poisoned")
            .push(event);
    }
}

async fn run_case(data: &[u8]) {
    let byte = |index: usize| data.get(index).copied().unwrap_or(0);
    let stable_rows = usize::from(byte(0) % 5) + 1;
    let changing_rows = usize::from(byte(1) % 7);
    let include_nulls = byte(2) & 1 != 0;
    let descending = byte(3) & 1 != 0;
    let text_length = usize::from(byte(4) % 65);
    let mutation = byte(5) % 3;
    let store_name = format!(
        "relation-cache-fuzz-{}",
        CASE.fetch_add(1, Ordering::Relaxed)
    );
    let store = Arc::new(
        Store::memory(&store_name)
            .await
            .expect("memory store open failed"),
    );
    let catalog = catalog::Catalog::new(store.clone());
    let stable = catalog
        .create_table(join_table(10, "stable"))
        .await
        .expect("stable table creation failed");
    let changing = catalog
        .create_table(join_table(20, "changing"))
        .await
        .expect("changing table creation failed");
    catalog
        .create_table(join_table(30, "unrelated"))
        .await
        .expect("unrelated table creation failed");
    let mut statistics = complete_stats(&stable, stable_rows as u64);
    statistics
        .synopsis_models
        .extend(complete_stats(&changing, 5_000).synopsis_models);
    let events = Arc::new(RecordingEvents::default());
    let engine = Engine::new(store.clone())
        .with_statistics_provider(Arc::new(FixedPlannerStats(Arc::new(statistics))))
        .with_event_hook(events.clone());

    let stable = (0..stable_rows)
        .map(|index| {
            let key = if include_nulls && index + 1 == stable_rows {
                None
            } else if index < 2 {
                Some("shared".to_owned())
            } else {
                Some(format!("key-{index}"))
            };
            join_row_value(&format!("stable-{index}"), key, text_length)
        })
        .collect();
    engine
        .create_many("stable", stable)
        .await
        .expect("stable seed failed");
    let changing = (0..changing_rows)
        .map(|index| {
            let key = if include_nulls && index + 1 == changing_rows {
                None
            } else {
                Some("shared".to_owned())
            };
            join_row_value(&format!("changing-{index}"), key, text_length)
        })
        .collect();
    engine
        .create_many("changing", changing)
        .await
        .expect("changing seed failed");

    let first_query = join_query(None, descending);
    assert_same(
        engine.execute_uncached(first_query.clone()).await,
        engine.execute(first_query).await,
    );
    require_result(
        &events.take(),
        RelationCacheLookupResult::Filled,
        "initial fill",
    );

    let result_tag = match mutation {
        0 => {
            engine
                .create(
                    "changing",
                    join_row_value("changing-new", Some("shared".into()), text_length),
                )
                .await
                .expect("changing mutation failed");
            None
        }
        1 => {
            engine
                .create(
                    "stable",
                    join_row_value("stable-new", Some("shared".into()), text_length),
                )
                .await
                .expect("stable mutation failed");
            None
        }
        _ => {
            engine
                .create(
                    "unrelated",
                    join_row_value("unrelated-new", Some("shared".into()), text_length),
                )
                .await
                .expect("unrelated mutation failed");
            Some("second")
        }
    };
    let second_query = join_query(result_tag, descending);
    assert_same(
        engine.execute_uncached(second_query.clone()).await,
        engine.execute(second_query).await,
    );
    require_result(
        &events.take(),
        if mutation == 1 {
            RelationCacheLookupResult::Filled
        } else {
            RelationCacheLookupResult::Reused
        },
        "second lookup",
    );
    store.close().await.expect("memory store close failed");
}

fn assert_same(expected: rad::engine::exec::Result<Datum>, actual: rad::engine::exec::Result<Datum>) {
    match (expected, actual) {
        (Ok(expected), Ok(actual)) => assert_eq!(expected, actual),
        (Err(expected), Err(actual)) => {
            assert_eq!(expected.kind(), actual.kind());
            assert_eq!(expected.reason(), actual.reason());
        }
        (expected, actual) => panic!("cached and uncached outcomes differ: {expected:?}, {actual:?}"),
    }
}

fn require_result(events: &[EngineEvent], expected: RelationCacheLookupResult, label: &str) {
    assert!(
        events.iter().any(|event| {
            matches!(
                event,
                EngineEvent::SubrelationCacheLookupCompleted {
                    materialization: RelationCacheMaterialization::HashJoinBuild,
                    result,
                    ..
                } if *result == expected
            )
        }),
        "{label} does not contain {expected:?}: {events:#?}"
    );
}

fn join_table(id: u32, name: &str) -> TableDef {
    TableDef {
        id: SchemaId::new(id).expect("positive schema ID"),
        name: name.into(),
        columns: vec![
            ColumnDef {
                id: SchemaId::new(id * 10 + 1).expect("positive schema ID"),
                name: "id".into(),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                default: None,
            },
            ColumnDef {
                id: SchemaId::new(id * 10 + 2).expect("positive schema ID"),
                name: "key".into(),
                scalar_type: ScalarType::Text,
                nullable: true,
                format: String::new(),
                default: None,
            },
        ],
        primary_key: vec!["id".into()],
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
    }
}

fn join_row_value(id: &str, key: Option<String>, text_length: usize) -> Row {
    Row::from([
        ("id".into(), Value::Text(stored_id(id, text_length))),
        (
            "key".into(),
            key.map_or(Value::Null(ScalarType::Text), Value::Text),
        ),
    ])
}

fn stored_id(id: &str, text_length: usize) -> String {
    format!("{id}{}", "x".repeat(text_length))
}

fn complete_stats(table: &Table, rows: u64) -> PlannerStats {
    let mut statistics = PlannerStats::empty();
    statistics.synopsis_models.insert(
        table.schema_id,
        SynopsisModel {
            table: table.schema_id,
            observed_rows: rows,
            coverage: SynopsisCoverage::Complete,
            sample_size: rows,
            changes_since_collection: 0,
            table_existence_generation: table.existence_generation.get(),
            collected_at_unix_micros: 0,
            catalog_version: 1,
            columns: table
                .columns
                .iter()
                .map(|column| ColumnSynopsis {
                    column: column.schema_id,
                    value_generation: column.value_generation.get(),
                    null_fraction: 0.0,
                    null_count: 0,
                    distinct: rows.max(1),
                    distinct_is_exact: false,
                    average_width: 8,
                    maximum_width: Some(8),
                    minimum: None,
                    maximum: None,
                    most_common_values: Vec::new(),
                    range_distribution: None,
                    degree_sequence: None,
                })
                .collect(),
            column_groups: Vec::new(),
            predicate_conditioned_degrees: Vec::new(),
        },
    );
    statistics
}

fn join_query(result_tag: Option<&str>, descending: bool) -> lir::Query {
    let joined = Relation::Join {
        left: Box::new(Relation::Scan {
            table: "stable".into(),
            scope: "stable".into(),
        }),
        right: Box::new(Relation::Scan {
            table: "changing".into(),
            scope: "changing".into(),
        }),
        kind: lir::JoinKind::Inner,
        on: Expr::Binary {
            op: BinaryOp::Eq,
            left: Box::new(column("stable", "key")),
            right: Box::new(column("changing", "key")),
        },
    };
    let mut fields = vec![
        lir::ProjectField {
            name: "stable_id".into(),
            expression: column("stable", "id"),
        },
        lir::ProjectField {
            name: "changing_id".into(),
            expression: column("changing", "id"),
        },
    ];
    if let Some(tag) = result_tag {
        fields.push(lir::ProjectField {
            name: "tag".into(),
            expression: Expr::Literal(Literal {
                raw: RawScalar::Text(tag.into()),
                kind: Some(lir::Kind::Text),
            }),
        });
    }
    lir::Query {
        root: Relation::Order {
            input: Box::new(Relation::Project {
                input: Box::new(joined),
                scope: Some("result".into()),
                spread: Vec::new(),
                fields,
            }),
            terms: vec![lir::OrderTerm {
                expression: column("result", "changing_id"),
                descending,
            }],
        },
        cardinality: RootCardinality::Many,
        bindings: HashMap::new(),
    }
}

fn column(scope: &str, name: &str) -> Expr {
    Expr::Column {
        scope: scope.into(),
        name: name.into(),
    }
}
