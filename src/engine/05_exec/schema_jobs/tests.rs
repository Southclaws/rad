use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::Notify;

use crate::engine::catalog;
use crate::engine::catalog::identity::SchemaId;
use crate::engine::catalog::model::{
    ColumnConversion, ColumnDef, ColumnReplacementDef, ConstraintDef, ConstraintKind, IndexDef,
    IndexState, Reclamation, ReclamationKind, ReclamationState, RetentionOwnerKind, RetentionPin,
    RetentionResource, RetentionResourceKind, ScalarType, SchemaTransition, TableDef, Timestamp,
    TransitionState,
};
use crate::engine::catalog::store;
use crate::engine::exec::{
    Engine, EngineEvent, EngineEventHook, EngineOperation, ErrorKind, GateAction, codec, row_store,
};
use crate::engine::kv::key_encoding::prefix_end;
use crate::engine::kv::slatedb::Store;
use crate::engine::kv::{IsolationLevel, KeyRange, KvView, TransactionView, TransactionalKv};
use crate::engine::lir::{Field, Kind, Row, RowType, SlotId, Type, Value};

use super::SchemaJob;

#[derive(Default)]
struct RecordingHook(Mutex<Vec<EngineEvent>>);

#[async_trait]
impl EngineEventHook for RecordingHook {
    async fn reach(&self, event: EngineEvent) {
        self.0
            .lock()
            .expect("schema event lock poisoned")
            .push(event);
    }
}

impl RecordingHook {
    fn events(&self) -> Vec<EngineEvent> {
        self.0.lock().expect("schema event lock poisoned").clone()
    }
}

#[derive(Clone, Copy)]
enum Boundary {
    ActivationCommitStarted,
    FirstPhysicalBatch,
    ReclamationPhysicalBatch,
    ReadyPublication,
}

struct BlockingHook {
    boundary: Boundary,
    blocked: AtomicBool,
    reached: Notify,
    release: Notify,
}

impl BlockingHook {
    fn new(boundary: Boundary) -> Self {
        Self {
            boundary,
            blocked: AtomicBool::new(false),
            reached: Notify::new(),
            release: Notify::new(),
        }
    }

    async fn wait_until_reached(&self) {
        let reached = self.reached.notified();
        if !self.blocked.load(Ordering::Acquire) {
            reached.await;
        }
    }

    fn release(&self) {
        self.release.notify_one();
    }

    fn matches(&self, event: &EngineEvent) -> bool {
        match self.boundary {
            Boundary::ActivationCommitStarted => matches!(
                event,
                EngineEvent::CommitStarted {
                    operation: EngineOperation::ActivateTransition { .. }
                }
            ),
            Boundary::FirstPhysicalBatch => {
                matches!(event, EngineEvent::PhysicalBatchStaged { .. })
            }
            Boundary::ReclamationPhysicalBatch => matches!(
                event,
                EngineEvent::PhysicalBatchStaged {
                    operation: EngineOperation::StepReclamation { .. },
                    ..
                }
            ),
            Boundary::ReadyPublication => matches!(
                event,
                EngineEvent::CatalogPublicationStaged {
                    state: TransitionState::Ready,
                    ..
                }
            ),
        }
    }
}

#[async_trait]
impl EngineEventHook for BlockingHook {
    async fn reach(&self, event: EngineEvent) {
        if self.matches(&event) && !self.blocked.swap(true, Ordering::AcqRel) {
            self.reached.notify_one();
            self.release.notified().await;
        }
    }
}

fn column(id: u32, name: &str, scalar_type: ScalarType, nullable: bool) -> ColumnDef {
    ColumnDef {
        id: SchemaId::new(id).unwrap(),
        name: name.into(),
        scalar_type,
        nullable,
        format: String::new(),
        default: None,
    }
}

async fn start_index(
    engine: &Engine,
    table: SchemaId,
    name: &str,
    column: &str,
    prerequisites: Vec<crate::engine::catalog::identity::TransitionId>,
) -> SchemaTransition {
    start_index_with_uniqueness(engine, table, name, column, false, prerequisites).await
}

async fn start_index_with_uniqueness(
    engine: &Engine,
    table: SchemaId,
    name: &str,
    column: &str,
    unique: bool,
    prerequisites: Vec<crate::engine::catalog::identity::TransitionId>,
) -> SchemaTransition {
    let transaction = engine
        .store
        .begin(IsolationLevel::SerializableSnapshot)
        .await
        .unwrap();
    let transition = {
        let mut view = TransactionView(&*transaction);
        let mut mutation = catalog::Mutation::with_runtime(&mut view, engine.runtime.clone());
        let transition = mutation
            .start_index_build_with_prerequisites(
                table,
                IndexDef {
                    name: name.into(),
                    columns: vec![column.into()],
                    unique,
                },
                prerequisites,
            )
            .await
            .unwrap();
        mutation.finish().await.unwrap();
        transition
    };
    transaction.commit().await.unwrap();
    transition
}

async fn start_replacement(
    engine: &Engine,
    table: SchemaId,
    column: SchemaId,
    scalar_type: ScalarType,
) -> SchemaTransition {
    start_replacement_with_prerequisites(engine, table, column, scalar_type, Vec::new()).await
}

async fn start_replacement_with_prerequisites(
    engine: &Engine,
    table: SchemaId,
    column: SchemaId,
    scalar_type: ScalarType,
    prerequisites: Vec<crate::engine::catalog::identity::TransitionId>,
) -> SchemaTransition {
    let transaction = engine
        .store
        .begin(IsolationLevel::SerializableSnapshot)
        .await
        .unwrap();
    let transition = {
        let mut view = TransactionView(&*transaction);
        let mut mutation = catalog::Mutation::with_runtime(&mut view, engine.runtime.clone());
        let transition = mutation
            .start_column_replacement(
                table,
                column,
                ColumnReplacementDef {
                    scalar_type,
                    nullable: false,
                    format: String::new(),
                    default: None,
                    conversion: ColumnConversion::StrictBuiltin,
                    prerequisites,
                },
            )
            .await
            .unwrap();
        mutation.finish().await.unwrap();
        transition
    };
    transaction.commit().await.unwrap();
    transition
}

async fn start_not_null_constraint(
    engine: &Engine,
    table: SchemaId,
    column: SchemaId,
    name: &str,
) -> SchemaTransition {
    let transaction = engine
        .store
        .begin(IsolationLevel::SerializableSnapshot)
        .await
        .unwrap();
    let transition = {
        let mut view = TransactionView(&*transaction);
        let mut mutation = catalog::Mutation::with_runtime(&mut view, engine.runtime.clone());
        let transition = mutation
            .start_constraint_validation(
                table,
                ConstraintDef {
                    name: name.into(),
                    kind: ConstraintKind::NotNull,
                    column_id: column,
                    prerequisites: Vec::new(),
                },
            )
            .await
            .unwrap();
        mutation.finish().await.unwrap();
        transition
    };
    transaction.commit().await.unwrap();
    transition
}

async fn run_transition(engine: &Engine, id: &crate::engine::catalog::identity::TransitionId) {
    let owner = engine.claim_schema_transition(id).await.unwrap();
    for _ in 0..32 {
        let step = engine.step_schema_transition(id, owner, 1).await.unwrap();
        if step.transition.state.is_terminal() {
            return;
        }
    }
    panic!("transition {id:?} did not finish");
}

fn text_input_type(names: &[&str]) -> RowType {
    RowType {
        fields: names
            .iter()
            .enumerate()
            .map(|(index, name)| Field {
                name: (*name).into(),
                slot: SlotId(index),
                value_type: Type::scalar(Kind::Text, false),
            })
            .collect(),
    }
}

async fn assert_exact_index(engine: &Engine, table_name: &str, index_name: &str) {
    let catalog = catalog::Catalog::new(engine.store.clone());
    let table = catalog
        .get_table(table_name)
        .await
        .unwrap()
        .expect("audited table exists");
    let index = table
        .indexes
        .iter()
        .find(|index| index.name == index_name)
        .expect("audited index exists");
    assert_eq!(index.state, IndexState::Ready);

    let transaction = engine.store.begin(IsolationLevel::Snapshot).await.unwrap();
    let view = TransactionView(&*transaction);
    let rows = row_store::scan_table_columns(&view, &table, &table.columns)
        .await
        .unwrap();
    let expected = rows
        .iter()
        .map(|row| {
            let primary_key = codec::encode_row_tuple(row, &table.primary_key).unwrap();
            let tuple = codec::encode_row_tuple(row, &index.columns).unwrap();
            (
                codec::index_key(&table, &index.id, &tuple, &primary_key),
                primary_key,
            )
        })
        .collect::<BTreeMap<_, _>>();

    let prefix = codec::index_prefix(&table, &index.id);
    let mut iterator = view
        .scan(KeyRange {
            start: Some(Bytes::from(prefix.clone())),
            end: prefix_end(&prefix).map(Bytes::from),
        })
        .await
        .unwrap();
    let mut actual = BTreeMap::new();
    while let Some(entry) = iterator.next().await.unwrap() {
        actual.insert(entry.key.to_vec(), entry.value.to_vec());
    }
    drop(iterator);
    transaction.rollback();

    assert_eq!(actual, expected, "ready index diverged from table contents");
}

