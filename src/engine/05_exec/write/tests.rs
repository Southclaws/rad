use std::sync::Arc;

use crate::engine::catalog::identity::{
    AccessGeneration, DefinitionGeneration, ExistenceGeneration, SchemaId, ValueGeneration,
    WriteProtocolGeneration,
};
use crate::engine::catalog::model::{
    Column, Index, IndexDeltaSink, IndexState, ScalarType, Table, Timestamp, WriteProtocol,
};
use crate::engine::catalog::store;
use crate::engine::kv::fault::{
    FaultAction, FaultController, FaultRule, FaultingKv, Operation, Target, TracePhase,
};
use crate::engine::kv::slatedb::Store;
use crate::engine::kv::{
    ErrorKind as KvErrorKind, IsolationLevel, Kv, TransactionView, TransactionalKv,
};
use crate::engine::lir::{Row, Value};

use super::{delete, insert, replace};
use crate::engine::exec::{ErrorKind, codec};

struct Fixture {
    store: Arc<Store>,
    table: Table,
    row: Row,
    primary_key: Vec<u8>,
    ready_index_key: Vec<u8>,
}

async fn fixture(name: &str) -> Fixture {
    let mut store = Store::memory(name).await.unwrap();
    let table = table();
    let row = row("a", 10);
    let primary_key = codec::encode_row_tuple(&row, &table.primary_key).unwrap();
    let ready = ready_index();
    let tuple = codec::encode_row_tuple(&row, &ready.columns).unwrap();
    let ready_index_key = codec::index_key(&table, &ready.id, &tuple, &primary_key);
    store::save_write_protocol(
        &mut store,
        WriteProtocol {
            table_id: table.id.clone(),
            generation: table.write_protocol_generation,
            ready_indexes: vec![ready],
            delta_sinks: vec![IndexDeltaSink {
                transition_id: "tr-build".into(),
                index: building_index(),
                columns: vec!["score".into()],
                delta_hard_limit: 100,
            }],
            column_replacements: Vec::new(),
            constraint_checks: Vec::new(),
            finalization_gate: None,
        },
        Timestamp::test_value(),
    )
    .await
    .unwrap();
    Fixture {
        store: Arc::new(store),
        table,
        row,
        primary_key,
        ready_index_key,
    }
}

fn table() -> Table {
    Table {
        id: "table".into(),
        schema_id: SchemaId::new(1).unwrap(),
        name: "samples".into(),
        definition_generation: DefinitionGeneration::from(1),
        existence_generation: ExistenceGeneration::ZERO,
        write_protocol_generation: WriteProtocolGeneration::from(1),
        columns: vec![
            Column {
                id: "c1".into(),
                schema_id: SchemaId::new(1).unwrap(),
                name: "id".into(),
                value_generation: ValueGeneration::ZERO,
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                insert_default: None,
                missing_value: None,
            },
            Column {
                id: "c2".into(),
                schema_id: SchemaId::new(2).unwrap(),
                name: "score".into(),
                value_generation: ValueGeneration::ZERO,
                scalar_type: ScalarType::Int64,
                nullable: false,
                format: String::new(),
                insert_default: None,
                missing_value: None,
            },
        ],
        primary_key: vec!["id".into()],
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        constraints: Vec::new(),
    }
}

fn ready_index() -> Index {
    index("ready", "ready", IndexState::Ready)
}

fn building_index() -> Index {
    index("building", "building", IndexState::Building)
}

fn index(id: &str, name: &str, state: IndexState) -> Index {
    Index {
        id: id.into(),
        logical_id: format!("logical-{id}").into(),
        definition_generation: DefinitionGeneration::from(1),
        access_generation: AccessGeneration::from(1),
        state,
        name: name.into(),
        columns: vec!["score".into()],
        column_ids: vec!["c2".into()],
        unique: false,
    }
}

fn row(id: &str, score: i64) -> Row {
    Row::from([
        ("id".into(), Value::Text(id.into())),
        ("score".into(), Value::Int64(score)),
    ])
}

fn started_keys(controller: &FaultController, operation: Operation) -> Vec<Vec<u8>> {
    controller
        .trace()
        .into_iter()
        .filter(|event| event.phase == TracePhase::Started && event.operation == operation)
        .map(|event| match event.target {
            Target::Key { bytes } => bytes,
            target => panic!("expected key target, got {target:?}"),
        })
        .collect()
}

async fn insert_through(
    fixture: &Fixture,
    controller: FaultController,
) -> (
    crate::engine::exec::Result<()>,
    Box<dyn crate::engine::kv::Transaction>,
) {
    let inner: Arc<dyn TransactionalKv> = fixture.store.clone();
    let faulting = FaultingKv::new(inner, controller);
    let transaction = faulting
        .begin(IsolationLevel::SerializableSnapshot)
        .await
        .unwrap();
    let result = {
        let mut view = TransactionView(transaction.as_ref());
        insert(
            &mut view,
            &fixture.table,
            &fixture.row,
            &fixture.primary_key,
        )
        .await
    };
    (result, transaction)
}

