package model

import "time"

// RetentionOwnerKind identifies the class of long-lived consumer holding a
// durable resource pin.
type RetentionOwnerKind string

// Retention owner kinds are diagnostic; OwnerID supplies the exact identity.
const (
	RetentionOwnerPreparedPlan   RetentionOwnerKind = "prepared_plan"
	RetentionOwnerDataSnapshot   RetentionOwnerKind = "data_snapshot"
	RetentionOwnerReplica        RetentionOwnerKind = "replica"
	RetentionOwnerCDC            RetentionOwnerKind = "cdc"
	RetentionOwnerTransition     RetentionOwnerKind = "schema_transition"
	RetentionOwnerSchemaWorker   RetentionOwnerKind = "schema_worker"
	RetentionOwnerPhysicalReader RetentionOwnerKind = "physical_reader"
)

// RetentionResourceKind identifies the namespace in which an exact retained
// resource is addressed.
type RetentionResourceKind string

// Retention resource kinds intentionally keep catalog, data-position,
// transition, and physical-artifact horizons separate.
const (
	RetentionTableDefinition         RetentionResourceKind = "table_definition"
	RetentionWriteProtocolDefinition RetentionResourceKind = "write_protocol_definition"
	RetentionDataSnapshot            RetentionResourceKind = "data_snapshot"
	RetentionTransitionDeltas        RetentionResourceKind = "transition_deltas"
	RetentionPhysicalTable           RetentionResourceKind = "physical_table"
	RetentionPhysicalColumn          RetentionResourceKind = "physical_column"
	RetentionPhysicalIndex           RetentionResourceKind = "physical_index"
	RetentionTransitionDiagnostics   RetentionResourceKind = "transition_diagnostics"
)

// RetentionResource names exactly one durable resource protected by a pin.
// DataPosition remains opaque: its retention/release semantics belong to the
// KV backend, while the catalog records only the identity supplied by it.
type RetentionResource struct {
	Kind                    RetentionResourceKind `json:"kind"`
	TableID                 string                `json:"table_id,omitempty"`
	TableSchemaID           SchemaID              `json:"table_schema_id,omitempty"`
	ColumnID                string                `json:"column_id,omitempty"`
	IndexID                 string                `json:"index_id,omitempty"`
	DefinitionGeneration    uint64                `json:"definition_generation,omitempty"`
	WriteProtocolGeneration uint64                `json:"write_protocol_generation,omitempty"`
	TransitionID            string                `json:"transition_id,omitempty"`
	DataPosition            DataPosition          `json:"data_position,omitempty"`
}

// RetentionPin is a durable edge from a long-lived owner to one exact resource.
// The catalog store JSON-encodes it below /rad/catalog/retention_pin/{pin-id};
// these tags define internal Slate storage, not an API representation.
type RetentionPin struct {
	ID        string             `json:"id"`
	OwnerKind RetentionOwnerKind `json:"owner_kind"`
	OwnerID   string             `json:"owner_id"`
	Resource  RetentionResource  `json:"resource"`
	CreatedAt time.Time          `json:"created_at"`
}

// RetentionHorizon is one exact resource at the live-pin frontier. Multiple
// independent owners may retain the same resource.
type RetentionHorizon struct {
	Resource RetentionResource
	PinCount uint64
}

// RetentionHorizons is the derived resource-specific live-pin frontier. Exact
// resources are intentionally separate because no catalog generation can
// substitute for an opaque data position, transition delta stream, or physical
// range.
type RetentionHorizons struct {
	CatalogDefinitions    []RetentionHorizon
	DataSnapshots         []RetentionHorizon
	TransitionDeltas      []RetentionHorizon
	PhysicalArtifacts     []RetentionHorizon
	TransitionDiagnostics []RetentionHorizon
}
