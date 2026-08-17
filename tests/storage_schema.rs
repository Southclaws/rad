//! Binds the storage JSON Schemas to the bytes the engine writes: every
//! structured durable value produced through the public store surface must
//! validate against the schema its keyspace declares in
//! `protocol/storage.keyform`. Complements the generated `storage_spec`
//! suite, which covers keys and binary codecs.

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use chrono::DateTime;
use rad::engine::catalog::identity::{
    AccessGeneration, CatalogVersion, DefinitionGeneration, ExistenceGeneration, OwnerEpoch,
    SchemaId, StorageGeneration, TransitionGeneration, ValueGeneration, WriteProtocolGeneration,
};
use rad::engine::catalog::model::{
    Column, Constraint, ConstraintKind, ConstraintState, DataPosition, ForeignKey, Index,
    IndexDelta, IndexDeltaOperation, IndexState, Reclamation, ReclamationKind, RetentionOwnerKind,
    RetentionPin, RetentionResource, RetentionResourceKind, ScalarType, SchemaTransition, Table,
    Timestamp, TransitionKind, TransitionState, TransitionWorkState, WriteProtocol,
};
use rad::engine::catalog::store;
use rad::engine::kv::slatedb::Store;
use rad::engine::kv::{KeyRange, Kv, TransactionalKv, keys, keyspace};

fn timestamp() -> Timestamp {
    Timestamp::from(DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp"))
}

fn column(id: &str, schema_id: u32, name: &str) -> Column {
    Column {
        id: id.into(),
        schema_id: SchemaId::new(schema_id).unwrap(),
        name: name.into(),
        value_generation: ValueGeneration::from(1),
        scalar_type: ScalarType::Text,
        nullable: false,
        format: String::new(),
        insert_default: None,
        missing_value: None,
    }
}

fn index() -> Index {
    Index {
        id: "i1".into(),
        logical_id: "ix1".into(),
        definition_generation: DefinitionGeneration::from(1),
        access_generation: AccessGeneration::from(1),
        state: IndexState::Ready,
        name: "by_id".into(),
        columns: vec!["id".into()],
        column_ids: vec!["c1".into()],
        unique: true,
    }
}

fn table() -> Table {
    Table {
        id: "t1".into(),
        schema_id: SchemaId::new(1).unwrap(),
        name: "users".into(),
        definition_generation: DefinitionGeneration::ZERO,
        existence_generation: ExistenceGeneration::from(1),
        write_protocol_generation: WriteProtocolGeneration::from(1),
        storage_generation: StorageGeneration::INITIAL,
        columns: vec![column("c1", 1, "id")],
        primary_key: vec!["id".into()],
        indexes: vec![index()],
        foreign_keys: vec![ForeignKey {
            id: "fk1".into(),
            name: "owner".into(),
            columns: vec!["id".into()],
            ref_table_id: "t2".into(),
            ref_columns: vec!["id".into()],
        }],
        constraints: vec![Constraint {
            id: "ct1".into(),
            definition_generation: DefinitionGeneration::from(1),
            name: "id_not_null".into(),
            kind: ConstraintKind::NotNull,
            state: ConstraintState::Valid,
            column_ids: vec!["c1".into()],
        }],
    }
}

fn transition() -> SchemaTransition {
    SchemaTransition {
        id: "tr1".into(),
        kind: TransitionKind::IndexBuild,
        object_id: "ix1".into(),
        state: TransitionState::Building,
        generation: TransitionGeneration::from(1),
        owner_epoch: OwnerEpoch::ZERO,
        source_catalog_version: CatalogVersion::from(1),
        base_position: DataPosition::new("1"),
        barrier_position: DataPosition::default(),
        table_id: "t1".into(),
        table_schema_id: SchemaId::new(1).unwrap(),
        affected_column_ids: Vec::new(),
        index: index(),
        index_request: None,
        column_replacement: None,
        replacement_request: None,
        constraint: None,
        constraint_request: None,
        prerequisites: Vec::new(),
        gate_table_ids: Vec::new(),
        cursor: vec![0x01],
        batch_id: 1,
        applied_delta: 0,
        delta_high_water: 0,
        delta_soft_limit: 0,
        delta_hard_limit: 0,
        work_state: TransitionWorkState::Normal,
        rows_scanned: 1,
        last_error: String::new(),
        created_at: timestamp(),
        updated_at: timestamp(),
        compacted_at: Timestamp::default(),
    }
}

async fn seed(database: &mut Store) {
    store::admit_storage_compatibility(database, true)
        .await
        .unwrap();
    let mut referenced = Table {
        id: "t2".into(),
        schema_id: SchemaId::new(2).unwrap(),
        name: "owners".into(),
        definition_generation: DefinitionGeneration::ZERO,
        existence_generation: ExistenceGeneration::from(1),
        write_protocol_generation: WriteProtocolGeneration::ZERO,
        storage_generation: StorageGeneration::INITIAL,
        columns: vec![column("c2", 1, "id")],
        primary_key: vec!["id".into()],
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        constraints: Vec::new(),
    };
    store::save_table(database, &mut referenced).await.unwrap();
    let mut table = table();
    store::save_table(database, &mut table).await.unwrap();
    store::bump_revision(database, timestamp()).await.unwrap();
    store::save_write_protocol(
        database,
        WriteProtocol {
            table_id: "t1".into(),
            generation: WriteProtocolGeneration::from(1),
            ready_indexes: vec![index()],
            delta_sinks: Vec::new(),
            column_replacements: Vec::new(),
            constraint_checks: Vec::new(),
            finalization_gate: None,
        },
        timestamp(),
    )
    .await
    .unwrap();
    store::create_transition(database, &transition())
        .await
        .unwrap();
    store::append_index_delta(
        database,
        &"tr1".into(),
        0,
        IndexDelta {
            id: String::new(),
            sequence: 0,
            operation: IndexDeltaOperation::Put,
            pk: vec![0x05, 0x61, 0x00, 0x01],
            tuple: vec![0x03, 0x80, 0, 0, 0, 0, 0, 0, 0],
        },
    )
    .await
    .unwrap();
    store::put_unique_claim(
        database,
        &"tr1".into(),
        &[0x03, 0x80, 0, 0, 0, 0, 0, 0, 0],
        &[0x05, 0x61, 0x00, 0x01],
    )
    .await
    .unwrap();
    store::save_retention_pin(
        database,
        RetentionPin {
            id: "p1".into(),
            owner_kind: RetentionOwnerKind::SchemaTransition,
            owner_id: "tr1".into(),
            resource: RetentionResource {
                kind: RetentionResourceKind::TableDefinition,
                table_id: "t1".into(),
                table_schema_id: Some(SchemaId::new(1).unwrap()),
                column_id: Default::default(),
                index_id: Default::default(),
                definition_generation: DefinitionGeneration::from(1),
                write_protocol_generation: WriteProtocolGeneration::ZERO,
                transition_id: "tr1".into(),
                data_position: DataPosition::default(),
            },
            created_at: timestamp(),
        },
        timestamp(),
    )
    .await
    .unwrap();
    let mut reclamation = Reclamation::pending(
        store::table_definition_reclamation_id(
            SchemaId::new(2).unwrap(),
            DefinitionGeneration::from(1),
        ),
        ReclamationKind::TableDefinition,
        CatalogVersion::from(2),
        timestamp(),
    );
    reclamation.table_schema_id = Some(SchemaId::new(2).unwrap());
    reclamation.definition_generation = DefinitionGeneration::from(1);
    store::queue_reclamation(database, reclamation, timestamp())
        .await
        .unwrap();
}

fn schema_validators() -> BTreeMap<String, jsonschema::Validator> {
    let mut validators = BTreeMap::new();
    for space in keyspace::KEYSPACES {
        let Some(reference) = space.value.strip_prefix("json schema ") else {
            continue;
        };
        if validators.contains_key(reference) {
            continue;
        }
        let path = format!(
            "{}/protocol/storage/{reference}.schema.yaml",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("read storage schema {path}: {error}");
        });
        let document: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
        let json = serde_json::to_value(document).unwrap();
        validators.insert(
            reference.to_owned(),
            jsonschema::validator_for(&json).unwrap(),
        );
    }
    validators
}

