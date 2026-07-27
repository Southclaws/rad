package model

import "time"

// DataPosition is an opaque backend position used for transition provenance
// and barriers. Catalog generations must never be compared with it.
type DataPosition string

// IndexState is the planner visibility and physical lifecycle of an index.
type IndexState string

// Index lifecycle states keep unfinished physical representations invisible to
// new plans.
const (
	IndexBuilding   IndexState = "building"
	IndexCatchingUp IndexState = "catching_up"
	IndexValidating IndexState = "validating"
	IndexReady      IndexState = "ready"
	IndexDeleting   IndexState = "deleting"
	IndexFailed     IndexState = "failed"
	IndexCancelled  IndexState = "cancelled"
)

// TransitionState is the durable lifecycle of a schema transition.
type TransitionState string

// Transition states are generic; individual transition kinds may skip states.
const (
	TransitionWaiting    TransitionState = "waiting"
	TransitionBuilding   TransitionState = "building"
	TransitionCatchingUp TransitionState = "catching_up"
	TransitionValidating TransitionState = "validating"
	TransitionReady      TransitionState = "ready"
	TransitionFailed     TransitionState = "failed"
	TransitionCancelled  TransitionState = "cancelled"
)

// TransitionKind selects a schema transition's activation, work, validation,
// publication, and cleanup protocol.
type TransitionKind string

// TransitionIndexBuild identifies an online index construction transition.
const TransitionIndexBuild TransitionKind = "index_build"

// Additional transition kinds share durable scheduling but keep distinct
// physical protocols.
const (
	TransitionColumnReplacement    TransitionKind = "column_replacement"
	TransitionConstraintValidation TransitionKind = "constraint_validation"
)

// TransitionWorkState summarizes retained-work pressure for diagnostics and
// foreground backpressure.
type TransitionWorkState string

// Transition work states are derived from durable progress and configured
// delta thresholds.
const (
	TransitionWorkNormal     TransitionWorkState = "normal"
	TransitionWorkDegraded   TransitionWorkState = "degraded"
	TransitionWorkWriteGated TransitionWorkState = "write_gated"
)

// Default transition delta thresholds bound retained foreground mutations when
// no explicit policy is supplied.
const (
	DefaultDeltaSoftLimit uint64 = 10_000
	DefaultDeltaHardLimit uint64 = 100_000
)

// WriteProtocol is the immutable set of obligations a mutation admitted at
// one table write-protocol generation must perform. The protocol value itself
// is the compatibility fence: replacing it conflicts with transactions that
// read an older generation.
//
// The catalog store JSON-encodes each generation at
// /rad/catalog/object/write_protocol/{table-id}/definition/{generation}. The
// tags are its durable Slate storage schema; WriteProtocol is not an API type.
type WriteProtocol struct {
	TableID            string                   `json:"table_id"`
	Generation         uint64                   `json:"generation"`
	ReadyIndexes       []Index                  `json:"ready_indexes,omitempty"`
	DeltaSinks         []IndexDeltaSink         `json:"delta_sinks,omitempty"`
	ColumnReplacements []ColumnReplacementWrite `json:"column_replacements,omitempty"`
	ConstraintChecks   []ConstraintCheck        `json:"constraint_checks,omitempty"`
	FinalizationGate   *SchemaFinalizationGate  `json:"finalization_gate,omitempty"`
}

// IndexDeltaSink is a write-protocol obligation that captures foreground table
// mutations for one online index build.
type IndexDeltaSink struct {
	TransitionID   string   `json:"transition_id"`
	Index          Index    `json:"index"`
	Columns        []string `json:"columns"`
	DeltaHardLimit uint64   `json:"delta_hard_limit,omitempty"`
}

