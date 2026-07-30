use bytes::{BufMut, Bytes};

use crate::engine::catalog::identity::{TableId, TransitionId, WriteProtocolGeneration};
use crate::engine::catalog::model::{
    IndexDelta, SchemaTransition, UniqueIndexClaim, WriteProtocol,
};
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::key_encoding::prefix_end;
use crate::engine::kv::{KeyRange, KvView};

use super::durable_json::{decode, encode};
use super::write_protocol_canonical::canonical_write_protocol;
use super::{current_revision, map_kv, queue_reclamation, write_protocol_reclamation_id};
use crate::engine::catalog::model::Timestamp;
use crate::engine::catalog::model::{Reclamation, ReclamationKind, Table};

const WRITE_PROTOCOL_PREFIX: &str = "/rad/catalog/fence/table/";
const WRITE_PROTOCOL_DEFINITION_PREFIX: &str = "/rad/catalog/object/write_protocol/";
const TRANSITION_PREFIX: &str = "/rad/catalog/transition/";
const TRANSITION_WAKE_KEY: &[u8] = b"/rad/catalog/meta/transition_seen";
const DELTA_PREFIX: &str = "/rad/catalog/transition_delta/";
const DELTA_SEQUENCE_PREFIX: &str = "/rad/catalog/transition_delta_sequence/";
const DELTA_APPLIED_PREFIX: &str = "/rad/catalog/transition_delta_applied/";
const UNIQUE_CLAIM_PREFIX: &str = "/rad/catalog/transition_unique_claim/";
const UNIQUE_VIOLATION_PREFIX: &str = "/rad/catalog/transition_unique_violation/";

pub fn write_protocol_key(table_id: &TableId) -> Vec<u8> {
    format!("{WRITE_PROTOCOL_PREFIX}{table_id}/write_protocol").into_bytes()
}

pub fn write_protocol_definition_key(
    table_id: &TableId,
    generation: WriteProtocolGeneration,
) -> Vec<u8> {
    format!(
        "{WRITE_PROTOCOL_DEFINITION_PREFIX}{table_id}/definition/{:020}",
        generation.get()
    )
    .into_bytes()
}

pub fn write_protocol_definition_range(table_id: &TableId) -> (Vec<u8>, Vec<u8>) {
    let start = format!("{WRITE_PROTOCOL_DEFINITION_PREFIX}{table_id}/definition/").into_bytes();
    let end = prefix_end(&start).expect("catalog prefix has an upper bound");
    (start, end)
}

pub async fn write_protocol_generation<V: KvView + ?Sized>(
    view: &mut V,
    table_id: &TableId,
) -> Result<Option<WriteProtocolGeneration>> {
    let Some(raw) = view
        .get(&write_protocol_key(table_id))
        .await
        .map_err(map_kv)?
    else {
        return Ok(None);
    };
    parse_generation("write protocol fence", table_id.as_str(), &raw).map(Some)
}

pub async fn read_write_protocol<V: KvView + ?Sized>(
    view: &mut V,
    table: &Table,
) -> Result<WriteProtocol> {
    let Some(generation) = write_protocol_generation(view, &table.id).await? else {
        if !table.write_protocol_generation.is_zero() {
            return Err(Error::message(
                ErrorKind::CatalogCorrupt,
                format!(
                    "catalog: table {:?} expects missing write protocol generation {}",
                    table.name, table.write_protocol_generation
                ),
            ));
        }
        return Ok(WriteProtocol {
            table_id: table.id.clone(),
            generation: WriteProtocolGeneration::ZERO,
            ready_indexes: table
                .indexes
                .iter()
                .filter(|index| index.is_ready())
                .cloned()
                .collect(),
            delta_sinks: Vec::new(),
            column_replacements: Vec::new(),
            constraint_checks: Vec::new(),
            finalization_gate: None,
        });
    };
    if generation != table.write_protocol_generation {
        return Err(Error::message(
            ErrorKind::Conflict,
            format!(
                "catalog: table {:?} write protocol changed from generation {} to {generation}",
                table.name, table.write_protocol_generation
            ),
        ));
    }
    let key = write_protocol_definition_key(&table.id, generation);
    let Some(raw) = view.get(&key).await.map_err(map_kv)? else {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!(
                "catalog: table {:?} write protocol definition {generation} is missing",
                table.name
            ),
        ));
    };
    let protocol: WriteProtocol = decode("write protocol definition", &table.name, &raw)?;
    if protocol.table_id != table.id || protocol.generation != generation {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!(
                "catalog: invalid write protocol definition for table {:?} at generation {generation}",
                table.name
            ),
        ));
    }
    Ok(protocol)
}