async fn count_prefix(engine: &Engine, prefix: Vec<u8>) -> usize {
    let transaction = engine.store.begin(IsolationLevel::Snapshot).await.unwrap();
    let view = TransactionView(&*transaction);
    let mut iterator = view
        .scan(KeyRange {
            start: Some(Bytes::from(prefix.clone())),
            end: prefix_end(&prefix).map(Bytes::from),
        })
        .await
        .unwrap();
    let mut count = 0;
    while iterator.next().await.unwrap().is_some() {
        count += 1;
    }
    drop(iterator);
    transaction.rollback();
    count
}

async fn reclamation_by_kind(engine: &Engine, kind: ReclamationKind) -> Reclamation {
    let transaction = engine.store.begin(IsolationLevel::Snapshot).await.unwrap();
    let values = {
        let mut view = TransactionView(&*transaction);
        store::list_reclamations(&mut view).await.unwrap()
    };
    transaction.rollback();
    values
        .into_iter()
        .find(|value| value.kind == kind && value.state != ReclamationState::Reclaimed)
        .unwrap_or_else(|| panic!("missing {kind:?} reclamation"))
}

async fn finish_reclamation(engine: &Engine, reclamation: &Reclamation, batch_size: usize) {
    let owner = engine
        .claim_reclamation(&reclamation.id)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..128 {
        let (current, items) = engine
            .step_reclamation(&reclamation.id, owner, batch_size)
            .await
            .unwrap();
        assert!(items <= batch_size.max(1));
        if current.state == ReclamationState::Reclaimed {
            return;
        }
    }
    panic!("reclamation {:?} did not finish", reclamation.id);
}

#[tokio::test]
async fn index_worker_fences_owners_activates_prerequisites_and_reclaims_diagnostics() {
    let kv = Arc::new(Store::memory("schema-job-index").await.unwrap());
    let catalog = catalog::Catalog::new(kv.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(1).unwrap(),
            name: "items".into(),
            columns: vec![
                column(1, "id", ScalarType::Text, false),
                column(2, "status", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let engine = Engine::new(kv.clone());
    for (id, status) in [("a", "open"), ("b", "closed"), ("c", "open")] {
        engine
            .create(
                "items",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("status".into(), Value::Text(status.into())),
                ]),
            )
            .await
            .unwrap();
    }

    let first = start_index(&engine, table.schema_id, "status_idx", "status", Vec::new()).await;
    let second = start_index(
        &engine,
        table.schema_id,
        "status_idx_2",
        "status",
        vec![first.id.clone()],
    )
    .await;
    assert_eq!(second.state, TransitionState::Waiting);
    assert!(
        engine
            .discover_schema_jobs(8)
            .await
            .unwrap()
            .contains(&SchemaJob::Activation(second.id.clone()))
    );
    let still_waiting = engine
        .activate_waiting_schema_transition(&second.id)
        .await
        .unwrap();
    assert_eq!(still_waiting.state, TransitionState::Waiting);

    let stale_owner = engine.claim_schema_transition(&first.id).await.unwrap();
    let owner = engine.claim_schema_transition(&first.id).await.unwrap();
    assert!(owner > stale_owner);
    let error = engine
        .step_schema_transition(&first.id, stale_owner, 1)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict);
    for _ in 0..32 {
        let step = engine
            .step_schema_transition(&first.id, owner, 1)
            .await
            .unwrap();
        if step.transition.state.is_terminal() {
            assert_eq!(step.transition.state, TransitionState::Ready);
            assert_eq!(step.transition.rows_scanned, 3);
            break;
        }
    }

    let activated = engine
        .activate_waiting_schema_transition(&second.id)
        .await
        .unwrap();
    assert_eq!(activated.state, TransitionState::Building);
    let current = catalog.get_table("items").await.unwrap().unwrap();
    assert!(
        current
            .indexes
            .iter()
            .any(|index| { index.name == "status_idx" && index.state == IndexState::Ready })
    );

    let reclamation = {
        let transaction = kv.begin(IsolationLevel::Snapshot).await.unwrap();
        let values = {
            let mut view = TransactionView(&*transaction);
            store::list_reclamations(&mut view).await.unwrap()
        };
        transaction.rollback();
        values
            .into_iter()
            .find(|value| value.transition_id == first.id)
            .unwrap()
    };
    assert!(
        engine
            .discover_schema_jobs(8)
            .await
            .unwrap()
            .contains(&SchemaJob::Reclamation(reclamation.id.clone()))
    );
    let reclamation_owner = engine
        .claim_reclamation(&reclamation.id)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..32 {
        let (current, _) = engine
            .step_reclamation(&reclamation.id, reclamation_owner, 1)
            .await
            .unwrap();
        if current.state == ReclamationState::Reclaimed {
            break;
        }
    }
    let metrics = engine.schema_storage_metrics().await.unwrap();
    assert_eq!(metrics.terminal_reclamation_records, 1);
    assert_eq!(metrics.uncompacted_terminal_transitions, 0);
}

