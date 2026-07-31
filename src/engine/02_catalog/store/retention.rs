use std::collections::HashMap;

use bytes::Bytes;

use crate::engine::catalog::identity::{RetentionPinId, TransitionId};
use crate::engine::catalog::model::{
    Reclamation, ReclamationKind, RetentionHorizon, RetentionHorizons, RetentionPin,
    RetentionResource, RetentionResourceKind, Timestamp,
};
use crate::engine::catalog::{Error, ErrorKind, Result};
use crate::engine::kv::KvView;

use super::durable_json::{decode, encode};
use super::{map_kv, prefix_range};

const RETENTION_PIN_PREFIX: &str = "/rad/catalog/retention_pin/";

fn retention_pin_key(id: &RetentionPinId) -> Vec<u8> {
    format!("{RETENTION_PIN_PREFIX}{id}").into_bytes()
}

pub async fn save_retention_pin<V: KvView + ?Sized>(
    view: &mut V,
    mut pin: RetentionPin,
    now: Timestamp,
) -> Result<()> {
    if pin.created_at.is_zero() {
        pin.created_at = now;
    }
    validate_retention_pin(&pin)?;
    let key = retention_pin_key(&pin.id);
    if let Some(raw) = view.get(&key).await.map_err(map_kv)? {
        let current = decode_retention_pin(pin.id.as_str(), &raw)?;
        if current.id == pin.id
            && current.owner_kind == pin.owner_kind
            && current.owner_id == pin.owner_id
            && current.resource == pin.resource
        {
            return Ok(());
        }
        return Err(Error::message(
            ErrorKind::Conflict,
            format!(
                "catalog: retention pin {:?} already protects a different resource",
                pin.id
            ),
        ));
    }
    let raw = encode("retention pin", pin.id.as_str(), &pin)?;
    view.put(Bytes::from(key), Bytes::from(raw))
        .await
        .map_err(map_kv)
}

pub async fn delete_retention_pin<V: KvView + ?Sized>(
    view: &mut V,
    id: &RetentionPinId,
) -> Result<()> {
    if id.is_empty() {
        return Err(Error::message(
            ErrorKind::InvalidInput,
            "catalog: retention pin ID must not be empty",
        ));
    }
    view.delete(&retention_pin_key(id)).await.map_err(map_kv)
}

pub async fn get_retention_pin<V: KvView + ?Sized>(
    view: &mut V,
    id: &RetentionPinId,
) -> Result<Option<RetentionPin>> {
    let Some(raw) = view.get(&retention_pin_key(id)).await.map_err(map_kv)? else {
        return Ok(None);
    };
    decode_retention_pin(id.as_str(), &raw).map(Some)
}

pub async fn list_retention_pins<V: KvView + ?Sized>(view: &mut V) -> Result<Vec<RetentionPin>> {
    let prefix = RETENTION_PIN_PREFIX.as_bytes();
    let mut iterator = view.scan(prefix_range(prefix)).await.map_err(map_kv)?;
    let mut pins = Vec::new();
    while let Some(entry) = iterator.next().await.map_err(map_kv)? {
        let id = entry
            .key
            .strip_prefix(prefix)
            .and_then(|value| std::str::from_utf8(value).ok())
            .ok_or_else(|| {
                Error::message(
                    ErrorKind::CatalogCorrupt,
                    "catalog: malformed retention key",
                )
            })?;
        pins.push(decode_retention_pin(id, &entry.value)?);
    }
    Ok(pins)
}

fn decode_retention_pin(id: &str, raw: &[u8]) -> Result<RetentionPin> {
    let pin: RetentionPin = decode("retention pin", id, raw)?;
    if pin.id.as_str() != id {
        return Err(Error::message(
            ErrorKind::CatalogCorrupt,
            format!("catalog: retention pin key {id:?} contains ID {:?}", pin.id),
        ));
    }
    validate_retention_pin(&pin)
        .map_err(|error| Error::message(ErrorKind::CatalogCorrupt, error.to_string()))?;
    Ok(pin)
}