pub async fn save_write_protocol<V: KvView + ?Sized>(
    view: &mut V,
    protocol: WriteProtocol,
    now: Timestamp,
) -> Result<()> {
    if protocol.generation.is_zero() {
        return Err(Error::message(
            ErrorKind::InvalidInput,
            format!(
                "catalog: cannot publish write protocol generation zero for table {:?}",
                protocol.table_id
            ),
        ));
    }
    let protocol = canonical_write_protocol(protocol);
    let raw = encode(
        "write protocol definition",
        protocol.table_id.as_str(),
        &protocol,
    )?;
    if let Some(current) = write_protocol_generation(view, &protocol.table_id).await?
        && current != protocol.generation
    {
        let revision = current_revision(view).await?;
        let mut reclamation = Reclamation::pending(
            write_protocol_reclamation_id(&protocol.table_id, current),
            ReclamationKind::WriteProtocolDefinition,
            revision.version.next(),
            now,
        );
        reclamation.table_id = protocol.table_id.clone();
        reclamation.write_protocol_generation = current;
        queue_reclamation(view, reclamation, now).await?;
    }
    let definition_key = write_protocol_definition_key(&protocol.table_id, protocol.generation);
    if let Some(existing) = view.get(&definition_key).await.map_err(map_kv)? {
        if existing.as_ref() != raw.as_slice() {
            return Err(Error::message(
                ErrorKind::Conflict,
                format!(
                    "catalog: immutable write protocol {:?} generation {} already has different contents",
                    protocol.table_id, protocol.generation
                ),
            ));
        }
    } else {
        view.put(Bytes::from(definition_key), Bytes::from(raw))
            .await
            .map_err(map_kv)?;
    }
    view.put(
        Bytes::from(write_protocol_key(&protocol.table_id)),
        Bytes::from(protocol.generation.to_string()),
    )
    .await
    .map_err(map_kv)
}

fn transition_key(id: &TransitionId) -> Vec<u8> {
    format!("{TRANSITION_PREFIX}{id}").into_bytes()
}

pub async fn save_transition<V: KvView + ?Sized>(
    view: &mut V,
    transition: &SchemaTransition,
) -> Result<()> {
    validate_transition(transition)?;
    let raw = encode("transition", transition.id.as_str(), transition)?;
    view.put(
        Bytes::from(transition_key(&transition.id)),
        Bytes::from(raw),
    )
    .await
    .map_err(map_kv)
}

/// Persist a newly admitted transition and mark transition history present.
///
/// Product callers perform this inside the catalog transaction so the marker
/// and transition become visible atomically. Later checkpoints must use
/// [`save_transition`] and therefore do not contend on the database-wide
/// discovery marker.
pub async fn create_transition<V: KvView + ?Sized>(
    view: &mut V,
    transition: &SchemaTransition,
) -> Result<()> {
    save_transition(view, transition).await?;
    view.put(
        Bytes::from_static(TRANSITION_WAKE_KEY),
        Bytes::from_static(&[1]),
    )
    .await
    .map_err(map_kv)
}

pub async fn has_transition_history<V: KvView + ?Sized>(view: &mut V) -> Result<bool> {
    Ok(view
        .get(TRANSITION_WAKE_KEY)
        .await
        .map_err(map_kv)?
        .is_some())
}

pub async fn get_transition<V: KvView + ?Sized>(
    view: &mut V,
    id: &TransitionId,
) -> Result<Option<SchemaTransition>> {
    let Some(raw) = view.get(&transition_key(id)).await.map_err(map_kv)? else {
        return Ok(None);
    };
    decode_transition(id.as_str(), &raw).map(Some)
}

pub async fn list_transitions<V: KvView + ?Sized>(view: &mut V) -> Result<Vec<SchemaTransition>> {
    let prefix = TRANSITION_PREFIX.as_bytes();
    let mut iterator = view
        .scan(KeyRange::new(
            Bytes::copy_from_slice(prefix),
            Bytes::from(prefix_end(prefix).expect("catalog prefix has an upper bound")),
        ))
        .await
        .map_err(map_kv)?;
    let mut transitions = Vec::new();
    while let Some(entry) = iterator.next().await.map_err(map_kv)? {
        let id = entry
            .key
            .strip_prefix(prefix)
            .and_then(|value| std::str::from_utf8(value).ok())
            .ok_or_else(|| {
                Error::message(
                    ErrorKind::CatalogCorrupt,
                    "catalog: malformed transition key",
                )
            })?;
        transitions.push(decode_transition(id, &entry.value)?);
    }
    Ok(transitions)
}

