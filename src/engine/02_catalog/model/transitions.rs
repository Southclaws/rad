use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{
    Column, DefaultValue, Index, ScalarType, Timestamp, deserialize_null_default, is_empty, is_zero,
};
use crate::engine::catalog::identity::{
    CatalogVersion, ColumnId, ConstraintId, DefinitionGeneration, LogicalIndexId, OwnerEpoch,
    SchemaId, TableId, TransitionGeneration, TransitionId, WriteProtocolGeneration,
};

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
        pub enum $name {
            $(#[serde(rename = $wire)] $variant),+
        }
    };
}

/// Opaque KV position. It is deliberately not comparable with catalog
/// versions or generations.
#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct DataPosition(String);

impl DataPosition {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

string_enum!(IndexState {
    LegacyReady => "",
    Building => "building",
    CatchingUp => "catching_up",
    Validating => "validating",
    Ready => "ready",
    Deleting => "deleting",
    Failed => "failed",
    Cancelled => "cancelled",
});

#[allow(clippy::derivable_impls)]
impl Default for IndexState {
    fn default() -> Self {
        Self::LegacyReady
    }
}

impl IndexState {
    pub fn is_legacy_ready(&self) -> bool {
        *self == Self::LegacyReady
    }
}

string_enum!(TransitionState {
    Waiting => "waiting",
    Building => "building",
    CatchingUp => "catching_up",
    Validating => "validating",
    Ready => "ready",
    Failed => "failed",
    Cancelled => "cancelled",
});

impl TransitionState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Ready | Self::Failed | Self::Cancelled)
    }
}

string_enum!(TransitionKind {
    IndexBuild => "index_build",
    ColumnReplacement => "column_replacement",
    ConstraintValidation => "constraint_validation",
});

string_enum!(TransitionWorkState {
    Unspecified => "",
    Normal => "normal",
    Degraded => "degraded",
    WriteGated => "write_gated",
});

#[allow(clippy::derivable_impls)]
impl Default for TransitionWorkState {
    fn default() -> Self {
        Self::Unspecified
    }
}

impl TransitionWorkState {
    pub fn is_unspecified(&self) -> bool {
        *self == Self::Unspecified
    }
}

pub const DEFAULT_DELTA_SOFT_LIMIT: u64 = 10_000;
pub const DEFAULT_DELTA_HARD_LIMIT: u64 = 100_000;