// IndexBuildRequest is the durable logical definition retained while an
// online index waits for prerequisites. ColumnSchemaIDs preserve declared
// tuple order. Activation looks up these stable logical identities—not the
// caller's original names—to resolve the current physical columns after a
// preceding replacement or rename; current names are only reprojected into the
// materialized index definition.
type IndexBuildRequest struct {
	PhysicalID      string     `json:"physical_id"`
	LogicalID       string     `json:"logical_id"`
	Name            string     `json:"name"`
	ColumnSchemaIDs []SchemaID `json:"column_schema_ids"`
	Unique          bool       `json:"unique"`
}

// ColumnReplacementWrite is a write-protocol obligation that keeps a
// replacement physical column current while old rows are backfilled.
type ColumnReplacementWrite struct {
	TransitionID string            `json:"transition_id"`
	Replacement  ColumnReplacement `json:"replacement"`
}

// ConstraintCheck is a write-protocol obligation that enforces a constraint
// for foreground mutations while historical rows are validated.
type ConstraintCheck struct {
	TransitionID string     `json:"transition_id"`
	Constraint   Constraint `json:"constraint"`
}

// SchemaFinalizationGate is persisted inside a WriteProtocol definition. A
// table can be exclusively gated by one transition while it validates and
// atomically publishes its logical result. A transition that affects multiple
// tables installs one gate in each table's write protocol.
type SchemaFinalizationGate struct {
	TransitionID string         `json:"transition_id"`
	ObjectID     string         `json:"object_id"`
	Kind         TransitionKind `json:"kind"`
}

// SchemaTransition is durable worker and recovery state. A batch's physical
// writes and checkpoint commit in one bounded Slate transaction; BatchID and
// Cursor therefore identify the durable progress point without a separate
// applied-intent record. The catalog store persists this JSON document at
// /rad/catalog/transition/{transition-id}; the tags are an on-disk schema, not
// an HTTP contract.
type SchemaTransition struct {
	ID                   string                       `json:"id"`
	Kind                 TransitionKind               `json:"kind"`
	ObjectID             string                       `json:"object_id,omitempty"`
	State                TransitionState              `json:"state"`
	Generation           uint64                       `json:"generation"`
	OwnerEpoch           uint64                       `json:"owner_epoch"`
	SourceCatalogVersion uint64                       `json:"source_catalog_version"`
	BasePosition         DataPosition                 `json:"base_position"`
	BarrierPosition      DataPosition                 `json:"barrier_position,omitempty"`
	TableID              string                       `json:"table_id"`
	TableSchemaID        SchemaID                     `json:"table_schema_id"`
	AffectedColumnIDs    []SchemaID                   `json:"affected_column_schema_ids,omitempty"`
	Index                Index                        `json:"index"`
	IndexRequest         *IndexBuildRequest           `json:"index_build_request,omitempty"`
	ColumnReplacement    *ColumnReplacement           `json:"column_replacement,omitempty"`
	ReplacementRequest   *ColumnReplacementRequest    `json:"column_replacement_request,omitempty"`
	Constraint           *Constraint                  `json:"constraint,omitempty"`
	ConstraintRequest    *ConstraintValidationRequest `json:"constraint_validation_request,omitempty"`
	Prerequisites        []string                     `json:"prerequisites,omitempty"`
	GateTableIDs         []string                     `json:"gate_table_ids,omitempty"`
	Cursor               []byte                       `json:"cursor,omitempty"`
	BatchID              uint64                       `json:"batch_id,omitempty"`
	AppliedDelta         uint64                       `json:"applied_delta,omitempty"`
	DeltaHighWater       uint64                       `json:"delta_high_water,omitempty"`
	DeltaSoftLimit       uint64                       `json:"delta_soft_limit,omitempty"`
	DeltaHardLimit       uint64                       `json:"delta_hard_limit,omitempty"`
	WorkState            TransitionWorkState          `json:"work_state,omitempty"`
	RowsScanned          uint64                       `json:"rows_scanned,omitempty"`
	LastError            string                       `json:"last_error,omitempty"`
	CreatedAt            time.Time                    `json:"created_at"`
	UpdatedAt            time.Time                    `json:"updated_at"`
	CompactedAt          time.Time                    `json:"compacted_at,omitempty"`
}