#[tokio::test]
async fn replacement_and_constraint_workers_publish_catalog_changes() {
    let kv = Arc::new(
        Store::memory("schema-job-replacement-constraint")
            .await
            .unwrap(),
    );
    let catalog = catalog::Catalog::new(kv.clone());
    let replacements = catalog
        .create_table(TableDef {
            id: SchemaId::new(10).unwrap(),
            name: "replacements".into(),
            columns: vec![
                column(10, "id", ScalarType::Text, false),
                column(11, "value", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let constraints = catalog
        .create_table(TableDef {
            id: SchemaId::new(20).unwrap(),
            name: "constraints".into(),
            columns: vec![
                column(20, "id", ScalarType::Text, false),
                column(21, "label", ScalarType::Text, true),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let events = Arc::new(RecordingHook::default());
    let engine = Engine::new(kv.clone()).with_event_hook(events.clone());
    for (table, id, value_name, value) in [
        ("replacements", "r1", "value", "41"),
        ("replacements", "r2", "value", "42"),
        ("constraints", "c1", "label", "known"),
    ] {
        engine
            .create(
                table,
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    (value_name.into(), Value::Text(value.into())),
                ]),
            )
            .await
            .unwrap();
    }

    let replacement = {
        let transaction = kv
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        let transition = {
            let mut view = TransactionView(&*transaction);
            let mut mutation = catalog::Mutation::new(&mut view);
            let transition = mutation
                .start_column_replacement(
                    replacements.schema_id,
                    SchemaId::new(11).unwrap(),
                    ColumnReplacementDef {
                        scalar_type: ScalarType::Int64,
                        nullable: false,
                        format: String::new(),
                        default: None,
                        conversion: ColumnConversion::StrictBuiltin,
                        prerequisites: Vec::new(),
                    },
                )
                .await
                .unwrap();
            mutation.finish().await.unwrap();
            transition
        };
        transaction.commit().await.unwrap();
        transition
    };
    let constraint = {
        let transaction = kv
            .begin(IsolationLevel::SerializableSnapshot)
            .await
            .unwrap();
        let transition = {
            let mut view = TransactionView(&*transaction);
            let mut mutation = catalog::Mutation::new(&mut view);
            let transition = mutation
                .start_constraint_validation(
                    constraints.schema_id,
                    ConstraintDef {
                        name: "label_not_null".into(),
                        kind: ConstraintKind::NotNull,
                        column_id: SchemaId::new(21).unwrap(),
                        prerequisites: Vec::new(),
                    },
                )
                .await
                .unwrap();
            mutation.finish().await.unwrap();
            transition
        };
        transaction.commit().await.unwrap();
        transition
    };

    run_transition(&engine, &replacement.id).await;
    run_transition(&engine, &constraint.id).await;
    assert_eq!(
        engine
            .inspect_schema_transition(&replacement.id)
            .await
            .unwrap()
            .state,
        TransitionState::Ready
    );
    assert_eq!(
        engine
            .inspect_schema_transition(&constraint.id)
            .await
            .unwrap()
            .state,
        TransitionState::Ready
    );

    let replacement_table = catalog.get_table("replacements").await.unwrap().unwrap();
    let value_column = replacement_table
        .columns
        .iter()
        .find(|column| column.name == "value")
        .unwrap();
    assert_eq!(value_column.scalar_type, ScalarType::Int64);
    let transaction = kv.begin(IsolationLevel::Snapshot).await.unwrap();
    let rows = {
        let view = TransactionView(&*transaction);
        row_store::scan_table_columns(&view, &replacement_table, &replacement_table.columns)
            .await
            .unwrap()
    };
    transaction.rollback();
    assert_eq!(
        rows.iter()
            .map(|row| row["value"].clone())
            .collect::<Vec<_>>(),
        vec![Value::Int64(41), Value::Int64(42)]
    );
    let constraint_table = catalog.get_table("constraints").await.unwrap().unwrap();
    assert!(
        !constraint_table
            .columns
            .iter()
            .find(|column| column.name == "label")
            .unwrap()
            .nullable
    );

    let recorded = events.events();
    for (id, kind) in [
        (
            &replacement.id,
            crate::engine::catalog::model::TransitionKind::ColumnReplacement,
        ),
        (
            &constraint.id,
            crate::engine::catalog::model::TransitionKind::ConstraintValidation,
        ),
    ] {
        assert!(recorded.iter().any(|event| matches!(
            event,
            EngineEvent::PhysicalBatchStaged {
                operation: EngineOperation::StepTransition {
                    transition_id,
                    transition_kind,
                    ..
                },
                ..
            } if transition_id == id && *transition_kind == kind
        )));
        assert!(recorded.iter().any(|event| matches!(
            event,
            EngineEvent::FinalizationGateStaged {
                operation: EngineOperation::StepTransition { transition_id, .. },
                action: GateAction::Acquired,
            } if transition_id == id
        )));
        assert!(recorded.iter().any(|event| matches!(
            event,
            EngineEvent::CatalogPublicationStaged {
                operation: EngineOperation::StepTransition { transition_id, .. },
                state: TransitionState::Ready,
            } if transition_id == id
        )));
    }
}

#[tokio::test]
async fn worker_errors_are_durable_and_reclamation_failure_is_terminal() {
    let kv = Arc::new(Store::memory("schema-job-errors").await.unwrap());
    let catalog = catalog::Catalog::new(kv.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(30).unwrap(),
            name: "errors".into(),
            columns: vec![column(30, "id", ScalarType::Text, false)],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let engine = Engine::new(kv.clone());
    let transition = start_index(&engine, table.schema_id, "id_idx", "id", Vec::new()).await;
    let owner = engine
        .claim_schema_transition(&transition.id)
        .await
        .unwrap();
    let current = engine
        .record_schema_transition_error(&transition.id, owner, "injected worker failure")
        .await
        .unwrap();
    assert_eq!(current.state, TransitionState::Building);
    assert_eq!(current.last_error, "injected worker failure");
    catalog
        .cancel_schema_transition(&transition.id)
        .await
        .unwrap();
    let reclamation = {
        let transaction = kv.begin(IsolationLevel::Snapshot).await.unwrap();
        let values = {
            let mut view = TransactionView(&*transaction);
            store::list_reclamations(&mut view).await.unwrap()
        };
        transaction.rollback();
        values
            .into_iter()
            .find(|value| value.transition_id == transition.id)
            .unwrap()
    };
    let owner = engine
        .claim_reclamation(&reclamation.id)
        .await
        .unwrap()
        .unwrap();
    let failed = engine
        .fail_reclamation(&reclamation.id, owner, "injected reclamation failure")
        .await
        .unwrap();
    assert_eq!(failed.state, ReclamationState::Failed);
    assert_eq!(failed.last_error, "injected reclamation failure");
    let metrics = engine.schema_storage_metrics().await.unwrap();
    assert_eq!(metrics.failed_reclamations, 1);
}

#[tokio::test]
async fn unique_index_build_validates_final_state_treats_nulls_as_distinct_and_releases_failures() {
    let kv = Arc::new(
        Store::memory("schema-job-unique-final-state")
            .await
            .unwrap(),
    );
    let catalog = catalog::Catalog::new(kv.clone());
    let engine = Engine::new(kv.clone());

    let resolving = catalog
        .create_table(TableDef {
            id: SchemaId::new(200).unwrap(),
            name: "resolving".into(),
            columns: vec![
                column(200, "id", ScalarType::Text, false),
                column(201, "code", ScalarType::Text, true),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    for (id, code) in [
        ("a", Value::Text("duplicate".into())),
        ("b", Value::Text("duplicate".into())),
        ("c", Value::Null(ScalarType::Text)),
        ("d", Value::Null(ScalarType::Text)),
    ] {
        engine
            .create(
                "resolving",
                Row::from([("id".into(), Value::Text(id.into())), ("code".into(), code)]),
            )
            .await
            .unwrap();
    }
    let transition = start_index_with_uniqueness(
        &engine,
        resolving.schema_id,
        "resolving_code_idx",
        "code",
        true,
        Vec::new(),
    )
    .await;
    let owner = engine
        .claim_schema_transition(&transition.id)
        .await
        .unwrap();
    assert_eq!(
        engine
            .step_schema_transition(&transition.id, owner, 1)
            .await
            .unwrap()
            .items,
        1
    );
    engine
        .update_many(
            "resolving",
            text_input_type(&["id", "code"]),
            vec![Row::from([
                ("id".into(), Value::Text("b".into())),
                ("code".into(), Value::Text("resolved".into())),
            ])],
        )
        .await
        .unwrap();
    for _ in 0..32 {
        let step = engine
            .step_schema_transition(&transition.id, owner, 1)
            .await
            .unwrap();
        if step.transition.state.is_terminal() {
            assert_eq!(step.transition.state, TransitionState::Ready);
            break;
        }
    }
    assert_exact_index(&engine, "resolving", "resolving_code_idx").await;
    assert_eq!(
        engine
            .create(
                "resolving",
                Row::from([
                    ("id".into(), Value::Text("e".into())),
                    ("code".into(), Value::Text("duplicate".into())),
                ]),
            )
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::ConstraintViolation
    );

    let failing = catalog
        .create_table(TableDef {
            id: SchemaId::new(210).unwrap(),
            name: "failing".into(),
            columns: vec![
                column(210, "id", ScalarType::Text, false),
                column(211, "code", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    for id in ["a", "b"] {
        engine
            .create(
                "failing",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("code".into(), Value::Text("duplicate".into())),
                ]),
            )
            .await
            .unwrap();
    }
    let failed = start_index_with_uniqueness(
        &engine,
        failing.schema_id,
        "failing_code_idx",
        "code",
        true,
        Vec::new(),
    )
    .await;
    run_transition(&engine, &failed.id).await;
    let failed = engine.inspect_schema_transition(&failed.id).await.unwrap();
    assert_eq!(failed.state, TransitionState::Failed);
    assert!(failed.last_error.contains("unique"));
    engine
        .create(
            "failing",
            Row::from([
                ("id".into(), Value::Text("c".into())),
                ("code".into(), Value::Text("distinct".into())),
            ]),
        )
        .await
        .expect("a failed build must release its finalization gate");
    assert!(
        catalog
            .get_table("failing")
            .await
            .unwrap()
            .unwrap()
            .indexes
            .iter()
            .all(|index| index.name != "failing_code_idx")
    );
}

#[tokio::test]
async fn replacement_failure_preserves_source_and_constraint_repair_clears_violations() {
    let kv = Arc::new(Store::memory("schema-job-failure-repair").await.unwrap());
    let catalog = catalog::Catalog::new(kv.clone());
    let engine = Engine::new(kv.clone());

    let replacement_table = catalog
        .create_table(TableDef {
            id: SchemaId::new(220).unwrap(),
            name: "replacement_failure".into(),
            columns: vec![
                column(220, "id", ScalarType::Text, false),
                column(221, "value", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    engine
        .create(
            "replacement_failure",
            Row::from([
                ("id".into(), Value::Text("bad".into())),
                ("value".into(), Value::Text("not-an-integer".into())),
            ]),
        )
        .await
        .unwrap();
    let replacement = start_replacement(
        &engine,
        replacement_table.schema_id,
        SchemaId::new(221).unwrap(),
        ScalarType::Int64,
    )
    .await;
    run_transition(&engine, &replacement.id).await;
    let replacement = engine
        .inspect_schema_transition(&replacement.id)
        .await
        .unwrap();
    assert_eq!(replacement.state, TransitionState::Failed);
    assert!(replacement.last_error.contains("not-an-integer"));
    let current = catalog
        .get_table("replacement_failure")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        current.column("value").unwrap().scalar_type,
        ScalarType::Text
    );
    let rows = engine.scan_table_rows(&current).await.unwrap();
    assert_eq!(rows[0]["value"], Value::Text("not-an-integer".into()));
    engine
        .create(
            "replacement_failure",
            Row::from([
                ("id".into(), Value::Text("after".into())),
                ("value".into(), Value::Text("still-text".into())),
            ]),
        )
        .await
        .expect("failed replacement must remove dual-write obligations");

    let constraint_table = catalog
        .create_table(TableDef {
            id: SchemaId::new(230).unwrap(),
            name: "constraint_repair".into(),
            columns: vec![
                column(230, "id", ScalarType::Text, false),
                column(231, "label", ScalarType::Text, true),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    for (id, label) in [
        ("a", Value::Null(ScalarType::Text)),
        ("b", Value::Text("known".into())),
    ] {
        engine
            .create(
                "constraint_repair",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("label".into(), label),
                ]),
            )
            .await
            .unwrap();
    }
    let constraint = start_not_null_constraint(
        &engine,
        constraint_table.schema_id,
        SchemaId::new(231).unwrap(),
        "label_not_null",
    )
    .await;
    let owner = engine
        .claim_schema_transition(&constraint.id)
        .await
        .unwrap();
    assert_eq!(
        engine
            .step_schema_transition(&constraint.id, owner, 1)
            .await
            .unwrap()
            .items,
        0,
        "the first step must publish enforcement before scanning history"
    );
    assert_eq!(
        engine
            .create(
                "constraint_repair",
                Row::from([
                    ("id".into(), Value::Text("blocked".into())),
                    ("label".into(), Value::Null(ScalarType::Text)),
                ]),
            )
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::ConstraintViolation
    );
    assert_eq!(
        engine
            .step_schema_transition(&constraint.id, owner, 1)
            .await
            .unwrap()
            .items,
        1
    );
    engine
        .update_many(
            "constraint_repair",
            text_input_type(&["id", "label"]),
            vec![Row::from([
                ("id".into(), Value::Text("a".into())),
                ("label".into(), Value::Text("repaired".into())),
            ])],
        )
        .await
        .unwrap();
    for _ in 0..32 {
        let step = engine
            .step_schema_transition(&constraint.id, owner, 1)
            .await
            .unwrap();
        if step.transition.state.is_terminal() {
            assert_eq!(step.transition.state, TransitionState::Ready);
            break;
        }
    }
    let current = catalog
        .get_table("constraint_repair")
        .await
        .unwrap()
        .unwrap();
    assert!(!current.column("label").unwrap().nullable);
}

#[tokio::test]
async fn replacement_chain_rebinds_each_waiting_step_to_the_published_physical_source() {
    let kv = Arc::new(Store::memory("schema-job-replacement-chain").await.unwrap());
    let catalog = catalog::Catalog::new(kv.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(240).unwrap(),
            name: "replacement_chain".into(),
            columns: vec![
                column(240, "id", ScalarType::Text, false),
                column(241, "value", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let engine = Engine::new(kv);
    engine
        .create(
            "replacement_chain",
            Row::from([
                ("id".into(), Value::Text("row".into())),
                ("value".into(), Value::Text("41".into())),
            ]),
        )
        .await
        .unwrap();

    let first = start_replacement(
        &engine,
        table.schema_id,
        SchemaId::new(241).unwrap(),
        ScalarType::Int64,
    )
    .await;
    let original_source = first.column_replacement.as_ref().unwrap().source.id.clone();
    let second = start_replacement_with_prerequisites(
        &engine,
        table.schema_id,
        SchemaId::new(241).unwrap(),
        ScalarType::Text,
        vec![first.id.clone()],
    )
    .await;
    assert_eq!(second.state, TransitionState::Waiting);
    assert!(second.column_replacement.is_none());

    run_transition(&engine, &first.id).await;
    let published = catalog
        .get_table("replacement_chain")
        .await
        .unwrap()
        .unwrap()
        .column("value")
        .unwrap()
        .clone();
    assert_eq!(published.scalar_type, ScalarType::Int64);
    assert_ne!(published.id, original_source);

    let activated = engine
        .activate_waiting_schema_transition(&second.id)
        .await
        .unwrap();
    assert_eq!(activated.state, TransitionState::Building);
    assert_eq!(
        activated.column_replacement.as_ref().unwrap().source.id,
        published.id,
        "activation must bind the logical column to the prerequisite's published physical cell"
    );
    run_transition(&engine, &second.id).await;
    let final_table = catalog
        .get_table("replacement_chain")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        final_table.column("value").unwrap().scalar_type,
        ScalarType::Text
    );
    let rows = engine.scan_table_rows(&final_table).await.unwrap();
    assert_eq!(rows[0]["value"], Value::Text("41".into()));
}

#[tokio::test]
async fn cancelled_index_reclamation_is_prefix_bounded() {
    let kv = Arc::new(Store::memory("schema-job-cancelled-index").await.unwrap());
    let catalog = catalog::Catalog::new(kv.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(40).unwrap(),
            name: "bounded".into(),
            columns: vec![
                column(40, "id", ScalarType::Text, false),
                column(41, "value", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let engine = Engine::new(kv.clone());
    for id in ["a", "b"] {
        engine
            .create(
                "bounded",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("value".into(), Value::Text("kept".into())),
                ]),
            )
            .await
            .unwrap();
    }
    let transition =
        start_index(&engine, table.schema_id, "partial_idx", "value", Vec::new()).await;
    let owner = engine
        .claim_schema_transition(&transition.id)
        .await
        .unwrap();
    assert_eq!(
        engine
            .step_schema_transition(&transition.id, owner, 1)
            .await
            .unwrap()
            .items,
        1
    );
    catalog
        .cancel_schema_transition(&transition.id)
        .await
        .unwrap();
    let reclamation = {
        let transaction = kv.begin(IsolationLevel::Snapshot).await.unwrap();
        let values = {
            let mut view = TransactionView(&*transaction);
            store::list_reclamations(&mut view).await.unwrap()
        };
        transaction.rollback();
        values
            .into_iter()
            .find(|value| value.transition_id == transition.id)
            .unwrap()
    };
    let owner = engine
        .claim_reclamation(&reclamation.id)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..32 {
        let (current, _) = engine
            .step_reclamation(&reclamation.id, owner, 1)
            .await
            .unwrap();
        if current.state == ReclamationState::Reclaimed {
            break;
        }
    }
    let current = catalog.get_table("bounded").await.unwrap().unwrap();
    let transaction = kv.begin(IsolationLevel::Snapshot).await.unwrap();
    let rows = {
        let view = TransactionView(&*transaction);
        row_store::scan_table_columns(&view, &current, &current.columns)
            .await
            .unwrap()
    };
    transaction.rollback();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|row| row["value"] == Value::Text("kept".into()))
    );
}

#[tokio::test]
async fn table_column_and_index_reclamation_is_bounded_and_preserves_old_snapshots() {
    let kv = Arc::new(
        Store::memory("schema-job-physical-reclamation")
            .await
            .unwrap(),
    );
    let catalog = catalog::Catalog::new(kv.clone());
    let engine = Engine::new(kv.clone());

    let retired_table = catalog
        .create_table(TableDef {
            id: SchemaId::new(250).unwrap(),
            name: "retired_table".into(),
            columns: vec![
                column(250, "id", ScalarType::Text, false),
                column(251, "value", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: vec![IndexDef {
                name: "retired_table_value_idx".into(),
                columns: vec!["value".into()],
                unique: false,
            }],
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    for id in ["a", "b"] {
        engine
            .create(
                "retired_table",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("value".into(), Value::Text(format!("value-{id}"))),
                ]),
            )
            .await
            .unwrap();
    }
    let table_snapshot = kv.begin(IsolationLevel::Snapshot).await.unwrap();
    catalog.delete_table("retired_table").await.unwrap();
    let table_reclamation = reclamation_by_kind(&engine, ReclamationKind::Table).await;
    finish_reclamation(&engine, &table_reclamation, 1).await;
    assert_eq!(
        count_prefix(&engine, codec::data_prefix(&retired_table)).await,
        0
    );
    for index in &retired_table.indexes {
        assert_eq!(
            count_prefix(&engine, codec::index_prefix(&retired_table, &index.id)).await,
            0
        );
    }
    let old_rows = {
        let view = TransactionView(&*table_snapshot);
        row_store::scan_table_columns(&view, &retired_table, &retired_table.columns)
            .await
            .unwrap()
    };
    assert_eq!(old_rows.len(), 2);
    table_snapshot.rollback();

    let column_table = catalog
        .create_table(TableDef {
            id: SchemaId::new(260).unwrap(),
            name: "retired_column".into(),
            columns: vec![
                column(260, "id", ScalarType::Text, false),
                column(261, "kept", ScalarType::Text, false),
                column(262, "retiring", ScalarType::Text, true),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    for id in ["a", "b"] {
        engine
            .create(
                "retired_column",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("kept".into(), Value::Text(format!("kept-{id}"))),
                    ("retiring".into(), Value::Text(format!("retired-{id}"))),
                ]),
            )
            .await
            .unwrap();
    }
    let column_snapshot = kv.begin(IsolationLevel::Snapshot).await.unwrap();
    catalog
        .delete_column("retired_column", "retiring")
        .await
        .unwrap();
    let column_reclamation = reclamation_by_kind(&engine, ReclamationKind::Column).await;
    finish_reclamation(&engine, &column_reclamation, 1).await;
    let current = catalog.get_table("retired_column").await.unwrap().unwrap();
    let current_rows = engine.scan_table_rows(&current).await.unwrap();
    assert!(
        current_rows
            .iter()
            .all(|row| row.contains_key("kept") && !row.contains_key("retiring"))
    );
    let old_rows = {
        let view = TransactionView(&*column_snapshot);
        row_store::scan_table_columns(&view, &column_table, &column_table.columns)
            .await
            .unwrap()
    };
    assert!(old_rows.iter().all(|row| row.contains_key("retiring")));
    column_snapshot.rollback();

    let index_table = catalog
        .create_table(TableDef {
            id: SchemaId::new(270).unwrap(),
            name: "retired_index".into(),
            columns: vec![
                column(270, "id", ScalarType::Text, false),
                column(271, "value", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: vec![IndexDef {
                name: "retired_index_value_idx".into(),
                columns: vec!["value".into()],
                unique: false,
            }],
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    for id in ["a", "b"] {
        engine
            .create(
                "retired_index",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("value".into(), Value::Text(format!("value-{id}"))),
                ]),
            )
            .await
            .unwrap();
    }
    let retired_index = index_table
        .index("retired_index_value_idx")
        .unwrap()
        .clone();
    let index_prefix = codec::index_prefix(&index_table, &retired_index.id);
    let index_snapshot = kv.begin(IsolationLevel::Snapshot).await.unwrap();
    catalog
        .delete_index("retired_index", "retired_index_value_idx")
        .await
        .unwrap();
    let index_reclamation = reclamation_by_kind(&engine, ReclamationKind::Index).await;
    finish_reclamation(&engine, &index_reclamation, 1).await;
    assert_eq!(count_prefix(&engine, index_prefix.clone()).await, 0);
    let old_count = {
        let view = TransactionView(&*index_snapshot);
        let mut iterator = view
            .scan(KeyRange {
                start: Some(Bytes::from(index_prefix.clone())),
                end: prefix_end(&index_prefix).map(Bytes::from),
            })
            .await
            .unwrap();
        let mut count = 0;
        while iterator.next().await.unwrap().is_some() {
            count += 1;
        }
        count
    };
    assert_eq!(old_count, 2);
    index_snapshot.rollback();
}

#[tokio::test]
async fn index_build_replays_updates_deletes_and_inserts_that_overtake_a_staged_batch() {
    let kv = Arc::new(
        Store::memory("schema-job-index-mutation-race")
            .await
            .unwrap(),
    );
    let catalog = catalog::Catalog::new(kv.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(50).unwrap(),
            name: "racing_items".into(),
            columns: vec![
                column(50, "id", ScalarType::Text, false),
                column(51, "status", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let hook = Arc::new(BlockingHook::new(Boundary::FirstPhysicalBatch));
    let engine = Arc::new(Engine::new(kv).with_event_hook(hook.clone()));
    for (id, status) in [("a", "open"), ("b", "closed"), ("c", "pending")] {
        engine
            .create(
                "racing_items",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("status".into(), Value::Text(status.into())),
                ]),
            )
            .await
            .unwrap();
    }
    let transition = start_index(
        &engine,
        table.schema_id,
        "racing_status_idx",
        "status",
        Vec::new(),
    )
    .await;
    let owner = engine
        .claim_schema_transition(&transition.id)
        .await
        .unwrap();

    let worker_engine = engine.clone();
    let transition_id = transition.id.clone();
    let worker = tokio::spawn(async move {
        worker_engine
            .step_schema_transition(&transition_id, owner, 1)
            .await
    });
    hook.wait_until_reached().await;

    engine
        .update_many(
            "racing_items",
            text_input_type(&["id", "status"]),
            vec![Row::from([
                ("id".into(), Value::Text("a".into())),
                ("status".into(), Value::Text("renamed".into())),
            ])],
        )
        .await
        .unwrap();
    engine
        .delete_many(
            "racing_items",
            text_input_type(&["id"]),
            vec![Row::from([("id".into(), Value::Text("b".into()))])],
        )
        .await
        .unwrap();
    engine
        .create(
            "racing_items",
            Row::from([
                ("id".into(), Value::Text("d".into())),
                ("status".into(), Value::Text("new".into())),
            ]),
        )
        .await
        .unwrap();

    hook.release();
    worker.await.unwrap().unwrap();
    run_transition(&engine, &transition.id).await;
    assert_exact_index(&engine, "racing_items", "racing_status_idx").await;
}

#[tokio::test]
async fn cancellation_overtakes_staged_ready_publication_without_resurrecting_the_index() {
    let kv = Arc::new(
        Store::memory("schema-job-index-cancel-publication")
            .await
            .unwrap(),
    );
    let catalog = catalog::Catalog::new(kv.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(60).unwrap(),
            name: "cancel_race".into(),
            columns: vec![
                column(60, "id", ScalarType::Text, false),
                column(61, "status", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let hook = Arc::new(BlockingHook::new(Boundary::ReadyPublication));
    let engine = Arc::new(Engine::new(kv).with_event_hook(hook.clone()));
    engine
        .create(
            "cancel_race",
            Row::from([
                ("id".into(), Value::Text("a".into())),
                ("status".into(), Value::Text("open".into())),
            ]),
        )
        .await
        .unwrap();
    let transition = start_index(
        &engine,
        table.schema_id,
        "cancelled_status_idx",
        "status",
        Vec::new(),
    )
    .await;
    let owner = engine
        .claim_schema_transition(&transition.id)
        .await
        .unwrap();

    let worker_engine = engine.clone();
    let transition_id = transition.id.clone();
    let worker = tokio::spawn(async move {
        for _ in 0..32 {
            let step = worker_engine
                .step_schema_transition(&transition_id, owner, 1)
                .await?;
            if step.transition.state.is_terminal() {
                return Ok::<_, crate::engine::exec::Error>(step);
            }
        }
        panic!("transition did not reach publication");
    });
    hook.wait_until_reached().await;

    let cancelled = engine.cancel_schema_transition(&transition.id).await;
    hook.release();
    assert_eq!(cancelled.unwrap().state, TransitionState::Cancelled);
    let worker_error = worker.await.unwrap().unwrap_err();
    assert_eq!(worker_error.kind(), ErrorKind::Conflict);

    let current = engine
        .inspect_schema_transition(&transition.id)
        .await
        .unwrap();
    assert_eq!(current.state, TransitionState::Cancelled);
    let current_table = catalog.get_table("cancel_race").await.unwrap().unwrap();
    assert!(
        current_table
            .indexes
            .iter()
            .all(|index| index.name != "cancelled_status_idx")
    );
    engine
        .create(
            "cancel_race",
            Row::from([
                ("id".into(), Value::Text("after".into())),
                ("status".into(), Value::Text("cancel".into())),
            ]),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn owner_takeover_overtakes_a_staged_batch_without_leaking_old_owner_writes() {
    let kv = Arc::new(
        Store::memory("schema-job-index-owner-takeover")
            .await
            .unwrap(),
    );
    let catalog = catalog::Catalog::new(kv.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(70).unwrap(),
            name: "takeover_items".into(),
            columns: vec![
                column(70, "id", ScalarType::Text, false),
                column(71, "status", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let hook = Arc::new(BlockingHook::new(Boundary::FirstPhysicalBatch));
    let engine = Arc::new(Engine::new(kv).with_event_hook(hook.clone()));
    for (id, status) in [("a", "open"), ("b", "closed"), ("c", "pending")] {
        engine
            .create(
                "takeover_items",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("status".into(), Value::Text(status.into())),
                ]),
            )
            .await
            .unwrap();
    }
    let transition = start_index(
        &engine,
        table.schema_id,
        "takeover_status_idx",
        "status",
        Vec::new(),
    )
    .await;
    let old_owner = engine
        .claim_schema_transition(&transition.id)
        .await
        .unwrap();

    let worker_engine = engine.clone();
    let transition_id = transition.id.clone();
    let old_worker = tokio::spawn(async move {
        worker_engine
            .step_schema_transition(&transition_id, old_owner, 1)
            .await
    });
    hook.wait_until_reached().await;

    let new_owner = engine
        .claim_schema_transition(&transition.id)
        .await
        .unwrap();
    assert!(new_owner > old_owner);
    hook.release();
    let old_error = old_worker.await.unwrap().unwrap_err();
    assert_eq!(old_error.kind(), ErrorKind::Conflict);

    let after_takeover = engine
        .inspect_schema_transition(&transition.id)
        .await
        .unwrap();
    assert_eq!(after_takeover.owner_epoch, new_owner);
    assert_eq!(after_takeover.rows_scanned, 0);
    for _ in 0..32 {
        let step = engine
            .step_schema_transition(&transition.id, new_owner, 1)
            .await
            .unwrap();
        if step.transition.state.is_terminal() {
            break;
        }
    }
    assert_exact_index(&engine, "takeover_items", "takeover_status_idx").await;
}

#[tokio::test]
async fn duplicate_unique_claim_overtaking_a_staged_scan_fails_the_build_durably() {
    let kv = Arc::new(Store::memory("schema-job-unique-claim-race").await.unwrap());
    let catalog = catalog::Catalog::new(kv.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(80).unwrap(),
            name: "unique_race".into(),
            columns: vec![
                column(80, "id", ScalarType::Text, false),
                column(81, "token", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let hook = Arc::new(BlockingHook::new(Boundary::FirstPhysicalBatch));
    let engine = Arc::new(Engine::new(kv).with_event_hook(hook.clone()));
    for (id, token) in [("a", "shared"), ("b", "different")] {
        engine
            .create(
                "unique_race",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("token".into(), Value::Text(token.into())),
                ]),
            )
            .await
            .unwrap();
    }
    let transition = start_index_with_uniqueness(
        &engine,
        table.schema_id,
        "unique_token_idx",
        "token",
        true,
        Vec::new(),
    )
    .await;
    let owner = engine
        .claim_schema_transition(&transition.id)
        .await
        .unwrap();

    let worker_engine = engine.clone();
    let transition_id = transition.id.clone();
    let worker = tokio::spawn(async move {
        worker_engine
            .step_schema_transition(&transition_id, owner, 1)
            .await
    });
    hook.wait_until_reached().await;
    engine
        .create(
            "unique_race",
            Row::from([
                ("id".into(), Value::Text("c".into())),
                ("token".into(), Value::Text("shared".into())),
            ]),
        )
        .await
        .unwrap();
    hook.release();
    let error = worker.await.unwrap().unwrap_err();
    assert!(error.is_conflict());

    run_transition(&engine, &transition.id).await;
    let current = engine
        .inspect_schema_transition(&transition.id)
        .await
        .unwrap();
    assert_eq!(current.state, TransitionState::Failed);
    assert!(current.last_error.contains("shared by multiple rows"));
    let current_table = catalog.get_table("unique_race").await.unwrap().unwrap();
    assert!(
        current_table
            .indexes
            .iter()
            .all(|index| index.name != "unique_token_idx")
    );
}

#[tokio::test]
async fn retention_pin_overtaking_a_staged_reclamation_prevents_physical_deletion() {
    let kv = Arc::new(
        Store::memory("schema-job-reclamation-pin-race")
            .await
            .unwrap(),
    );
    let catalog = catalog::Catalog::new(kv.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(90).unwrap(),
            name: "retained_items".into(),
            columns: vec![
                column(90, "id", ScalarType::Text, false),
                column(91, "status", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let bootstrap = Engine::new(kv.clone());
    for (id, status) in [("a", "open"), ("b", "closed")] {
        bootstrap
            .create(
                "retained_items",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("status".into(), Value::Text(status.into())),
                ]),
            )
            .await
            .unwrap();
    }
    let transition = start_index(
        &bootstrap,
        table.schema_id,
        "retired_status_idx",
        "status",
        Vec::new(),
    )
    .await;
    let transition_owner = bootstrap
        .claim_schema_transition(&transition.id)
        .await
        .unwrap();
    bootstrap
        .step_schema_transition(&transition.id, transition_owner, 1)
        .await
        .unwrap();
    bootstrap
        .cancel_schema_transition(&transition.id)
        .await
        .unwrap();
    let reclamation = {
        let transaction = kv.begin(IsolationLevel::Snapshot).await.unwrap();
        let values = {
            let mut view = TransactionView(&*transaction);
            store::list_reclamations(&mut view).await.unwrap()
        };
        transaction.rollback();
        values
            .into_iter()
            .find(|value| value.transition_id == transition.id)
            .unwrap()
    };
    let prefix = codec::index_prefix(&table, &transition.index.id);
    assert_eq!(count_prefix(&bootstrap, prefix.clone()).await, 1);

    let hook = Arc::new(BlockingHook::new(Boundary::ReclamationPhysicalBatch));
    let engine = Arc::new(Engine::new(kv.clone()).with_event_hook(hook.clone()));
    let owner = engine
        .claim_reclamation(&reclamation.id)
        .await
        .unwrap()
        .unwrap();
    let worker_engine = engine.clone();
    let reclamation_id = reclamation.id.clone();
    let worker = tokio::spawn(async move {
        worker_engine
            .step_reclamation(&reclamation_id, owner, 1)
            .await
    });
    hook.wait_until_reached().await;

    let pin_id = crate::engine::catalog::identity::RetentionPinId::from("pin-racing-reclamation");
    let transaction = kv
        .begin(IsolationLevel::SerializableSnapshot)
        .await
        .unwrap();
    {
        let mut view = TransactionView(&*transaction);
        store::save_retention_pin(
            &mut view,
            RetentionPin {
                id: pin_id.clone(),
                owner_kind: RetentionOwnerKind::PhysicalReader,
                owner_id: "reader-1".into(),
                resource: RetentionResource {
                    kind: RetentionResourceKind::PhysicalIndex,
                    table_id: table.id.clone(),
                    table_schema_id: None,
                    column_id: Default::default(),
                    index_id: transition.index.id.clone(),
                    definition_generation: Default::default(),
                    write_protocol_generation: Default::default(),
                    transition_id: Default::default(),
                    data_position: Default::default(),
                },
                created_at: Timestamp::default(),
            },
            Timestamp::test_value(),
        )
        .await
        .unwrap();
    }
    transaction.commit().await.unwrap();
    hook.release();

    let error = worker.await.unwrap().unwrap_err();
    assert!(error.is_conflict());
    assert_eq!(count_prefix(&engine, prefix.clone()).await, 1);
    let blocked = engine
        .step_reclamation(&reclamation.id, owner, 1)
        .await
        .unwrap_err();
    assert!(blocked.is_conflict());
    assert_eq!(count_prefix(&engine, prefix.clone()).await, 1);

    let transaction = kv
        .begin(IsolationLevel::SerializableSnapshot)
        .await
        .unwrap();
    {
        let mut view = TransactionView(&*transaction);
        store::delete_retention_pin(&mut view, &pin_id)
            .await
            .unwrap();
    }
    transaction.commit().await.unwrap();
    for _ in 0..32 {
        let (current, _) = engine
            .step_reclamation(&reclamation.id, owner, 1)
            .await
            .unwrap();
        if current.state == ReclamationState::Reclaimed {
            break;
        }
    }
    assert_eq!(count_prefix(&engine, prefix).await, 0);
}

#[tokio::test]
async fn reclamation_owner_takeover_rolls_back_the_old_owners_staged_deletes() {
    let kv = Arc::new(
        Store::memory("schema-job-reclamation-owner-race")
            .await
            .unwrap(),
    );
    let catalog = catalog::Catalog::new(kv.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(95).unwrap(),
            name: "takeover_reclamation".into(),
            columns: vec![
                column(95, "id", ScalarType::Text, false),
                column(96, "status", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let bootstrap = Engine::new(kv.clone());
    for (id, status) in [("a", "open"), ("b", "closed")] {
        bootstrap
            .create(
                "takeover_reclamation",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("status".into(), Value::Text(status.into())),
                ]),
            )
            .await
            .unwrap();
    }
    let transition = start_index(
        &bootstrap,
        table.schema_id,
        "retired_takeover_idx",
        "status",
        Vec::new(),
    )
    .await;
    let transition_owner = bootstrap
        .claim_schema_transition(&transition.id)
        .await
        .unwrap();
    bootstrap
        .step_schema_transition(&transition.id, transition_owner, 2)
        .await
        .unwrap();
    bootstrap
        .cancel_schema_transition(&transition.id)
        .await
        .unwrap();
    let reclamation = {
        let transaction = kv.begin(IsolationLevel::Snapshot).await.unwrap();
        let values = {
            let mut view = TransactionView(&*transaction);
            store::list_reclamations(&mut view).await.unwrap()
        };
        transaction.rollback();
        values
            .into_iter()
            .find(|value| value.transition_id == transition.id)
            .unwrap()
    };
    let prefix = codec::index_prefix(&table, &transition.index.id);
    assert_eq!(count_prefix(&bootstrap, prefix.clone()).await, 2);

    let hook = Arc::new(BlockingHook::new(Boundary::ReclamationPhysicalBatch));
    let engine = Arc::new(Engine::new(kv).with_event_hook(hook.clone()));
    let old_owner = engine
        .claim_reclamation(&reclamation.id)
        .await
        .unwrap()
        .unwrap();
    let worker_engine = engine.clone();
    let reclamation_id = reclamation.id.clone();
    let old_worker = tokio::spawn(async move {
        worker_engine
            .step_reclamation(&reclamation_id, old_owner, 1)
            .await
    });
    hook.wait_until_reached().await;
    let new_owner = engine
        .claim_reclamation(&reclamation.id)
        .await
        .unwrap()
        .unwrap();
    assert!(new_owner > old_owner);
    hook.release();
    let old_error = old_worker.await.unwrap().unwrap_err();
    assert!(old_error.is_conflict());
    assert_eq!(count_prefix(&engine, prefix.clone()).await, 2);

    let current = {
        let transaction = engine.store.begin(IsolationLevel::Snapshot).await.unwrap();
        let value = {
            let mut view = TransactionView(&*transaction);
            store::get_reclamation(&mut view, &reclamation.id)
                .await
                .unwrap()
                .unwrap()
        };
        transaction.rollback();
        value
    };
    assert_eq!(current.owner_epoch, new_owner);
    assert_eq!(current.items_reclaimed, 0);
    for _ in 0..32 {
        let (current, _) = engine
            .step_reclamation(&reclamation.id, new_owner, 1)
            .await
            .unwrap();
        if current.state == ReclamationState::Reclaimed {
            break;
        }
    }
    assert_eq!(count_prefix(&engine, prefix).await, 0);
}

#[tokio::test]
async fn replacement_scan_cannot_overwrite_a_foreground_update_that_overtakes_its_batch() {
    let kv = Arc::new(
        Store::memory("schema-job-replacement-write-race")
            .await
            .unwrap(),
    );
    let catalog = catalog::Catalog::new(kv.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(100).unwrap(),
            name: "replacement_race".into(),
            columns: vec![
                column(100, "id", ScalarType::Text, false),
                column(101, "value", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let bootstrap = Engine::new(kv.clone());
    for (id, value) in [("a", "41"), ("b", "42")] {
        bootstrap
            .create(
                "replacement_race",
                Row::from([
                    ("id".into(), Value::Text(id.into())),
                    ("value".into(), Value::Text(value.into())),
                ]),
            )
            .await
            .unwrap();
    }
    let transition = start_replacement(
        &bootstrap,
        table.schema_id,
        SchemaId::new(101).unwrap(),
        ScalarType::Int64,
    )
    .await;
    let hook = Arc::new(BlockingHook::new(Boundary::FirstPhysicalBatch));
    let engine = Arc::new(Engine::new(kv).with_event_hook(hook.clone()));
    let owner = engine
        .claim_schema_transition(&transition.id)
        .await
        .unwrap();
    let worker_engine = engine.clone();
    let transition_id = transition.id.clone();
    let worker = tokio::spawn(async move {
        worker_engine
            .step_schema_transition(&transition_id, owner, 1)
            .await
    });
    hook.wait_until_reached().await;

    engine
        .update_many(
            "replacement_race",
            text_input_type(&["id", "value"]),
            vec![Row::from([
                ("id".into(), Value::Text("a".into())),
                ("value".into(), Value::Text("99".into())),
            ])],
        )
        .await
        .unwrap();
    hook.release();
    let error = worker.await.unwrap().unwrap_err();
    assert!(error.is_conflict());

    run_transition(&engine, &transition.id).await;
    let current = catalog
        .get_table("replacement_race")
        .await
        .unwrap()
        .unwrap();
    let value = current.column("value").unwrap();
    assert_eq!(value.scalar_type, ScalarType::Int64);
    let transaction = engine.store.begin(IsolationLevel::Snapshot).await.unwrap();
    let rows = {
        let view = TransactionView(&*transaction);
        row_store::scan_table_columns(&view, &current, &current.columns)
            .await
            .unwrap()
    };
    transaction.rollback();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().any(|row| {
            row["id"] == Value::Text("a".into()) && row["value"] == Value::Int64(99)
        })
    );
    assert!(
        rows.iter().any(|row| {
            row["id"] == Value::Text("b".into()) && row["value"] == Value::Int64(42)
        })
    );
}

#[tokio::test]
async fn constraint_scan_cannot_restore_a_violation_fixed_by_an_overtaking_write() {
    let kv = Arc::new(
        Store::memory("schema-job-constraint-write-race")
            .await
            .unwrap(),
    );
    let catalog = catalog::Catalog::new(kv.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(110).unwrap(),
            name: "constraint_race".into(),
            columns: vec![
                column(110, "id", ScalarType::Text, false),
                column(111, "label", ScalarType::Text, true),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let bootstrap = Engine::new(kv.clone());
    bootstrap
        .create(
            "constraint_race",
            Row::from([
                ("id".into(), Value::Text("a".into())),
                ("label".into(), Value::Null(ScalarType::Text)),
            ]),
        )
        .await
        .unwrap();
    bootstrap
        .create(
            "constraint_race",
            Row::from([
                ("id".into(), Value::Text("b".into())),
                ("label".into(), Value::Text("known".into())),
            ]),
        )
        .await
        .unwrap();
    let transition = start_not_null_constraint(
        &bootstrap,
        table.schema_id,
        SchemaId::new(111).unwrap(),
        "label_not_null",
    )
    .await;
    let hook = Arc::new(BlockingHook::new(Boundary::FirstPhysicalBatch));
    let engine = Arc::new(Engine::new(kv).with_event_hook(hook.clone()));
    let owner = engine
        .claim_schema_transition(&transition.id)
        .await
        .unwrap();
    engine
        .step_schema_transition(&transition.id, owner, 1)
        .await
        .unwrap();
    let worker_engine = engine.clone();
    let transition_id = transition.id.clone();
    let worker = tokio::spawn(async move {
        worker_engine
            .step_schema_transition(&transition_id, owner, 1)
            .await
    });
    hook.wait_until_reached().await;

    engine
        .update_many(
            "constraint_race",
            text_input_type(&["id", "label"]),
            vec![Row::from([
                ("id".into(), Value::Text("a".into())),
                ("label".into(), Value::Text("fixed".into())),
            ])],
        )
        .await
        .unwrap();
    let rejected = engine
        .update_many(
            "constraint_race",
            text_input_type(&["id", "label"]),
            vec![Row::from([
                ("id".into(), Value::Text("b".into())),
                ("label".into(), Value::Null(ScalarType::Text)),
            ])],
        )
        .await
        .unwrap_err();
    assert_eq!(rejected.kind(), ErrorKind::ConstraintViolation);
    hook.release();
    let error = worker.await.unwrap().unwrap_err();
    assert!(error.is_conflict());

    run_transition(&engine, &transition.id).await;
    let current_transition = engine
        .inspect_schema_transition(&transition.id)
        .await
        .unwrap();
    assert_eq!(current_transition.state, TransitionState::Ready);
    let current = catalog.get_table("constraint_race").await.unwrap().unwrap();
    assert!(!current.column("label").unwrap().nullable);
    let transaction = engine.store.begin(IsolationLevel::Snapshot).await.unwrap();
    let rows = {
        let view = TransactionView(&*transaction);
        row_store::scan_table_columns(&view, &current, &current.columns)
            .await
            .unwrap()
    };
    transaction.rollback();
    assert!(rows.iter().all(|row| !row["label"].is_null()));
    assert!(rows.iter().any(|row| {
        row["id"] == Value::Text("a".into()) && row["label"] == Value::Text("fixed".into())
    }));
}

#[tokio::test]
async fn prerequisite_cancellation_racing_activation_fails_a_multi_table_dependency_chain() {
    let kv = Arc::new(
        Store::memory("schema-job-dependency-cancel-race")
            .await
            .unwrap(),
    );
    let catalog = catalog::Catalog::new(kv.clone());
    let first_table = catalog
        .create_table(TableDef {
            id: SchemaId::new(120).unwrap(),
            name: "dependency_first".into(),
            columns: vec![
                column(120, "id", ScalarType::Text, false),
                column(121, "value", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let second_table = catalog
        .create_table(TableDef {
            id: SchemaId::new(130).unwrap(),
            name: "dependency_second".into(),
            columns: vec![
                column(130, "id", ScalarType::Text, false),
                column(131, "value", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let bootstrap = Engine::new(kv.clone());
    let first = start_index(
        &bootstrap,
        first_table.schema_id,
        "first_value_idx",
        "value",
        Vec::new(),
    )
    .await;
    let second = start_index(
        &bootstrap,
        second_table.schema_id,
        "second_value_idx",
        "value",
        vec![first.id.clone()],
    )
    .await;
    let third = start_index(
        &bootstrap,
        first_table.schema_id,
        "third_value_idx",
        "value",
        vec![second.id.clone()],
    )
    .await;
    assert_eq!(second.state, TransitionState::Waiting);
    assert_eq!(third.state, TransitionState::Waiting);

    let hook = Arc::new(BlockingHook::new(Boundary::ActivationCommitStarted));
    let engine = Arc::new(Engine::new(kv).with_event_hook(hook.clone()));
    let worker_engine = engine.clone();
    let second_id = second.id.clone();
    let activation = tokio::spawn(async move {
        worker_engine
            .activate_waiting_schema_transition(&second_id)
            .await
    });
    hook.wait_until_reached().await;
    let cancelled = engine.cancel_schema_transition(&first.id).await.unwrap();
    assert_eq!(cancelled.state, TransitionState::Cancelled);
    hook.release();
    match activation.await.unwrap() {
        Ok(transition) => assert_eq!(transition.state, TransitionState::Waiting),
        Err(error) => assert!(error.is_conflict()),
    }

    let failed_second = engine
        .activate_waiting_schema_transition(&second.id)
        .await
        .unwrap();
    assert_eq!(failed_second.state, TransitionState::Failed);
    assert!(
        failed_second
            .last_error
            .contains("completed in state Cancelled")
    );
    let failed_third = engine
        .activate_waiting_schema_transition(&third.id)
        .await
        .unwrap();
    assert_eq!(failed_third.state, TransitionState::Failed);
    assert!(
        failed_third
            .last_error
            .contains("completed in state Failed")
    );

    for table_name in ["dependency_first", "dependency_second"] {
        let table = catalog.get_table(table_name).await.unwrap().unwrap();
        assert!(table.indexes.is_empty());
        engine
            .create(
                table_name,
                Row::from([
                    ("id".into(), Value::Text("after".into())),
                    ("value".into(), Value::Text("still-writable".into())),
                ]),
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn replacement_cancellation_overtakes_staged_publication_and_removes_dual_writes() {
    let kv = Arc::new(
        Store::memory("schema-job-replacement-cancel-race")
            .await
            .unwrap(),
    );
    let catalog = catalog::Catalog::new(kv.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(140).unwrap(),
            name: "cancel_replacement".into(),
            columns: vec![
                column(140, "id", ScalarType::Text, false),
                column(141, "value", ScalarType::Text, false),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let bootstrap = Engine::new(kv.clone());
    bootstrap
        .create(
            "cancel_replacement",
            Row::from([
                ("id".into(), Value::Text("a".into())),
                ("value".into(), Value::Text("41".into())),
            ]),
        )
        .await
        .unwrap();
    let transition = start_replacement(
        &bootstrap,
        table.schema_id,
        SchemaId::new(141).unwrap(),
        ScalarType::Int64,
    )
    .await;
    let hook = Arc::new(BlockingHook::new(Boundary::ReadyPublication));
    let engine = Arc::new(Engine::new(kv).with_event_hook(hook.clone()));
    let owner = engine
        .claim_schema_transition(&transition.id)
        .await
        .unwrap();
    let worker_engine = engine.clone();
    let transition_id = transition.id.clone();
    let worker = tokio::spawn(async move {
        for _ in 0..32 {
            let step = worker_engine
                .step_schema_transition(&transition_id, owner, 1)
                .await?;
            if step.transition.state.is_terminal() {
                return Ok::<_, crate::engine::exec::Error>(step);
            }
        }
        panic!("replacement did not reach publication");
    });
    hook.wait_until_reached().await;
    let cancelled = engine.cancel_schema_transition(&transition.id).await;
    hook.release();
    assert_eq!(cancelled.unwrap().state, TransitionState::Cancelled);
    assert!(worker.await.unwrap().unwrap_err().is_conflict());

    let current = catalog
        .get_table("cancel_replacement")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        current.column("value").unwrap().scalar_type,
        ScalarType::Text
    );
    engine
        .create(
            "cancel_replacement",
            Row::from([
                ("id".into(), Value::Text("after".into())),
                ("value".into(), Value::Text("not-an-int".into())),
            ]),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn constraint_cancellation_overtakes_staged_publication_and_removes_enforcement() {
    let kv = Arc::new(
        Store::memory("schema-job-constraint-cancel-race")
            .await
            .unwrap(),
    );
    let catalog = catalog::Catalog::new(kv.clone());
    let table = catalog
        .create_table(TableDef {
            id: SchemaId::new(150).unwrap(),
            name: "cancel_constraint".into(),
            columns: vec![
                column(150, "id", ScalarType::Text, false),
                column(151, "label", ScalarType::Text, true),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .await
        .unwrap();
    let bootstrap = Engine::new(kv.clone());
    bootstrap
        .create(
            "cancel_constraint",
            Row::from([
                ("id".into(), Value::Text("a".into())),
                ("label".into(), Value::Text("known".into())),
            ]),
        )
        .await
        .unwrap();
    let transition = start_not_null_constraint(
        &bootstrap,
        table.schema_id,
        SchemaId::new(151).unwrap(),
        "cancelled_not_null",
    )
    .await;
    let hook = Arc::new(BlockingHook::new(Boundary::ReadyPublication));
    let engine = Arc::new(Engine::new(kv).with_event_hook(hook.clone()));
    let owner = engine
        .claim_schema_transition(&transition.id)
        .await
        .unwrap();
    let worker_engine = engine.clone();
    let transition_id = transition.id.clone();
    let worker = tokio::spawn(async move {
        for _ in 0..32 {
            let step = worker_engine
                .step_schema_transition(&transition_id, owner, 1)
                .await?;
            if step.transition.state.is_terminal() {
                return Ok::<_, crate::engine::exec::Error>(step);
            }
        }
        panic!("constraint did not reach publication");
    });
    hook.wait_until_reached().await;
    let cancelled = engine.cancel_schema_transition(&transition.id).await;
    hook.release();
    assert_eq!(cancelled.unwrap().state, TransitionState::Cancelled);
    assert!(worker.await.unwrap().unwrap_err().is_conflict());

    let current = catalog
        .get_table("cancel_constraint")
        .await
        .unwrap()
        .unwrap();
    assert!(current.column("label").unwrap().nullable);
    engine
        .create(
            "cancel_constraint",
            Row::from([
                ("id".into(), Value::Text("after".into())),
                ("label".into(), Value::Null(ScalarType::Text)),
            ]),
        )
        .await
        .unwrap();
}
