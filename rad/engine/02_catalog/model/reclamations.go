package model

import "time"

// ReclamationKind identifies the physical artifact or durable history owned by
// a cleanup job.
type ReclamationKind string

// Reclamation kinds are target-specific because each target has a different
// retirement proof and batch protocol.
const (
	ReclamationTable                   ReclamationKind = "table"
	ReclamationColumn                  ReclamationKind = "column"
	ReclamationIndex                   ReclamationKind = "index"
	ReclamationTableDefinition         ReclamationKind = "table_definition"
	ReclamationWriteProtocolDefinition ReclamationKind = "write_protocol_definition"
	ReclamationTransitionDeltas        ReclamationKind = "transition_deltas"
	ReclamationCancelledIndex          ReclamationKind = "cancelled_index"
	ReclamationFailedIndex             ReclamationKind = "failed_index"
	ReclamationReplacedColumn          ReclamationKind = "replaced_column"
	ReclamationCancelledReplacement    ReclamationKind = "cancelled_replacement"
	ReclamationFailedReplacement       ReclamationKind = "failed_replacement"
	ReclamationConstraintValidation    ReclamationKind = "constraint_validation"
)

// ReclamationState is the durable lifecycle of a physical cleanup job.
type ReclamationState string

// Reclamation states distinguish claimable work, owned work, and terminal
// outcomes.
const (
	ReclamationPending    ReclamationState = "pending"
	ReclamationReclaiming ReclamationState = "reclaiming"
	ReclamationReclaimed  ReclamationState = "reclaimed"
	ReclamationFailed     ReclamationState = "failed"
)

// Reclamation is the durable recovery record for one logically retired
// artifact. The catalog store JSON-encodes it below
// /rad/catalog/reclamation/{reclamation-id}; these tags define that internal
// Slate storage schema, not an HTTP or PIR representation.
//
// Physical work is performed in bounded transactions. Cursor and Phase are
// checkpointed atomically with each batch, while OwnerEpoch fences workers
// that were superseded after a crash or concurrent takeover.
type Reclamation struct {
	ID                      string           `json:"id"`
	Kind                    ReclamationKind  `json:"kind"`
	State                   ReclamationState `json:"state"`
	Generation              uint64           `json:"generation"`
	OwnerEpoch              uint64           `json:"owner_epoch"`
	RetiredCatalogVersion   uint64           `json:"retired_catalog_version,omitempty"`
	TableID                 string           `json:"table_id,omitempty"`
	TableSchemaID           SchemaID         `json:"table_schema_id,omitempty"`
	ColumnID                string           `json:"column_id,omitempty"`
	IndexID                 string           `json:"index_id,omitempty"`
	IndexIDs                []string         `json:"index_ids,omitempty"`
	DefinitionGeneration    uint64           `json:"definition_generation,omitempty"`
	WriteProtocolGeneration uint64           `json:"write_protocol_generation,omitempty"`
	TransitionID            string           `json:"transition_id,omitempty"`
	Phase                   string           `json:"phase,omitempty"`
	Cursor                  []byte           `json:"cursor,omitempty"`
	BatchID                 uint64           `json:"batch_id,omitempty"`
	ItemsReclaimed          uint64           `json:"items_reclaimed,omitempty"`
	LastError               string           `json:"last_error,omitempty"`
	CreatedAt               time.Time        `json:"created_at"`
	UpdatedAt               time.Time        `json:"updated_at"`
	CompactedAt             time.Time        `json:"compacted_at,omitempty"`
}