string_enum!(ColumnConversion {
    StrictBuiltin => "strict_builtin",
});

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnReplacement {
    pub source: Column,
    pub target: Column,
    pub conversion: ColumnConversion,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnReplacementDef {
    pub scalar_type: ScalarType,
    pub nullable: bool,
    pub format: String,
    pub default: Option<DefaultValue>,
    pub conversion: ColumnConversion,
    pub prerequisites: Vec<TransitionId>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnReplacementRequest {
    pub column_schema_id: SchemaId,
    #[serde(rename = "type")]
    pub scalar_type: ScalarType,
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<DefaultValue>,
    pub conversion: ColumnConversion,
}

string_enum!(ConstraintKind {
    NotNull => "not_null",
});

string_enum!(ConstraintState {
    Declared => "declared",
    EnforcingNewWrites => "enforcing_new_writes",
    ValidatingExisting => "validating_existing_data",
    Valid => "valid",
    Failed => "failed",
    Cancelled => "cancelled",
});

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Constraint {
    pub id: ConstraintId,
    pub definition_generation: DefinitionGeneration,
    pub name: String,
    pub kind: ConstraintKind,
    pub state: ConstraintState,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub column_ids: Vec<ColumnId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConstraintDef {
    pub name: String,
    pub kind: ConstraintKind,
    pub column_id: SchemaId,
    pub prerequisites: Vec<TransitionId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConstraintValidationRequest {
    pub constraint_id: ConstraintId,
    pub column_schema_id: SchemaId,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WriteProtocol {
    pub table_id: TableId,
    pub generation: WriteProtocolGeneration,
    #[serde(
        default,
        deserialize_with = "deserialize_null_default",
        skip_serializing_if = "is_empty"
    )]
    pub ready_indexes: Vec<Index>,
    #[serde(
        default,
        deserialize_with = "deserialize_null_default",
        skip_serializing_if = "is_empty"
    )]
    pub delta_sinks: Vec<IndexDeltaSink>,
    #[serde(
        default,
        deserialize_with = "deserialize_null_default",
        skip_serializing_if = "is_empty"
    )]
    pub column_replacements: Vec<ColumnReplacementWrite>,
    #[serde(
        default,
        deserialize_with = "deserialize_null_default",
        skip_serializing_if = "is_empty"
    )]
    pub constraint_checks: Vec<ConstraintCheck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalization_gate: Option<SchemaFinalizationGate>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexDeltaSink {
    pub transition_id: TransitionId,
    pub index: Index,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub columns: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub delta_hard_limit: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexBuildRequest {
    pub physical_id: crate::engine::catalog::identity::IndexId,
    pub logical_id: LogicalIndexId,
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub column_schema_ids: Vec<SchemaId>,
    pub unique: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnReplacementWrite {
    pub transition_id: TransitionId,
    pub replacement: ColumnReplacement,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConstraintCheck {
    pub transition_id: TransitionId,
    pub constraint: Constraint,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaFinalizationGate {
    pub transition_id: TransitionId,
    pub object_id: String,
    pub kind: TransitionKind,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaTransition {
    pub id: TransitionId,
    pub kind: TransitionKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub object_id: String,
    pub state: TransitionState,
    pub generation: TransitionGeneration,
    pub owner_epoch: OwnerEpoch,
    pub source_catalog_version: CatalogVersion,
    pub base_position: DataPosition,
    #[serde(default, skip_serializing_if = "DataPosition::is_empty")]
    pub barrier_position: DataPosition,
    pub table_id: TableId,
    pub table_schema_id: SchemaId,
    #[serde(
        default,
        rename = "affected_column_schema_ids",
        deserialize_with = "deserialize_null_default",
        skip_serializing_if = "is_empty"
    )]
    pub affected_column_ids: Vec<SchemaId>,
    pub index: Index,
    #[serde(
        default,
        rename = "index_build_request",
        skip_serializing_if = "Option::is_none"
    )]
    pub index_request: Option<IndexBuildRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_replacement: Option<ColumnReplacement>,
    #[serde(
        default,
        rename = "column_replacement_request",
        skip_serializing_if = "Option::is_none"
    )]
    pub replacement_request: Option<ColumnReplacementRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraint: Option<Constraint>,
    #[serde(
        default,
        rename = "constraint_validation_request",
        skip_serializing_if = "Option::is_none"
    )]
    pub constraint_request: Option<ConstraintValidationRequest>,
    #[serde(
        default,
        deserialize_with = "deserialize_null_default",
        skip_serializing_if = "is_empty"
    )]
    pub prerequisites: Vec<TransitionId>,
    #[serde(
        default,
        deserialize_with = "deserialize_null_default",
        skip_serializing_if = "is_empty"
    )]
    pub gate_table_ids: Vec<TableId>,
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
    pub applied_delta: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub delta_high_water: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub delta_soft_limit: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub delta_hard_limit: u64,
    #[serde(default, skip_serializing_if = "TransitionWorkState::is_unspecified")]
    pub work_state: TransitionWorkState,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rows_scanned: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_error: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub compacted_at: Timestamp,
}

impl SchemaTransition {
    pub fn control(&self) -> TransitionControl {
        let object_id = if self.object_id.is_empty() {
            self.index.logical_id.as_str().to_owned()
        } else {
            self.object_id.clone()
        };
        TransitionControl {
            kind: "transition".into(),
            transition_id: self.id.clone(),
            object_id,
            transition_kind: self.kind,
            state: self.state,
            generation: self.generation,
            prerequisites: self.prerequisites.clone(),
            retained_work_state: self.work_state,
            last_error: self.last_error.clone(),
            rows_scanned: self.rows_scanned,
            applied_delta: self.applied_delta,
            delta_lag: self.delta_high_water.saturating_sub(self.applied_delta),
        }
    }