fn decode_transition(id: &str, raw: &[u8]) -> Result<SchemaTransition> {
    let transition: SchemaTransition = decode("transition", id, raw)?;
    if transition.id.as_str() != id {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!(
                "catalog: transition key {id:?} contains ID {:?}",
                transition.id
            ),
        ));
    }
    validate_transition(&transition)
        .map_err(|error| Error::message(ErrorKind::CatalogCorrupt, error.to_string()))?;
    Ok(transition)
}

fn validate_transition(value: &SchemaTransition) -> Result<()> {
    if value.id.is_empty()
        || value.generation.is_zero()
        || value.created_at.is_zero()
        || value.updated_at.is_zero()
    {
        return Err(Error::message(
            ErrorKind::InvalidInput,
            format!("catalog: invalid transition checkpoint {:?}", value.id),
        ));
    }
    if !value.compacted_at.is_zero() {
        if !value.state.is_terminal() {
            return Err(Error::message(
                ErrorKind::InvalidInput,
                format!(
                    "catalog: non-terminal transition {:?} has a compaction timestamp",
                    value.id
                ),
            ));
        }
        return Ok(());
    }
    if value.table_id.is_empty() {
        return Err(Error::message(
            ErrorKind::InvalidInput,
            format!(
                "catalog: transition {:?} has an incomplete table target",
                value.id
            ),
        ));
    }
    if value.delta_hard_limit > 0 && value.delta_soft_limit > value.delta_hard_limit {
        return Err(Error::message(
            ErrorKind::InvalidInput,
            format!(
                "catalog: transition {:?} has soft delta limit {} above hard limit {}",
                value.id, value.delta_soft_limit, value.delta_hard_limit
            ),
        ));
    }
    Ok(())
}

fn delta_sequence_key(id: &TransitionId) -> Vec<u8> {
    format!("{DELTA_SEQUENCE_PREFIX}{id}").into_bytes()
}

fn delta_applied_key(id: &TransitionId) -> Vec<u8> {
    format!("{DELTA_APPLIED_PREFIX}{id}").into_bytes()
}

pub fn delta_key(id: &TransitionId, sequence: u64) -> Vec<u8> {
    format!("{DELTA_PREFIX}{id}/{sequence:020}").into_bytes()
}

pub fn delta_range(id: &TransitionId) -> (Vec<u8>, Vec<u8>) {
    let start = format!("{DELTA_PREFIX}{id}/").into_bytes();
    let end = prefix_end(&start).expect("catalog prefix has an upper bound");
    (start, end)
}

pub async fn delete_delta_metadata<V: KvView + ?Sized>(
    view: &mut V,
    id: &TransitionId,
) -> Result<()> {
    view.delete(&delta_sequence_key(id)).await.map_err(map_kv)?;
    view.delete(&delta_applied_key(id)).await.map_err(map_kv)
}

pub async fn append_index_delta<V: KvView + ?Sized>(
    view: &mut V,
    transition_id: &TransitionId,
    hard_limit: u64,
    mut delta: IndexDelta,
) -> Result<u64> {
    // This counter intentionally serializes writers captured by one index
    // build. Its order is the durable catch-up order: replacing it with an
    // identifier allocated before commit could let the worker advance past a
    // delta that becomes visible later. Keep the contention explicit until a
    // replacement protocol supplies commit-ordered identities or rescans a
    // safely fenced frontier.
    let key = delta_sequence_key(transition_id);
    let sequence = match view.get(&key).await.map_err(map_kv)? {
        Some(raw) => parse_u64_record("delta sequence", transition_id.as_str(), &raw)?,
        None => 0,
    };
    if sequence == u64::MAX {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!("catalog: delta sequence for transition {transition_id:?} is exhausted"),
        ));
    }
    let applied = delta_applied(view, transition_id).await?;
    let lag = sequence.saturating_sub(applied);
    if hard_limit > 0 && lag >= hard_limit {
        return Err(Error::message(
            ErrorKind::TransitionBackpressure,
            format!(
                "catalog: transition {transition_id:?} retained delta work reached hard limit {hard_limit}; retry after the schema worker catches up"
            ),
        ));
    }
    let sequence = sequence + 1;
    delta.id = format!("{transition_id}:{sequence:020}");
    delta.sequence = sequence;
    let raw = encode("index delta", &delta.id, &delta)?;
    view.put(
        Bytes::from(delta_key(transition_id, sequence)),
        Bytes::from(raw),
    )
    .await
    .map_err(map_kv)?;
    view.put(Bytes::from(key), Bytes::from(sequence.to_string()))
        .await
        .map_err(map_kv)?;
    Ok(sequence)
}