pub async fn retention_horizons<V: KvView + ?Sized>(view: &mut V) -> Result<RetentionHorizons> {
    let mut grouped = HashMap::<RetentionResource, u64>::new();
    for pin in list_retention_pins(view).await? {
        *grouped.entry(pin.resource).or_default() += 1;
    }
    let mut horizons = RetentionHorizons::default();
    for (resource, pin_count) in grouped {
        let group = match resource.kind {
            RetentionResourceKind::TableDefinition
            | RetentionResourceKind::WriteProtocolDefinition => &mut horizons.catalog_definitions,
            RetentionResourceKind::DataSnapshot => &mut horizons.data_snapshots,
            RetentionResourceKind::TransitionDeltas => &mut horizons.transition_deltas,
            RetentionResourceKind::PhysicalTable
            | RetentionResourceKind::PhysicalColumn
            | RetentionResourceKind::PhysicalIndex => &mut horizons.physical_artifacts,
            RetentionResourceKind::TransitionDiagnostics => &mut horizons.transition_diagnostics,
        };
        group.push(RetentionHorizon {
            resource,
            pin_count,
        });
    }
    for group in [
        &mut horizons.catalog_definitions,
        &mut horizons.data_snapshots,
        &mut horizons.transition_deltas,
        &mut horizons.physical_artifacts,
        &mut horizons.transition_diagnostics,
    ] {
        group.sort_by(|left, right| left.resource.cmp(&right.resource));
    }
    Ok(horizons)
}

pub async fn retention_blocker<V: KvView + ?Sized>(
    view: &mut V,
    reclamation: &Reclamation,
) -> Result<Option<RetentionPin>> {
    Ok(list_retention_pins(view).await?.into_iter().find(|pin| {
        pin.resource.kind == RetentionResourceKind::DataSnapshot
            || pin_blocks_reclamation(&pin.resource, reclamation)
    }))
}

fn pin_blocks_reclamation(resource: &RetentionResource, reclamation: &Reclamation) -> bool {
    match reclamation.kind {
        ReclamationKind::Table => {
            resource.kind == RetentionResourceKind::PhysicalTable
                && resource.table_id == reclamation.table_id
        }
        ReclamationKind::Column
        | ReclamationKind::ReplacedColumn
        | ReclamationKind::CancelledReplacement
        | ReclamationKind::FailedReplacement => {
            resource.kind == RetentionResourceKind::PhysicalColumn
                && resource.table_id == reclamation.table_id
                && resource.column_id == reclamation.column_id
        }
        ReclamationKind::Index => {
            resource.kind == RetentionResourceKind::PhysicalIndex
                && resource.table_id == reclamation.table_id
                && resource.index_id == reclamation.index_id
        }
        ReclamationKind::TableDefinition => {
            resource.kind == RetentionResourceKind::TableDefinition
                && resource.table_schema_id == reclamation.table_schema_id
                && resource.definition_generation == reclamation.definition_generation
        }
        ReclamationKind::WriteProtocolDefinition => {
            resource.kind == RetentionResourceKind::WriteProtocolDefinition
                && resource.table_id == reclamation.table_id
                && resource.write_protocol_generation == reclamation.write_protocol_generation
        }
        ReclamationKind::TransitionDeltas => {
            resource.kind == RetentionResourceKind::TransitionDeltas
                && resource.transition_id == reclamation.transition_id
        }
        ReclamationKind::CancelledIndex | ReclamationKind::FailedIndex => {
            (resource.kind == RetentionResourceKind::TransitionDeltas
                && resource.transition_id == reclamation.transition_id)
                || (resource.kind == RetentionResourceKind::PhysicalIndex
                    && resource.table_id == reclamation.table_id
                    && resource.index_id == reclamation.index_id)
        }
        ReclamationKind::ConstraintValidation => false,
    }
}