    pub fn refresh_work_state(&mut self, high_water: u64) {
        self.delta_high_water = high_water;
        if self.state.is_terminal() {
            self.work_state = TransitionWorkState::Normal;
            return;
        }
        let lag = high_water.saturating_sub(self.applied_delta);
        self.work_state = if self.delta_hard_limit > 0 && lag >= self.delta_hard_limit {
            TransitionWorkState::WriteGated
        } else if self.delta_soft_limit > 0 && lag >= self.delta_soft_limit {
            TransitionWorkState::Degraded
        } else {
            TransitionWorkState::Normal
        };
    }

    /// Record one bounded physical scan and report whether it reached the end.
    pub(crate) fn advance_scan(
        &mut self,
        items: usize,
        last_key: Option<&[u8]>,
        now: Timestamp,
    ) -> bool {
        self.batch_id = self.batch_id.saturating_add(1);
        self.generation = self.generation.next();
        self.rows_scanned = self.rows_scanned.saturating_add(items as u64);
        self.updated_at = now;
        if let Some(last_key) = last_key {
            self.cursor.clear();
            self.cursor.extend_from_slice(last_key);
            false
        } else {
            true
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionControl {
    pub kind: String,
    pub transition_id: TransitionId,
    pub object_id: String,
    pub transition_kind: TransitionKind,
    pub state: TransitionState,
    pub generation: TransitionGeneration,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub prerequisites: Vec<TransitionId>,
    #[serde(default, skip_serializing_if = "TransitionWorkState::is_unspecified")]
    pub retained_work_state: TransitionWorkState,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_error: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rows_scanned: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub applied_delta: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub delta_lag: u64,
}

string_enum!(IndexDeltaOperation {
    Put => "put",
    Delete => "delete",
});

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexDelta {
    pub id: String,
    pub sequence: u64,
    pub operation: IndexDeltaOperation,
    #[serde(
        serialize_with = "serialize_bytes",
        deserialize_with = "deserialize_bytes"
    )]
    pub pk: Vec<u8>,
    #[serde(
        serialize_with = "serialize_bytes",
        deserialize_with = "deserialize_bytes"
    )]
    pub tuple: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UniqueIndexClaim {
    #[serde(
        serialize_with = "serialize_bytes",
        deserialize_with = "deserialize_bytes"
    )]
    pub tuple: Vec<u8>,
    #[serde(
        serialize_with = "serialize_bytes",
        deserialize_with = "deserialize_bytes"
    )]
    pub pk: Vec<u8>,
}

pub(crate) fn serialize_bytes<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
}

pub(crate) fn serialize_optional_bytes<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serialize_bytes(bytes, serializer)
}