#[tokio::test]
async fn insert_orders_base_row_ready_index_and_delta_append() {
    let fixture = fixture("write-order-insert").await;
    let controller = FaultController::default();
    let (result, transaction) = insert_through(&fixture, controller.clone()).await;
    result.unwrap();

    let data_key = codec::data_key(&fixture.table, &fixture.primary_key);
    assert_eq!(
        started_keys(&controller, Operation::TransactionPut),
        vec![
            data_key.clone(),
            fixture.ready_index_key.clone(),
            store::delta_key(&"tr-build".into(), 1),
            b"/rad/catalog/transition_delta_sequence/tr-build".to_vec(),
        ]
    );
    transaction.commit().await.unwrap();
    assert!(
        Kv::get(fixture.store.as_ref(), &data_key)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        Kv::get(fixture.store.as_ref(), &fixture.ready_index_key)
            .await
            .unwrap()
            .is_some()
    );
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn insert_failure_at_every_write_boundary_rolls_back_everything() {
    for occurrence in 1..=4 {
        let fixture = fixture(&format!("write-order-insert-fault-{occurrence}")).await;
        let controller = FaultController::new(vec![FaultRule {
            operation: Operation::TransactionPut,
            occurrence,
            action: FaultAction::ErrorBefore(KvErrorKind::Unavailable),
        }]);
        let (result, transaction) = insert_through(&fixture, controller).await;
        assert_eq!(result.unwrap_err().kind(), ErrorKind::Storage);
        transaction.rollback();

        let data_key = codec::data_key(&fixture.table, &fixture.primary_key);
        assert!(
            Kv::get(fixture.store.as_ref(), &data_key)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            Kv::get(fixture.store.as_ref(), &fixture.ready_index_key)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            Kv::get(
                fixture.store.as_ref(),
                &store::delta_key(&"tr-build".into(), 1)
            )
            .await
            .unwrap()
            .is_none()
        );
        fixture.store.close().await.unwrap();
    }
}

#[tokio::test]
async fn replace_and_delete_preserve_index_then_delta_order() {
    let fixture = fixture("write-order-replace-delete").await;
    let seed_controller = FaultController::default();
    let (result, seed) = insert_through(&fixture, seed_controller).await;
    result.unwrap();
    seed.commit().await.unwrap();

    let after = row("a", 20);
    let new_tuple = codec::encode_row_tuple(&after, &["score".into()]).unwrap();
    let new_index_key = codec::index_key(
        &fixture.table,
        &ready_index().id,
        &new_tuple,
        &fixture.primary_key,
    );
    let replace_controller = FaultController::default();
    let replace_inner: Arc<dyn TransactionalKv> = fixture.store.clone();
    let replace_store = FaultingKv::new(replace_inner, replace_controller.clone());
    let replace_transaction = replace_store
        .begin(IsolationLevel::SerializableSnapshot)
        .await
        .unwrap();
    {
        let mut view = TransactionView(replace_transaction.as_ref());
        replace(
            &mut view,
            &fixture.table,
            &fixture.row,
            &after,
            &fixture.primary_key,
        )
        .await
        .unwrap();
    }
    assert_eq!(
        started_keys(&replace_controller, Operation::TransactionDelete),
        vec![fixture.ready_index_key.clone()]
    );
    assert_eq!(
        started_keys(&replace_controller, Operation::TransactionPut),
        vec![
            codec::data_key(&fixture.table, &fixture.primary_key),
            new_index_key.clone(),
            store::delta_key(&"tr-build".into(), 2),
            b"/rad/catalog/transition_delta_sequence/tr-build".to_vec(),
            store::delta_key(&"tr-build".into(), 3),
            b"/rad/catalog/transition_delta_sequence/tr-build".to_vec(),
        ]
    );
    replace_transaction.commit().await.unwrap();

    let delete_controller = FaultController::default();
    let delete_inner: Arc<dyn TransactionalKv> = fixture.store.clone();
    let delete_store = FaultingKv::new(delete_inner, delete_controller.clone());
    let delete_transaction = delete_store
        .begin(IsolationLevel::SerializableSnapshot)
        .await
        .unwrap();
    {
        let mut view = TransactionView(delete_transaction.as_ref());
        delete(&mut view, &fixture.table, &after, &fixture.primary_key)
            .await
            .unwrap();
    }
    assert_eq!(
        started_keys(&delete_controller, Operation::TransactionDelete),
        vec![
            new_index_key,
            codec::data_key(&fixture.table, &fixture.primary_key),
        ]
    );
    assert_eq!(
        started_keys(&delete_controller, Operation::TransactionPut),
        vec![
            store::delta_key(&"tr-build".into(), 4),
            b"/rad/catalog/transition_delta_sequence/tr-build".to_vec(),
        ]
    );
    delete_transaction.rollback();
    fixture.store.close().await.unwrap();
}