async fn delta_applied<V: KvView + ?Sized>(view: &mut V, id: &TransitionId) -> Result<u64> {
    let Some(raw) = view.get(&delta_applied_key(id)).await.map_err(map_kv)? else {
        return Ok(0);
    };
    parse_u64_record("applied delta", id.as_str(), &raw)
}

pub async fn save_delta_applied<V: KvView + ?Sized>(
    view: &mut V,
    id: &TransitionId,
    sequence: u64,
) -> Result<()> {
    view.put(
        Bytes::from(delta_applied_key(id)),
        Bytes::from(sequence.to_string()),
    )
    .await
    .map_err(map_kv)
}

pub async fn delta_high_water<V: KvView + ?Sized>(view: &mut V, id: &TransitionId) -> Result<u64> {
    let Some(raw) = view.get(&delta_sequence_key(id)).await.map_err(map_kv)? else {
        return Ok(0);
    };
    parse_u64_record("delta sequence", id.as_str(), &raw)
}

pub fn decode_index_delta(
    transition_id: &TransitionId,
    key: &[u8],
    raw: &[u8],
) -> Result<IndexDelta> {
    let prefix = format!("{DELTA_PREFIX}{transition_id}/");
    let Some(encoded) = key.strip_prefix(prefix.as_bytes()) else {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!("catalog: index delta key {key:?} is outside transition {transition_id:?}"),
        ));
    };
    let encoded = std::str::from_utf8(encoded).map_err(|error| {
        Error::source(
            ErrorKind::CatalogCorrupt,
            "catalog: invalid index delta key",
            error,
        )
    })?;
    let sequence = encoded.parse::<u64>().map_err(|error| {
        Error::source(
            ErrorKind::CatalogCorrupt,
            "catalog: invalid index delta key",
            error,
        )
    })?;
    if encoded != format!("{sequence:020}") {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            "catalog: non-canonical index delta key",
        ));
    }
    let delta: IndexDelta = decode("index delta", &String::from_utf8_lossy(key), raw)?;
    let expected_id = format!("{transition_id}:{sequence:020}");
    if delta.sequence != sequence || delta.id != expected_id {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!(
                "catalog: index delta key {key:?} contains identity {:?} at sequence {}",
                delta.id, delta.sequence
            ),
        ));
    }
    Ok(delta)
}

fn unique_claim_tuple_prefix(id: &TransitionId, tuple: &[u8]) -> Vec<u8> {
    let mut key = format!("{UNIQUE_CLAIM_PREFIX}{id}/").into_bytes();
    key.put_u64(tuple.len() as u64);
    key.extend_from_slice(tuple);
    key.push(b'/');
    key
}

fn unique_claim_key(id: &TransitionId, tuple: &[u8], pk: &[u8]) -> Vec<u8> {
    let mut key = unique_claim_tuple_prefix(id, tuple);
    key.extend_from_slice(pk);
    key
}

pub fn unique_claim_range(id: &TransitionId) -> (Vec<u8>, Vec<u8>) {
    let start = format!("{UNIQUE_CLAIM_PREFIX}{id}/").into_bytes();
    let end = prefix_end(&start).expect("catalog prefix has an upper bound");
    (start, end)
}

pub fn unique_violation_range(id: &TransitionId) -> (Vec<u8>, Vec<u8>) {
    let start = format!("{UNIQUE_VIOLATION_PREFIX}{id}/").into_bytes();
    let end = prefix_end(&start).expect("catalog prefix has an upper bound");
    (start, end)
}

fn unique_violation_key(id: &TransitionId, tuple: &[u8]) -> Vec<u8> {
    let mut key = format!("{UNIQUE_VIOLATION_PREFIX}{id}/").into_bytes();
    key.put_u64(tuple.len() as u64);
    key.extend_from_slice(tuple);
    key
}