pub(crate) fn deserialize_bytes<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let encoded = Option::<String>::deserialize(deserializer)?.unwrap_or_default();
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::catalog::identity::{AccessGeneration, IndexId};

    fn transition() -> SchemaTransition {
        SchemaTransition {
            id: "tr42".into(),
            kind: TransitionKind::IndexBuild,
            object_id: String::new(),
            state: TransitionState::CatchingUp,
            generation: 7.into(),
            owner_epoch: 11.into(),
            source_catalog_version: 1.into(),
            base_position: DataPosition::new("base-internal"),
            barrier_position: DataPosition::new("barrier-internal"),
            table_id: "t1".into(),
            table_schema_id: SchemaId::new(1).unwrap(),
            affected_column_ids: Vec::new(),
            index: Index {
                id: IndexId::from("i1"),
                logical_id: "ix43".into(),
                access_generation: AccessGeneration::from(1),
                ..Index::default()
            },
            index_request: None,
            column_replacement: None,
            replacement_request: None,
            constraint: None,
            constraint_request: None,
            prerequisites: Vec::new(),
            gate_table_ids: Vec::new(),
            cursor: Vec::new(),
            batch_id: 0,
            applied_delta: 3,
            delta_high_water: 5,
            delta_soft_limit: 4,
            delta_hard_limit: 8,
            work_state: TransitionWorkState::Degraded,
            rows_scanned: 12,
            last_error: String::new(),
            created_at: Timestamp::default(),
            updated_at: Timestamp::default(),
            compacted_at: Timestamp::default(),
        }
    }

    #[test]
    fn control_keeps_worker_internals_off_the_wire() {
        let raw = serde_json::to_value(transition().control()).unwrap();
        let object = raw.as_object().unwrap();
        for internal in [
            "owner_epoch",
            "base_position",
            "barrier_position",
            "work_state",
        ] {
            assert!(!object.contains_key(internal));
        }
        assert_eq!(object["retained_work_state"], "degraded");
        assert_eq!(object["delta_lag"], 2);
    }

    #[test]
    fn retained_work_thresholds_and_terminal_reset_match_the_contract() {
        let mut value = transition();
        value.applied_delta = 3;
        value.refresh_work_state(7);
        assert_eq!(value.work_state, TransitionWorkState::Degraded);
        value.refresh_work_state(11);
        assert_eq!(value.work_state, TransitionWorkState::WriteGated);
        value.state = TransitionState::Ready;
        value.refresh_work_state(100);
        assert_eq!(value.work_state, TransitionWorkState::Normal);
    }

    #[test]
    fn scan_progress_advances_once_and_reports_exhaustion() {
        let mut value = transition();
        let now = Timestamp::test_value();
        assert!(!value.advance_scan(3, Some(b"last-key"), now));
        assert_eq!(value.batch_id, 1);
        assert_eq!(value.generation.get(), 8);
        assert_eq!(value.rows_scanned, 15);
        assert_eq!(value.cursor, b"last-key");
        assert_eq!(value.updated_at, now);
        assert!(value.advance_scan(0, None, now));
        assert_eq!(value.batch_id, 2);
        assert_eq!(value.generation.get(), 9);
    }

    #[test]
    fn byte_fields_use_the_canonical_json_base64_encoding() {
        let delta = IndexDelta {
            id: "d1".into(),
            sequence: 1,
            operation: IndexDeltaOperation::Put,
            pk: vec![0, 1, 255],
            tuple: b"rad".to_vec(),
        };
        let raw = serde_json::to_value(&delta).unwrap();
        assert_eq!(raw["pk"], "AAH/");
        assert_eq!(serde_json::from_value::<IndexDelta>(raw).unwrap(), delta);
    }

    #[test]
    fn write_protocol_matches_the_shared_fixture() {
        let expected = strip_fixture_line_ending(include_bytes!(
            "../../../../tests/fixtures/catalog/write-protocol.json"
        ));
        let value: WriteProtocol = serde_json::from_slice(expected).unwrap();
        assert_eq!(value.table_id.as_str(), "t1");
        assert_eq!(value.ready_indexes[0].logical_id.as_str(), "ix1");
        assert_eq!(value.delta_sinks[0].delta_hard_limit, 100_000);
        assert_eq!(serde_json::to_vec(&value).unwrap(), expected);
    }

    #[test]
    fn shared_fixture_line_endings_are_platform_independent() {
        assert_eq!(strip_fixture_line_ending(b"{}\n"), b"{}");
        assert_eq!(strip_fixture_line_ending(b"{}\r\n"), b"{}");
        assert_eq!(strip_fixture_line_ending(b"{}"), b"{}");
        assert_eq!(strip_fixture_line_ending(b"{}\n\n"), b"{}\n");
    }

    fn strip_fixture_line_ending(bytes: &[u8]) -> &[u8] {
        bytes
            .strip_suffix(b"\r\n")
            .or_else(|| bytes.strip_suffix(b"\n"))
            .unwrap_or(bytes)
    }
}