// IndexDeltaOperation is the idempotent physical index mutation recorded in a
// transition delta stream.
type IndexDeltaOperation string

// Index delta operations replay a row's index entry into or out of existence.
const (
	IndexDeltaPut    IndexDeltaOperation = "put"
	IndexDeltaDelete IndexDeltaOperation = "delete"
)

// IndexDelta is a durable transition-log record JSON-encoded below
// /rad/catalog/transition_delta/{transition-id}/{sequence}. It is replayed by
// online-index workers and is not exposed through an API.
type IndexDelta struct {
	ID        string              `json:"id"`
	Sequence  uint64              `json:"sequence"`
	Operation IndexDeltaOperation `json:"operation"`
	PK        []byte              `json:"pk"`
	Tuple     []byte              `json:"tuple"`
}

// UniqueIndexClaim is a durable per-row proof used while an online unique
// index is building. The catalog store JSON-encodes it below
// /rad/catalog/transition_unique_claim/{transition-id}; the tags define that
// internal Slate schema.
type UniqueIndexClaim struct {
	Tuple []byte `json:"tuple"`
	PK    []byte `json:"pk"`
}

// TransitionControl is deliberately wire-facing: transition-start statements
// place it in the /execute summary, and the administrative API returns the same
// projection. Identity, kind, state, generation, prerequisites, and terminal
// error are normative. Progress and retained-work fields are advisory observations.
// Worker fencing epochs and storage positions remain internal SchemaTransition
// state.
type TransitionControl struct {
	Kind              string              `json:"kind"`
	TransitionID      string              `json:"transition_id"`
	ObjectID          string              `json:"object_id"`
	TransitionKind    TransitionKind      `json:"transition_kind"`
	State             TransitionState     `json:"state"`
	Generation        uint64              `json:"generation"`
	Prerequisites     []string            `json:"prerequisites"`
	RetainedWorkState TransitionWorkState `json:"retained_work_state,omitempty"`
	LastError         string              `json:"last_error,omitempty"`
	RowsScanned       uint64              `json:"rows_scanned,omitempty"`
	AppliedDelta      uint64              `json:"applied_delta,omitempty"`
	DeltaLag          uint64              `json:"delta_lag,omitempty"`
}

// Control projects durable internal transition state into its wire-facing
// control summary.
func (t SchemaTransition) Control() TransitionControl {
	lag := uint64(0)
	if t.DeltaHighWater > t.AppliedDelta {
		lag = t.DeltaHighWater - t.AppliedDelta
	}
	objectID := t.ObjectID
	if objectID == "" {
		objectID = t.Index.LogicalID
	}
	return TransitionControl{
		Kind:              "transition",
		TransitionID:      t.ID,
		ObjectID:          objectID,
		TransitionKind:    t.Kind,
		State:             t.State,
		Generation:        t.Generation,
		Prerequisites:     append([]string{}, t.Prerequisites...),
		RetainedWorkState: t.WorkState,
		LastError:         t.LastError,
		RowsScanned:       t.RowsScanned,
		AppliedDelta:      t.AppliedDelta,
		DeltaLag:          lag,
	}
}

// RefreshWorkState derives retained-work health from durable delta progress.
// Terminal transitions are always normal because their delta sink has already
// been removed from the table write protocol.
func (t *SchemaTransition) RefreshWorkState(highWater uint64) {
	t.DeltaHighWater = highWater
	if t.State == TransitionReady || t.State == TransitionFailed || t.State == TransitionCancelled {
		t.WorkState = TransitionWorkNormal
		return
	}
	lag := uint64(0)
	if highWater > t.AppliedDelta {
		lag = highWater - t.AppliedDelta
	}
	switch {
	case t.DeltaHardLimit > 0 && lag >= t.DeltaHardLimit:
		t.WorkState = TransitionWorkWriteGated
	case t.DeltaSoftLimit > 0 && lag >= t.DeltaSoftLimit:
		t.WorkState = TransitionWorkDegraded
	default:
		t.WorkState = TransitionWorkNormal
	}
}
