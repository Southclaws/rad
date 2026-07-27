package model

// ColumnConversion selects the value conversion contract used while moving
// between physical column representations.
type ColumnConversion string

// ColumnConversionStrictBuiltin permits only Rad's checked built-in scalar
// conversions; failed values remain explicit transition violations.
const ColumnConversionStrictBuiltin ColumnConversion = "strict_builtin"

// ColumnReplacement is durable transition metadata: Source and Target are two
// immutable physical representations of one logical SchemaID. The catalog
// store persists it inside SchemaTransition and WriteProtocol JSON documents.
type ColumnReplacement struct {
	Source     Column           `json:"source"`
	Target     Column           `json:"target"`
	Conversion ColumnConversion `json:"conversion"`
}

// ColumnReplacementDef is the non-serialized client request for a physical
// column replacement transition.
type ColumnReplacementDef struct {
	Type          Type
	Nullable      bool
	Format        string
	Default       *Default
	Conversion    ColumnConversion
	Prerequisites []string
}

// ColumnReplacementRequest is the durable logical request retained while a
// transition waits for prerequisites. It deliberately names the source by
// stable schema ID rather than pinning the physical representation that a
// prerequisite may replace before activation.
type ColumnReplacementRequest struct {
	ColumnSchemaID SchemaID         `json:"column_schema_id"`
	Type           Type             `json:"type"`
	Nullable       bool             `json:"nullable"`
	Format         string           `json:"format,omitempty"`
	Default        *Default         `json:"default,omitempty"`
	Conversion     ColumnConversion `json:"conversion"`
}

// ConstraintKind selects the validation and foreground-enforcement protocol.
type ConstraintKind string

// ConstraintNotNull identifies the supported single-column non-null rule.
const ConstraintNotNull ConstraintKind = "not_null"

// ConstraintState is the durable lifecycle of declared and validating
// constraint metadata.
type ConstraintState string

// Constraint states distinguish inert declaration, foreground enforcement,
// historical validation, and terminal outcomes.
const (
	ConstraintDeclared           ConstraintState = "declared"
	ConstraintEnforcingNewWrites ConstraintState = "enforcing_new_writes"
	ConstraintValidatingExisting ConstraintState = "validating_existing_data"
	ConstraintValid              ConstraintState = "valid"
	ConstraintFailed             ConstraintState = "failed"
	ConstraintCancelled          ConstraintState = "cancelled"
)

// Constraint is operational catalog metadata persisted inside Table. A valid
// constraint's logical effect is also reflected in the canonical schema (for
// example, a valid not-null constraint makes Column.Nullable false).
type Constraint struct {
	ID                   string          `json:"id"`
	DefinitionGeneration uint64          `json:"definition_generation"`
	Name                 string          `json:"name"`
	Kind                 ConstraintKind  `json:"kind"`
	State                ConstraintState `json:"state"`
	ColumnIDs            []string        `json:"column_ids"`
}

// ConstraintDef is the non-serialized client request for a constraint
// validation transition.
type ConstraintDef struct {
	Name          string
	Kind          ConstraintKind
	ColumnID      SchemaID
	Prerequisites []string
}

// ConstraintValidationRequest preserves the logical target of a declared
// constraint while activation waits. Constraint.ColumnIDs remain physical
// execution identities and are rebound from ColumnSchemaID at activation.
type ConstraintValidationRequest struct {
	ConstraintID   string   `json:"constraint_id"`
	ColumnSchemaID SchemaID `json:"column_schema_id"`
}
