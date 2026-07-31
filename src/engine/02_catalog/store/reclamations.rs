use bytes::Bytes;

use crate::engine::catalog::identity::{
    ColumnId, DefinitionGeneration, IndexId, ReclamationId, SchemaId, TableId, TransitionId,
    WriteProtocolGeneration,
};
use crate::engine::catalog::model::{Reclamation, ReclamationKind, ReclamationState, Timestamp};
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::KvView;

use super::durable_json::{decode, encode};
use super::{map_kv, prefix_range};

const RECLAMATION_PREFIX: &str = "/rad/catalog/reclamation/";
const RECLAMATION_WAKE_KEY: &[u8] = b"/rad/catalog/meta/reclamation_seen";

fn reclamation_key(id: &ReclamationId) -> Vec<u8> {
    format!("{RECLAMATION_PREFIX}{id}").into_bytes()
}

pub async fn queue_reclamation<V: KvView + ?Sized>(
    view: &mut V,
    mut reclamation: Reclamation,
    now: Timestamp,
) -> Result<()> {
    if reclamation.generation.is_zero() {
        reclamation.generation = 1.into();
    }
    if reclamation.created_at.is_zero() {
        reclamation.created_at = now;
    }
    if reclamation.updated_at.is_zero() {
        reclamation.updated_at = reclamation.created_at;
    }
    validate_reclamation(&reclamation)?;
    let key = reclamation_key(&reclamation.id);
    if let Some(raw) = view.get(&key).await.map_err(map_kv)? {
        let current = decode_reclamation(reclamation.id.as_str(), &raw)?;
        if (!current.compacted_at.is_zero()
            && current.id == reclamation.id
            && current.kind == reclamation.kind)
            || same_reclamation_target(&current, &reclamation)
        {
            return Ok(());
        }
        return Err(Error::message(
            ErrorKind::Conflict,
            format!(
                "catalog: reclamation {:?} already exists with different contents",
                reclamation.id
            ),
        ));
    }
    let raw = encode("reclamation", reclamation.id.as_str(), &reclamation)?;
    view.put(Bytes::from(key), Bytes::from(raw))
        .await
        .map_err(map_kv)?;
    view.put(
        Bytes::from_static(RECLAMATION_WAKE_KEY),
        Bytes::from_static(&[1]),
    )
    .await
    .map_err(map_kv)
}

pub async fn has_reclamation_history<V: KvView + ?Sized>(view: &mut V) -> Result<bool> {
    Ok(view
        .get(RECLAMATION_WAKE_KEY)
        .await
        .map_err(map_kv)?
        .is_some())
}

pub async fn save_reclamation<V: KvView + ?Sized>(
    view: &mut V,
    reclamation: &Reclamation,
) -> Result<()> {
    validate_reclamation(reclamation)?;
    let raw = encode("reclamation", reclamation.id.as_str(), reclamation)?;
    view.put(
        Bytes::from(reclamation_key(&reclamation.id)),
        Bytes::from(raw),
    )
    .await
    .map_err(map_kv)
}

pub async fn get_reclamation<V: KvView + ?Sized>(
    view: &mut V,
    id: &ReclamationId,
) -> Result<Option<Reclamation>> {
    let Some(raw) = view.get(&reclamation_key(id)).await.map_err(map_kv)? else {
        return Ok(None);
    };
    decode_reclamation(id.as_str(), &raw).map(Some)
}

pub async fn list_reclamations<V: KvView + ?Sized>(view: &mut V) -> Result<Vec<Reclamation>> {
    let prefix = RECLAMATION_PREFIX.as_bytes();
    let mut iterator = view.scan(prefix_range(prefix)).await.map_err(map_kv)?;
    let mut values = Vec::new();
    while let Some(entry) = iterator.next().await.map_err(map_kv)? {
        let id = entry
            .key
            .strip_prefix(prefix)
            .and_then(|value| std::str::from_utf8(value).ok())
            .ok_or_else(|| {
                Error::message(
                    ErrorKind::CatalogCorrupt,
                    "catalog: malformed reclamation key",
                )
            })?;
        values.push(decode_reclamation(id, &entry.value)?);
    }
    Ok(values)
}

fn decode_reclamation(id: &str, raw: &[u8]) -> Result<Reclamation> {
    let reclamation: Reclamation = decode("reclamation", id, raw)?;
    if reclamation.id.as_str() != id {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!(
                "catalog: reclamation key {id:?} contains ID {:?}",
                reclamation.id
            ),
        ));
    }
    validate_reclamation(&reclamation)
        .map_err(|error| Error::message(ErrorKind::CatalogCorrupt, error.to_string()))?;
    Ok(reclamation)
}