pub async fn put_unique_claim<V: KvView + ?Sized>(
    view: &mut V,
    transition_id: &TransitionId,
    tuple: &[u8],
    pk: &[u8],
) -> Result<()> {
    let claim = UniqueIndexClaim {
        tuple: tuple.to_vec(),
        pk: pk.to_vec(),
    };
    let raw = encode("unique index claim", "", &claim)?;
    let prefix = unique_claim_tuple_prefix(transition_id, tuple);
    let mut duplicate = false;
    {
        let mut iterator = view
            .scan(KeyRange::new(
                Bytes::copy_from_slice(&prefix),
                Bytes::from(prefix_end(&prefix).expect("claim prefix has an upper bound")),
            ))
            .await
            .map_err(map_kv)?;
        while let Some(entry) = iterator.next().await.map_err(map_kv)? {
            let existing: UniqueIndexClaim = decode("unique index claim", "", &entry.value)?;
            if existing.tuple != tuple || existing.pk.as_slice() != &entry.key[prefix.len()..] {
                return Err(Error::message(
                    ErrorKind::CatalogCorrupt,
                    format!(
                        "catalog: unique claim key {:?} disagrees with its durable value",
                        entry.key
                    ),
                ));
            }
            if existing.pk != pk {
                duplicate = true;
                break;
            }
        }
    }
    let key = unique_claim_key(transition_id, tuple, pk);
    if let Some(existing) = view.get(&key).await.map_err(map_kv)?
        && existing.as_ref() != raw.as_slice()
    {
        return Err(Error::message(
            ErrorKind::Conflict,
            format!("catalog: unique claim {key:?} has conflicting contents"),
        ));
    }
    view.put(Bytes::from(key), Bytes::from(raw))
        .await
        .map_err(map_kv)?;
    if duplicate {
        view.put(
            Bytes::from(unique_violation_key(transition_id, tuple)),
            Bytes::copy_from_slice(tuple),
        )
        .await
        .map_err(map_kv)?;
    }
    Ok(())
}

pub async fn delete_unique_claim<V: KvView + ?Sized>(
    view: &mut V,
    transition_id: &TransitionId,
    tuple: &[u8],
    pk: &[u8],
) -> Result<()> {
    view.delete(&unique_claim_key(transition_id, tuple, pk))
        .await
        .map_err(map_kv)?;
    let prefix = unique_claim_tuple_prefix(transition_id, tuple);
    let remaining = {
        let mut iterator = view
            .scan(KeyRange::new(
                Bytes::copy_from_slice(&prefix),
                Bytes::from(prefix_end(&prefix).expect("claim prefix has an upper bound")),
            ))
            .await
            .map_err(map_kv)?;
        let mut remaining = 0;
        while let Some(entry) = iterator.next().await.map_err(map_kv)? {
            let claim: UniqueIndexClaim = decode("unique index claim", "", &entry.value)?;
            if claim.tuple != tuple || claim.pk.as_slice() != &entry.key[prefix.len()..] {
                return Err(Error::message(
                    ErrorKind::CatalogCorrupt,
                    format!(
                        "catalog: unique claim key {:?} disagrees with its durable value",
                        entry.key
                    ),
                ));
            }
            if claim.pk != pk {
                remaining += 1;
                if remaining > 1 {
                    break;
                }
            }
        }
        remaining
    };
    if remaining > 1 {
        view.put(
            Bytes::from(unique_violation_key(transition_id, tuple)),
            Bytes::copy_from_slice(tuple),
        )
        .await
        .map_err(map_kv)
    } else {
        view.delete(&unique_violation_key(transition_id, tuple))
            .await
            .map_err(map_kv)
    }
}

pub async fn first_unique_violation<V: KvView + ?Sized>(
    view: &mut V,
    transition_id: &TransitionId,
) -> Result<Option<Bytes>> {
    let (start, end) = unique_violation_range(transition_id);
    let mut iterator = view
        .scan(KeyRange::new(Bytes::from(start), Bytes::from(end)))
        .await
        .map_err(map_kv)?;
    Ok(iterator
        .next()
        .await
        .map_err(map_kv)?
        .map(|entry| entry.value))
}

fn parse_generation(kind: &str, table_id: &str, raw: &[u8]) -> Result<WriteProtocolGeneration> {
    parse_u64_record(kind, table_id, raw).map(Into::into)
}

