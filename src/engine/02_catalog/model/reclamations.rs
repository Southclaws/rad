use serde::{Deserialize, Serialize};

use super::{
    Timestamp, deserialize_bytes, deserialize_null_default, is_empty, is_zero,
    serialize_optional_bytes,
};
use crate::engine::catalog::identity::{
    ColumnId, DefinitionGeneration, IndexId, OwnerEpoch, ReclamationId, SchemaId, TableId,
    TransitionGeneration, TransitionId, WriteProtocolGeneration,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReclamationKind {
    Table,
    Column,
    Index,
    TableDefinition,
    WriteProtocolDefinition,
    TransitionDeltas,
    CancelledIndex,
    FailedIndex,
    ReplacedColumn,
    CancelledReplacement,
    FailedReplacement,
    ConstraintValidation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReclamationState {
    Pending,
    Reclaiming,
    Reclaimed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reclamation {
    pub id: ReclamationId,
    pub kind: ReclamationKind,
    pub state: ReclamationState,
    pub generation: TransitionGeneration,
    pub owner_epoch: OwnerEpoch,
    #[serde(
        default,
        skip_serializing_if = "crate::engine::catalog::identity::CatalogVersion::is_zero"
    )]
    pub retired_catalog_version: crate::engine::catalog::identity::CatalogVersion,
    #[serde(default, skip_serializing_if = "TableId::is_empty")]
    pub table_id: TableId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_schema_id: Option<SchemaId>,
    #[serde(default, skip_serializing_if = "ColumnId::is_empty")]
    pub column_id: ColumnId,
    #[serde(default, skip_serializing_if = "IndexId::is_empty")]
    pub index_id: IndexId,
    #[serde(
        default,
        deserialize_with = "deserialize_null_default",
        skip_serializing_if = "is_empty"
    )]
    pub index_ids: Vec<IndexId>,
    #[serde(default, skip_serializing_if = "DefinitionGeneration::is_zero")]
    pub definition_generation: DefinitionGeneration,
    #[serde(default, skip_serializing_if = "WriteProtocolGeneration::is_zero")]
    pub write_protocol_generation: WriteProtocolGeneration,
    #[serde(default, skip_serializing_if = "TransitionId::is_empty")]
    pub transition_id: TransitionId,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub phase: String,
    #[serde(
        default,
        serialize_with = "serialize_optional_bytes",
        deserialize_with = "deserialize_bytes",
        skip_serializing_if = "is_empty"
    )]
    pub cursor: Vec<u8>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub batch_id: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub items_reclaimed: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_error: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub compacted_at: Timestamp,
}

impl Reclamation {
    pub fn pending(
        id: impl Into<ReclamationId>,
        kind: ReclamationKind,
        retired_catalog_version: crate::engine::catalog::identity::CatalogVersion,
        now: Timestamp,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            state: ReclamationState::Pending,
            generation: 1.into(),
            owner_epoch: OwnerEpoch::ZERO,
            retired_catalog_version,
            table_id: TableId::default(),
            table_schema_id: None,
            column_id: ColumnId::default(),
            index_id: IndexId::default(),
            index_ids: Vec::new(),
            definition_generation: DefinitionGeneration::ZERO,
            write_protocol_generation: WriteProtocolGeneration::ZERO,
            transition_id: TransitionId::default(),
            phase: String::new(),
            cursor: Vec::new(),
            batch_id: 0,
            items_reclaimed: 0,
            last_error: String::new(),
            created_at: now,
            updated_at: now,
            compacted_at: Timestamp::default(),
        }
    }
}