fn validate_reclamation(value: &Reclamation) -> Result<()> {
    if value.id.is_empty()
        || value.generation.is_zero()
        || value.retired_catalog_version.is_zero()
        || value.created_at.is_zero()
        || value.updated_at.is_zero()
    {
        return Err(Error::message(
            ErrorKind::InvalidInput,
            format!("catalog: invalid reclamation checkpoint {:?}", value.id),
        ));
    }
    if !value.compacted_at.is_zero() && value.state != ReclamationState::Reclaimed {
        return Err(Error::message(
            ErrorKind::InvalidInput,
            format!(
                "catalog: non-reclaimed reclamation {:?} has a compaction timestamp",
                value.id
            ),
        ));
    }
    let has_schema_id = value.table_schema_id.is_some();
    let complete = match value.kind {
        ReclamationKind::Table => !value.table_id.is_empty() && has_schema_id,
        ReclamationKind::Column => {
            !value.table_id.is_empty() && has_schema_id && !value.column_id.is_empty()
        }
        ReclamationKind::Index => {
            !value.table_id.is_empty() && has_schema_id && !value.index_id.is_empty()
        }
        ReclamationKind::TableDefinition => has_schema_id && !value.definition_generation.is_zero(),
        ReclamationKind::WriteProtocolDefinition => {
            !value.table_id.is_empty() && !value.write_protocol_generation.is_zero()
        }
        ReclamationKind::TransitionDeltas
        | ReclamationKind::CancelledIndex
        | ReclamationKind::FailedIndex => {
            !value.table_id.is_empty()
                && has_schema_id
                && !value.index_id.is_empty()
                && !value.transition_id.is_empty()
        }
        ReclamationKind::ReplacedColumn
        | ReclamationKind::CancelledReplacement
        | ReclamationKind::FailedReplacement => {
            !value.table_id.is_empty()
                && has_schema_id
                && !value.column_id.is_empty()
                && !value.transition_id.is_empty()
        }
        ReclamationKind::ConstraintValidation => {
            !value.table_id.is_empty() && has_schema_id && !value.transition_id.is_empty()
        }
    };
    if !complete {
        return Err(Error::message(
            ErrorKind::InvalidInput,
            format!(
                "catalog: reclamation {:?} has an incomplete target",
                value.id
            ),
        ));
    }
    Ok(())
}

fn same_reclamation_target(left: &Reclamation, right: &Reclamation) -> bool {
    left.id == right.id
        && left.kind == right.kind
        && left.retired_catalog_version == right.retired_catalog_version
        && left.table_id == right.table_id
        && left.table_schema_id == right.table_schema_id
        && left.column_id == right.column_id
        && left.index_id == right.index_id
        && left.index_ids == right.index_ids
        && left.definition_generation == right.definition_generation
        && left.write_protocol_generation == right.write_protocol_generation
        && left.transition_id == right.transition_id
}

pub fn table_reclamation_id(table_id: &TableId) -> ReclamationId {
    format!("table-{table_id}").into()
}

pub fn column_reclamation_id(table_id: &TableId, column_id: &ColumnId) -> ReclamationId {
    format!("column-{table_id}-{column_id}").into()
}

pub fn index_reclamation_id(table_id: &TableId, index_id: &IndexId) -> ReclamationId {
    format!("index-{table_id}-{index_id}").into()
}

pub fn table_definition_reclamation_id(
    id: SchemaId,
    generation: DefinitionGeneration,
) -> ReclamationId {
    format!("table-definition-{id}-{generation}").into()
}

pub fn write_protocol_reclamation_id(
    table_id: &TableId,
    generation: WriteProtocolGeneration,
) -> ReclamationId {
    format!("write-protocol-{table_id}-{generation}").into()
}

pub fn transition_delta_reclamation_id(id: &TransitionId) -> ReclamationId {
    format!("transition-deltas-{id}").into()
}

pub fn cancelled_index_reclamation_id(id: &TransitionId) -> ReclamationId {
    format!("cancelled-index-{id}").into()
}

pub fn failed_index_reclamation_id(id: &TransitionId) -> ReclamationId {
    format!("failed-index-{id}").into()
}

pub fn replaced_column_reclamation_id(id: &TransitionId) -> ReclamationId {
    format!("replaced-column-{id}").into()
}

pub fn cancelled_replacement_reclamation_id(id: &TransitionId) -> ReclamationId {
    format!("cancelled-replacement-{id}").into()
}

pub fn failed_replacement_reclamation_id(id: &TransitionId) -> ReclamationId {
    format!("failed-replacement-{id}").into()
}

pub fn constraint_validation_reclamation_id(id: &TransitionId) -> ReclamationId {
    format!("constraint-validation-{id}").into()
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::CatalogVersion;
    use crate::engine::kv::TransactionalKv;
    use crate::engine::kv::slatedb;

    use super::*;

    fn fixture(id: &str) -> Reclamation {
        let mut value = Reclamation::pending(
            ReclamationId::from(id),
            ReclamationKind::TableDefinition,
            CatalogVersion::from(3),
            Timestamp::test_value(),
        );
        value.table_schema_id = Some(SchemaId::new(2).unwrap());
        value.definition_generation = DefinitionGeneration::from(4);
        value
    }

    #[tokio::test]
    async fn queue_is_idempotent_for_the_same_target() {
        let mut database = slatedb::Store::memory("reclamation-idempotence")
            .await
            .unwrap();
        queue_reclamation(&mut database, fixture("r1"), Timestamp::test_value())
            .await
            .unwrap();
        queue_reclamation(&mut database, fixture("r1"), Timestamp::test_value())
            .await
            .unwrap();
        assert!(has_reclamation_history(&mut database).await.unwrap());
        assert_eq!(list_reclamations(&mut database).await.unwrap().len(), 1);
        database.close().await.unwrap();
    }

    #[tokio::test]
    async fn durable_list_rejects_key_identity_drift() {
        let mut database = slatedb::Store::memory("reclamation-key-drift")
            .await
            .unwrap();
        let value = fixture("r1");
        let raw = encode("reclamation", "r1", &value).unwrap();
        database
            .put(
                Bytes::from(reclamation_key(&ReclamationId::from("alias"))),
                Bytes::from(raw),
            )
            .await
            .unwrap();
        assert_eq!(
            list_reclamations(&mut database).await.unwrap_err().kind(),
            ErrorKind::CatalogCorrupt
        );
        database.close().await.unwrap();
    }
}
