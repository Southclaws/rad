//! Semantic engine boundaries for deterministic scheduling and replay.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::engine::catalog::identity::{OwnerEpoch, ReclamationId, TableId, TransitionId};
use crate::engine::catalog::model::{
    ReclamationKind, ReclamationState, TransitionKind, TransitionState,
};
use crate::engine::lir::fingerprint::Fingerprint;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationCacheMaterialization {
    QueryResult,
    Rows,
    HashJoinBuild,
    GroupedHashJoinDimension,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationCacheAdmissionResult {
    Admitted,
    TooLarge,
    PolicyRejected,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationCacheEvictionCause {
    Capacity,
    Replaced,
    Removed,
    Cleared,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationCacheLookupResult {
    Filled,
    Reused,
    Failed,
}

/// The durable operation whose transaction is being observed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineOperation {
    CatalogProgram {
        statements: Vec<String>,
    },
    CancelTransition {
        transition_id: TransitionId,
    },
    ClaimTransition {
        transition_id: TransitionId,
    },
    ActivateTransition {
        transition_id: TransitionId,
    },
    RecordTransitionError {
        transition_id: TransitionId,
    },
    StepTransition {
        transition_id: TransitionId,
        owner: OwnerEpoch,
        transition_kind: TransitionKind,
        started_state: TransitionState,
    },
    ClaimReclamation {
        reclamation_id: ReclamationId,
    },
    StepReclamation {
        reclamation_id: ReclamationId,
        owner: OwnerEpoch,
    },
    FailReclamation {
        reclamation_id: ReclamationId,
    },
    CompactTransition {
        transition_id: TransitionId,
    },
    CompactCatalogHistory,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateAction {
    Acquired,
    Released,
}

/// A boundary at which a deterministic driver may suspend or crash a host.
///
/// `Staged` means the change is visible only in the current transaction. It
/// becomes durable only after the matching `CommitSucceeded` event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineEvent {
    ConditionalQueryPrepared,
    ConditionalQueryCompared {
        unchanged: bool,
    },
    ConditionalQueryExecutionStarted,
    RelationCacheCandidatesSelected {
        selected: u32,
        overlap_rejected: u32,
        budget_rejected: u32,
    },
    SubrelationCacheLookupStarted {
        materialization: RelationCacheMaterialization,
        relation: Fingerprint,
    },
    SubrelationCacheFillStarted {
        materialization: RelationCacheMaterialization,
        relation: Fingerprint,
    },
    SubrelationCacheFillReady {
        materialization: RelationCacheMaterialization,
        relation: Fingerprint,
    },
    SubrelationCacheLookupCompleted {
        materialization: RelationCacheMaterialization,
        relation: Fingerprint,
        result: RelationCacheLookupResult,
    },
    RelationCacheEntryEvicted {
        materialization: RelationCacheMaterialization,
        relation: Fingerprint,
        cause: RelationCacheEvictionCause,
    },
    RelationCacheAdmissionCompleted {
        materialization: RelationCacheMaterialization,
        relation: Fingerprint,
        result: RelationCacheAdmissionResult,
    },
    PhysicalBatchStaged {
        operation: EngineOperation,
        items: usize,
    },
    CheckpointStaged {
        operation: EngineOperation,
        generation: u64,
        batch_id: u64,
        state: TransitionState,
    },
    FinalizationGateStaged {
        operation: EngineOperation,
        action: GateAction,
    },
    WriteProtocolStaged {
        operation: EngineOperation,
        table_id: TableId,
        generation: u64,
    },
    CatalogPublicationStaged {
        operation: EngineOperation,
        state: TransitionState,
    },
    CompactionStaged {
        operation: EngineOperation,
        items: usize,
    },
    ReclamationCheckpointStaged {
        operation: EngineOperation,
        reclamation_kind: ReclamationKind,
        generation: u64,
        batch_id: u64,
        state: ReclamationState,
        phase: String,
    },
    CommitStarted {
        operation: EngineOperation,
    },
    CommitSucceeded {
        operation: EngineOperation,
    },
}

#[async_trait]
pub trait EngineEventHook: Send + Sync {
    async fn reach(&self, event: EngineEvent);
}

#[derive(Debug, Default)]
pub struct NoopEngineEventHook;

#[async_trait]
impl EngineEventHook for NoopEngineEventHook {
    async fn reach(&self, _event: EngineEvent) {}
}