fn validate_retention_pin(pin: &RetentionPin) -> Result<()> {
    if pin.id.is_empty() || pin.owner_id.is_empty() || pin.created_at.is_zero() {
        return Err(Error::message(
            ErrorKind::InvalidInput,
            format!("catalog: invalid retention pin {:?}", pin.id),
        ));
    }
    let resource = &pin.resource;
    let complete = match resource.kind {
        RetentionResourceKind::TableDefinition => {
            resource.table_schema_id.is_some() && !resource.definition_generation.is_zero()
        }
        RetentionResourceKind::WriteProtocolDefinition => {
            !resource.table_id.is_empty() && !resource.write_protocol_generation.is_zero()
        }
        RetentionResourceKind::DataSnapshot => !resource.data_position.is_empty(),
        RetentionResourceKind::TransitionDeltas | RetentionResourceKind::TransitionDiagnostics => {
            !resource.transition_id.is_empty()
        }
        RetentionResourceKind::PhysicalTable => !resource.table_id.is_empty(),
        RetentionResourceKind::PhysicalColumn => {
            !resource.table_id.is_empty() && !resource.column_id.is_empty()
        }
        RetentionResourceKind::PhysicalIndex => {
            !resource.table_id.is_empty() && !resource.index_id.is_empty()
        }
    };
    if !complete {
        return Err(Error::message(
            ErrorKind::InvalidInput,
            format!(
                "catalog: retention pin {:?} has an incomplete resource",
                pin.id
            ),
        ));
    }
    Ok(())
}

pub async fn transition_diagnostics_pinned<V: KvView + ?Sized>(
    view: &mut V,
    transition_id: &TransitionId,
) -> Result<bool> {
    Ok(list_retention_pins(view).await?.iter().any(|pin| {
        pin.resource.kind == RetentionResourceKind::TransitionDiagnostics
            && &pin.resource.transition_id == transition_id
    }))
}

#[cfg(test)]
mod tests {
    use crate::engine::catalog::identity::{CatalogVersion, DefinitionGeneration, SchemaId};
    use crate::engine::catalog::model::{
        ReclamationKind, RetentionOwnerKind, RetentionResourceKind,
    };
    use crate::engine::kv::slatedb;
    use crate::engine::kv::{Kv, TransactionalKv};

    use super::*;

    fn resource() -> RetentionResource {
        RetentionResource {
            kind: RetentionResourceKind::TableDefinition,
            table_id: Default::default(),
            table_schema_id: Some(SchemaId::new(1).unwrap()),
            column_id: Default::default(),
            index_id: Default::default(),
            definition_generation: DefinitionGeneration::from(2),
            write_protocol_generation: Default::default(),
            transition_id: Default::default(),
            data_position: Default::default(),
        }
    }

    #[tokio::test]
    async fn pins_are_idempotent_grouped_and_block_exact_reclamation() {
        let mut database = slatedb::Store::memory("catalog-retention").await.unwrap();
        for id in ["p1", "p2"] {
            save_retention_pin(
                &mut database,
                RetentionPin {
                    id: id.into(),
                    owner_kind: RetentionOwnerKind::PreparedPlan,
                    owner_id: id.into(),
                    resource: resource(),
                    created_at: Timestamp::default(),
                },
                Timestamp::test_value(),
            )
            .await
            .unwrap();
        }
        assert_eq!(
            retention_horizons(&mut database)
                .await
                .unwrap()
                .catalog_definitions[0]
                .pin_count,
            2
        );
        let mut reclamation = Reclamation::pending(
            "r1",
            ReclamationKind::TableDefinition,
            CatalogVersion::from(3),
            Timestamp::test_value(),
        );
        reclamation.table_schema_id = Some(SchemaId::new(1).unwrap());
        reclamation.definition_generation = DefinitionGeneration::from(2);
        assert!(
            retention_blocker(&mut database, &reclamation)
                .await
                .unwrap()
                .is_some()
        );

        let original = RetentionPinId::from("p1");
        let raw = Kv::get(&database, &retention_pin_key(&original))
            .await
            .unwrap()
            .unwrap();
        Kv::put(
            &database,
            Bytes::from(retention_pin_key(&RetentionPinId::from("alias"))),
            raw,
        )
        .await
        .unwrap();
        assert_eq!(
            list_retention_pins(&mut database).await.unwrap_err().kind(),
            ErrorKind::CatalogCorrupt
        );
        database.close().await.unwrap();
    }
}