#[tokio::test]
async fn every_structured_durable_value_validates_against_its_storage_schema() {
    let mut database = Store::memory("storage-schema-conformance").await.unwrap();
    seed(&mut database).await;

    let validators = schema_validators();
    let mut validated: BTreeSet<String> = BTreeSet::new();
    let mut iterator = Kv::scan(
        &database,
        KeyRange::new(
            Bytes::copy_from_slice(keys::ROOT_MAGIC),
            Bytes::from_static(&[0x72, 0x38]),
        ),
    )
    .await
    .unwrap();
    while let Some(entry) = iterator.next().await.unwrap() {
        let tag = entry.key.get(keys::ROOT_MAGIC.len()).copied().unwrap();
        let space = keyspace::KEYSPACES
            .iter()
            .find(|space| space.tag == tag)
            .unwrap_or_else(|| panic!("key {:?} belongs to no allocated keyspace", entry.key));
        let Some(reference) = space.value.strip_prefix("json schema ") else {
            continue;
        };
        let value: serde_json::Value = serde_json::from_slice(&entry.value)
            .unwrap_or_else(|error| panic!("space {}: value is not JSON: {error}", space.name));
        let validator = &validators[reference];
        let errors: Vec<String> = validator
            .iter_errors(&value)
            .map(|error| format!("{}: {error}", error.instance_path()))
            .collect();
        assert!(
            errors.is_empty(),
            "space {} value violates {reference}: {errors:?}\n{value}",
            space.name
        );
        validated.insert(reference.to_owned());
    }
    drop(iterator);

    let declared: BTreeSet<String> = validators.keys().cloned().collect();
    assert_eq!(
        validated, declared,
        "every declared storage schema must be exercised by a seeded value"
    );
    database.close().await.unwrap();
}
