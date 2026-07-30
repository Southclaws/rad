use serde::{Deserialize, Serialize};

use super::{DataPosition, Timestamp, deserialize_null_default};
use crate::engine::catalog::identity::{
    ColumnId, DefinitionGeneration, IndexId, RetentionPinId, SchemaId, TableId, TransitionId,
    WriteProtocolGeneration,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionOwnerKind {
    PreparedPlan,
    DataSnapshot,
    Replica,
    Cdc,
    SchemaTransition,
    SchemaWorker,
    PhysicalReader,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionResourceKind {
    TableDefinition,
    WriteProtocolDefinition,
    DataSnapshot,
    TransitionDeltas,
    PhysicalTable,
    PhysicalColumn,
    PhysicalIndex,
    TransitionDiagnostics,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionResource {
    pub kind: RetentionResourceKind,
    #[serde(default, skip_serializing_if = "TableId::is_empty")]
    pub table_id: TableId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_schema_id: Option<SchemaId>,
    #[serde(default, skip_serializing_if = "ColumnId::is_empty")]
    pub column_id: ColumnId,
    #[serde(default, skip_serializing_if = "IndexId::is_empty")]
    pub index_id: IndexId,
    #[serde(default, skip_serializing_if = "DefinitionGeneration::is_zero")]
    pub definition_generation: DefinitionGeneration,
    #[serde(default, skip_serializing_if = "WriteProtocolGeneration::is_zero")]
    pub write_protocol_generation: WriteProtocolGeneration,
    #[serde(default, skip_serializing_if = "TransitionId::is_empty")]
    pub transition_id: TransitionId,
    #[serde(default, skip_serializing_if = "DataPosition::is_empty")]
    pub data_position: DataPosition,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionPin {
    pub id: RetentionPinId,
    pub owner_kind: RetentionOwnerKind,
    pub owner_id: String,
    pub resource: RetentionResource,
    pub created_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionHorizon {
    pub resource: RetentionResource,
    pub pin_count: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RetentionHorizons {
    pub catalog_definitions: Vec<RetentionHorizon>,
    pub data_snapshots: Vec<RetentionHorizon>,
    pub transition_deltas: Vec<RetentionHorizon>,
    pub physical_artifacts: Vec<RetentionHorizon>,
    pub transition_diagnostics: Vec<RetentionHorizon>,
}

// Durable records accept JSON null as an empty retained-resource list.
#[allow(dead_code)]
fn _decode_compatibility<'de, D>(deserializer: D) -> Result<Vec<RetentionResource>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_null_default(deserializer)
}