fn parse_u64_record(kind: &str, id: &str, raw: &[u8]) -> Result<u64> {
    let value = std::str::from_utf8(raw).map_err(|error| {
        Error::source(
            ErrorKind::CatalogCorrupt,
            format!("catalog: corrupt {kind} for {id:?}"),
            error,
        )
    })?;
    value.parse::<u64>().map_err(|error| {
        Error::source(
            ErrorKind::CatalogCorrupt,
            format!("catalog: corrupt {kind} for {id:?}"),
            error,
        )
    })
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{
        AccessGeneration, CatalogVersion, DefinitionGeneration, ExistenceGeneration,
        LogicalIndexId, OwnerEpoch, SchemaId, TransitionGeneration, ValueGeneration,
    };
    use crate::engine::catalog::model::{
        Column, DataPosition, Index, IndexDeltaOperation, ScalarType, Timestamp, TransitionKind,
        TransitionState, TransitionWorkState,
    };
    use crate::engine::kv::slatedb;
    use crate::engine::kv::{IsolationLevel, Kv, TransactionView, TransactionalKv};

    use super::*;

    fn transition(id: &str) -> SchemaTransition {
        SchemaTransition {
            id: id.into(),
            kind: TransitionKind::IndexBuild,
            object_id: "li1".into(),
            state: TransitionState::Building,
            generation: TransitionGeneration::from(1),
            owner_epoch: OwnerEpoch::ZERO,
            source_catalog_version: CatalogVersion::from(1),
            base_position: DataPosition::new("1"),
            barrier_position: DataPosition::default(),
            table_id: "t1".into(),
            table_schema_id: SchemaId::new(1).unwrap(),
            affected_column_ids: Vec::new(),
            index: index("i1", "li1", "by_id"),
            index_request: None,
            column_replacement: None,
            replacement_request: None,
            constraint: None,
            constraint_request: None,
            prerequisites: Vec::new(),
            gate_table_ids: Vec::new(),
            cursor: Vec::new(),
            batch_id: 0,
            applied_delta: 0,
            delta_high_water: 0,
            delta_soft_limit: 0,
            delta_hard_limit: 0,
            work_state: TransitionWorkState::Normal,
            rows_scanned: 0,
            last_error: String::new(),
            created_at: Timestamp::test_value(),
            updated_at: Timestamp::test_value(),
            compacted_at: Timestamp::default(),
        }
    }

    fn index(id: &str, logical_id: &str, name: &str) -> Index {
        Index {
            id: id.into(),
            logical_id: logical_id.into(),
            definition_generation: DefinitionGeneration::from(1),
            access_generation: AccessGeneration::from(1),
            state: crate::engine::catalog::model::IndexState::Ready,
            name: name.into(),
            columns: vec!["id".into()],
            column_ids: vec!["c1".into()],
            unique: false,
        }
    }

    fn table() -> Table {
        Table {
            id: "t1".into(),
            schema_id: SchemaId::new(1).unwrap(),
            name: "users".into(),
            definition_generation: DefinitionGeneration::from(1),
            existence_generation: ExistenceGeneration::from(1),
            write_protocol_generation: WriteProtocolGeneration::from(1),
            columns: vec![Column {
                id: "c1".into(),
                schema_id: SchemaId::new(1).unwrap(),
                name: "id".into(),
                value_generation: ValueGeneration::from(1),
                scalar_type: ScalarType::Text,
                nullable: false,
                format: String::new(),
                insert_default: None,
                missing_value: None,
            }],
            primary_key: vec!["id".into()],
            indexes: vec![index("i1", "li1", "by_id")],
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    fn protocol(generation: u64) -> WriteProtocol {
        WriteProtocol {
            table_id: "t1".into(),
            generation: generation.into(),
            ready_indexes: vec![index("i2", "li2", "second"), index("i1", "li1", "first")],
            delta_sinks: Vec::new(),
            column_replacements: Vec::new(),
            constraint_checks: Vec::new(),
            finalization_gate: None,
        }
    }

    #[tokio::test]
    async fn transition_storage_is_strict_and_binds_key_to_payload_identity() {
        let mut database = slatedb::Store::memory("catalog-transition-strict")
            .await
            .unwrap();
        let value = transition("tr1");
        create_transition(&mut database, &value).await.unwrap();
        assert!(has_transition_history(&mut database).await.unwrap());
        assert_eq!(
            get_transition(&mut database, &value.id).await.unwrap(),
            Some(value.clone())
        );

        let raw = encode("transition", "tr1", &value).unwrap();
        Kv::put(
            &database,
            Bytes::from(transition_key(&TransitionId::from("alias"))),
            Bytes::from(raw),
        )
        .await
        .unwrap();
        assert_eq!(
            list_transitions(&mut database).await.unwrap_err().kind(),
            ErrorKind::CatalogCorrupt
        );

        let malformed = serde_json::json!({
            "id": "tr2",
            "kind": "index_build",
            "state": "building",
            "generation": 1,
            "owner_epoch": 0,
            "source_catalog_version": 1,
            "base_position": "1",
            "table_id": "t1",
            "table_schema_id": 1,
            "index": {"id":"i1","name":"by_id","columns":["id"],"unique":false},
            "created_at": Timestamp::test_value(),
            "updated_at": Timestamp::test_value(),
            "compacted_at": Timestamp::default(),
            "future": true
        });
        Kv::put(
            &database,
            Bytes::from(transition_key(&TransitionId::from("tr2"))),
            Bytes::from(serde_json::to_vec(&malformed).unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(
            get_transition(&mut database, &TransitionId::from("tr2"))
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::CatalogCorrupt
        );
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn unrelated_transition_checkpoints_do_not_conflict() {
        let mut database = slatedb::Store::memory("catalog-transition-independent-checkpoints")
            .await
            .unwrap();
        let mut first = transition("tr1");
        let mut second = transition("tr2");
        create_transition(&mut database, &first).await.unwrap();
        create_transition(&mut database, &second).await.unwrap();

        let first_transaction = database.begin(IsolationLevel::Snapshot).await.unwrap();
        let second_transaction = database.begin(IsolationLevel::Snapshot).await.unwrap();
        first.generation = first.generation.next();
        second.generation = second.generation.next();
        {
            let mut view = TransactionView(first_transaction.as_ref());
            save_transition(&mut view, &first).await.unwrap();
        }
        {
            let mut view = TransactionView(second_transaction.as_ref());
            save_transition(&mut view, &second).await.unwrap();
        }

        first_transaction.commit().await.unwrap();
        second_transaction.commit().await.unwrap();
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn transition_creation_does_not_conflict_with_an_unrelated_checkpoint() {
        let mut database = slatedb::Store::memory("catalog-transition-create-checkpoint-race")
            .await
            .unwrap();
        let mut checkpointed = transition("tr1");
        create_transition(&mut database, &checkpointed)
            .await
            .unwrap();

        let checkpoint_transaction = database.begin(IsolationLevel::Snapshot).await.unwrap();
        let creation_transaction = database.begin(IsolationLevel::Snapshot).await.unwrap();
        checkpointed.generation = checkpointed.generation.next();
        {
            let mut view = TransactionView(checkpoint_transaction.as_ref());
            save_transition(&mut view, &checkpointed).await.unwrap();
        }
        {
            let mut view = TransactionView(creation_transaction.as_ref());
            create_transition(&mut view, &transition("tr2"))
                .await
                .unwrap();
        }

        checkpoint_transaction.commit().await.unwrap();
        creation_transaction.commit().await.unwrap();
        assert!(has_transition_history(&mut database).await.unwrap());
        assert!(
            get_transition(&mut database, &TransitionId::from("tr2"))
                .await
                .unwrap()
                .is_some()
        );
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn compacted_terminal_transition_may_omit_live_target() {
        let mut database = slatedb::Store::memory("catalog-transition-compacted")
            .await
            .unwrap();
        let mut value = transition("tr1");
        value.state = TransitionState::Ready;
        value.table_id = TableId::default();
        value.compacted_at = Timestamp::test_value();
        save_transition(&mut database, &value).await.unwrap();
        assert_eq!(
            get_transition(&mut database, &value.id).await.unwrap(),
            Some(value)
        );
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn write_protocol_is_canonical_immutable_and_generation_fenced() {
        let mut database = slatedb::Store::memory("catalog-write-protocol")
            .await
            .unwrap();
        save_write_protocol(&mut database, protocol(1), Timestamp::test_value())
            .await
            .unwrap();
        let loaded = read_write_protocol(&mut database, &table()).await.unwrap();
        assert_eq!(
            loaded.ready_indexes[0].logical_id,
            LogicalIndexId::from("li1")
        );

        let mut conflicting = protocol(1);
        conflicting.ready_indexes.pop();
        assert_eq!(
            save_write_protocol(&mut database, conflicting, Timestamp::test_value())
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::Conflict
        );

        let mut stale = table();
        stale.write_protocol_generation = WriteProtocolGeneration::from(2);
        assert_eq!(
            read_write_protocol(&mut database, &stale)
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::Conflict
        );
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn deltas_have_canonical_ordered_identity_and_backpressure() {
        let mut database = slatedb::Store::memory("catalog-deltas").await.unwrap();
        let id = TransitionId::from("tr1");
        let sequence = append_index_delta(
            &mut database,
            &id,
            1,
            IndexDelta {
                id: String::new(),
                sequence: 0,
                operation: IndexDeltaOperation::Put,
                pk: b"pk".to_vec(),
                tuple: b"tuple".to_vec(),
            },
        )
        .await
        .unwrap();
        assert_eq!(sequence, 1);
        let raw = Kv::get(&database, &delta_key(&id, 1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            decode_index_delta(&id, &delta_key(&id, 1), &raw)
                .unwrap()
                .sequence,
            1
        );
        assert_eq!(
            append_index_delta(
                &mut database,
                &id,
                1,
                IndexDelta {
                    id: String::new(),
                    sequence: 0,
                    operation: IndexDeltaOperation::Delete,
                    pk: Vec::new(),
                    tuple: Vec::new(),
                },
            )
            .await
            .unwrap_err()
            .kind(),
            ErrorKind::TransitionBackpressure
        );
        save_delta_applied(&mut database, &id, 1).await.unwrap();
        assert_eq!(delta_high_water(&mut database, &id).await.unwrap(), 1);

        assert_eq!(
            decode_index_delta(&TransitionId::from("tr-other"), &delta_key(&id, 1), &raw)
                .unwrap_err()
                .kind(),
            ErrorKind::CatalogCorrupt
        );
        assert_eq!(
            decode_index_delta(&id, b"/rad/catalog/transition_delta/tr1/1", &raw)
                .unwrap_err()
                .kind(),
            ErrorKind::CatalogCorrupt
        );
        let mut tampered = decode_index_delta(&id, &delta_key(&id, 1), &raw).unwrap();
        tampered.sequence = 2;
        assert_eq!(
            decode_index_delta(
                &id,
                &delta_key(&id, 1),
                &serde_json::to_vec(&tampered).unwrap()
            )
            .unwrap_err()
            .kind(),
            ErrorKind::CatalogCorrupt
        );

        Kv::put(
            &database,
            Bytes::from(delta_sequence_key(&id)),
            Bytes::from(u64::MAX.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(
            append_index_delta(
                &mut database,
                &id,
                0,
                IndexDelta {
                    id: String::new(),
                    sequence: 0,
                    operation: IndexDeltaOperation::Put,
                    pk: Vec::new(),
                    tuple: Vec::new(),
                },
            )
            .await
            .unwrap_err()
            .kind(),
            ErrorKind::CatalogCorrupt
        );
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn active_index_delta_counter_serializes_overlapping_writers_by_design() {
        const WRITERS: usize = 8;

        let mut database = slatedb::Store::memory("catalog-delta-writer-contention")
            .await
            .unwrap();
        let id = TransitionId::from("tr1");
        let mut transactions = Vec::with_capacity(WRITERS);
        for writer in 0..WRITERS {
            let transaction = database.begin(IsolationLevel::Snapshot).await.unwrap();
            {
                let mut view = TransactionView(transaction.as_ref());
                let sequence = append_index_delta(
                    &mut view,
                    &id,
                    0,
                    IndexDelta {
                        id: String::new(),
                        sequence: 0,
                        operation: IndexDeltaOperation::Put,
                        pk: format!("pk-{writer}").into_bytes(),
                        tuple: format!("tuple-{writer}").into_bytes(),
                    },
                )
                .await
                .unwrap();
                assert_eq!(sequence, 1);
            }
            transactions.push(transaction);
        }

        let mut committed = 0;
        let mut conflicts = 0;
        for transaction in transactions {
            match transaction.commit().await {
                Ok(()) => committed += 1,
                Err(error) if error.kind() == crate::engine::kv::ErrorKind::Conflict => {
                    conflicts += 1;
                }
                Err(error) => panic!("unexpected delta writer failure: {error}"),
            }
        }
        assert_eq!(committed, 1);
        assert_eq!(conflicts, WRITERS - 1);
        assert_eq!(delta_high_water(&mut database, &id).await.unwrap(), 1);
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn unique_claims_publish_and_clear_a_durable_violation() {
        let mut database = slatedb::Store::memory("catalog-unique-claims")
            .await
            .unwrap();
        let id = TransitionId::from("tr1");
        put_unique_claim(&mut database, &id, b"same", b"pk1")
            .await
            .unwrap();
        assert!(
            first_unique_violation(&mut database, &id)
                .await
                .unwrap()
                .is_none()
        );
        put_unique_claim(&mut database, &id, b"same", b"pk2")
            .await
            .unwrap();
        assert_eq!(
            first_unique_violation(&mut database, &id)
                .await
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"same")
        );
        delete_unique_claim(&mut database, &id, b"same", b"pk2")
            .await
            .unwrap();
        assert!(
            first_unique_violation(&mut database, &id)
                .await
                .unwrap()
                .is_none()
        );

        let key = unique_claim_key(&id, b"same", b"pk1");
        let tampered = UniqueIndexClaim {
            tuple: b"same".to_vec(),
            pk: b"other-pk".to_vec(),
        };
        Kv::put(
            &database,
            Bytes::from(key),
            Bytes::from(serde_json::to_vec(&tampered).unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(
            put_unique_claim(&mut database, &id, b"same", b"pk2")
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::CatalogCorrupt
        );
        database.close().await.unwrap();
    }
}
