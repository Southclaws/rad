package pirwire

import (
	"bytes"
	"encoding/json"
	"fmt"
)

// A non-empty catalog name. PIR deliberately does not impose identifier
// syntax beyond non-emptiness; the catalog owns the shared naming rules for
// every schema and API surface.
type CatalogName = string

// PIR is Rad's program intermediate representation: the effectful layer above
// LIR. A client sends one program to `POST /execute`; the engine runs it as a
// single atomic unit. This schema is its normative specification. The type
// definitions and the prose in these descriptions are one artifact and move
// together.
//
// PIR is an internal contract with no external compatibility promise yet; it
// may change while the design is hardened.
//
// # Model
//
// A program is an ordered array of named statements executed sequentially
// within one implicit transaction. Relational statements evaluate their
// complete input relation against the transaction's initial snapshot plus the
// complete data and catalog effects of all preceding statements, but never
// observe their own effects while deriving that input.
//
// A mutation statement (`create`, `update`, `delete`) derives one logical
// mutation set from that complete relation, validates constraints against the
// post-statement state the whole set produces, then applies the set atomically.
// Input row encounter order has no semantic effect: there is no per-row
// interleaving of read, write, and check, so a create batch whose rows
// reference each other and an update that swaps two unique values both succeed
// when the end state is valid, and no partial statement effect is ever
// observable, not even to the rest of the program.
//
// Each successful relational statement produces one logical result relation.
// Its name enters the program binding namespace, and later relational
// statements consume it through an ordinary LIR `ref`, statement names and
// statement-local binding names share one namespace. A statement-result binding
// is produced exactly once and is never replayed: consuming it never
// re-executes its producing statement (unlike an ordinary LIR binding, whose
// single reference the planner may replay). References may only point
// backwards. The engine need not construct or retain a relational result when
// it is neither referenced by a later statement nor selected as the program
// result, an unreferenced 100,000-row delete may keep only its affected count.
//
// Catalog statements change the schema visible to every later statement in the
// same program. They have names for diagnostics and per-statement summaries,
// but produce no relation, do not enter the LIR binding namespace, and cannot be
// selected by `result`. Their effects are nevertheless ordinary transaction
// writes: a later failure rolls them back with the program, and no other client
// observes an intermediate catalog or data state.
//
// If any statement or the commit fails, the whole program fails and no effects
// become externally visible. A program with a selected relational result
// returns exactly that datum; a catalog-only program returns JSON null.
//
// There are two statement families. `query` reads data. `create`, `update`, and
// `delete` mutate data, and consume a *relation* rather than a literal row
// payload, a literal row is simply a one-row relation (LIR's `rows` node).
// `create_table`, `rename_table`, `delete_table`, `create_column`,
// `rename_column`, `change_column_default`, `delete_column`, `create_index`,
// `delete_index`, `start_index_build`, `start_column_replacement`,
// and `start_constraint_validation` mutate the catalog. Inspecting, listing,
// cancelling, and monitoring the durable work created by `start_*` statements
// are administrative operations exposed by the OpenAPI surface, not PIR
// statements.
//
// # Catalog changes
//
// PIR is the sole engine-level path for catalog-definition mutation and for
// starting durable schema work. A transport may keep ergonomic catalog
// endpoints such as `POST /tables`, but those endpoints are adapters: they
// resolve their name-based resource arguments, construct a one-statement PIR
// program, and execute it. They do not call a second catalog mutation path and
// acquire no independent transaction or revision semantics.
//
// Administrative lifecycle operations are deliberately separate. OpenAPI
// inspection/listing reads durable transition records, and cancellation is one
// atomic administrative catalog transaction. These operations do not compose
// with application data statements or gain PIR binding/result semantics.
//
// Existing tables and columns are addressed by their stable numeric schema IDs,
// not their mutable names. This makes a program deterministic across renames and
// prevents a stale name from selecting a different object. Indexes currently
// have no schema ID and are addressed by name within their table. New table and
// column definitions may omit IDs only when Direct mode is authorised to
// allocate them; a schema-managed migration must carry the IDs authored in
// `rad.schema.yaml`.
//
// Binding and preflight advance through a logical catalog in statement order.
// A data statement binds against all preceding catalog changes; a catalog
// statement validates against the catalog left by its predecessors. Renames
// therefore take effect immediately, tables and columns may be used only after
// creation and not after deletion, and a foreign key may target a table created
// earlier in the same program. Execution repeats that order against the real
// transaction. Data-dependent checks, such as building a unique index over
// existing rows, use the transaction-visible data left by preceding statements.
//
// Catalog authority is not granted by syntax. The caller supplies an execution
// policy that either forbids catalog statements, records each successful
// catalog statement as one revision, or groups the program's complete catalog
// work into one revision. Which entrypoints and catalog modes select each policy
// is product policy deliberately outside PIR. A failed program records no
// revisions.
//
// # The wire and its layers
//
// A program is `{ statements: [...], result? }`. Statements are an ordered
// array, never a map: statement order is the execution order, and a JSON
// object would not preserve it. Each relational statement carries an LIR
// document in its `relation` field, opaque to this schema. Validation runs in
// three phases:
//
//  1. this schema validates the program envelope and statement grammar;
//  2. the independent LIR schema validates each `relation` on its own;
//  3. the engine simulates catalog statements and binds relational statements
//     in program order, against the catalog visible at each statement and each
//     earlier relational statement's result schema.
//
// The first two phases are per-document and independent; the third is not,
// because a statement's relation may reference an earlier statement's result
// and its names resolve against an evolving catalog. Phase three is where
// backward references, result schemas, namespace collisions, catalog identity,
// the create/update/delete column shapes, type assignability, and the result
// selector are checked, so a malformed program is rejected before any effect
// is applied. Checks that depend on stored data remain execution-time checks and
// still roll back the complete program on failure.
type ColumnDefaultUnion interface {
	ColumnDefaultType() string
	isColumnDefault()
}

type ColumnDefault struct {
	ColumnDefaultUnion
}

func (w ColumnDefault) MarshalJSON() ([]byte, error) {
	if w.ColumnDefaultUnion == nil {
		return []byte("null"), nil
	}
	return json.Marshal(w.ColumnDefaultUnion)
}

func (w *ColumnDefault) UnmarshalJSON(data []byte) error {
	data = bytes.TrimSpace(data)
	if bytes.Equal(data, []byte("null")) {
		w.ColumnDefaultUnion = nil
		return nil
	}

	var peek struct {
		Type string `json:"kind"`
	}
	if err := json.Unmarshal(data, &peek); err != nil {
		return fmt.Errorf("ColumnDefault: invalid JSON: %w", err)
	}
	if peek.Type == "" {
		return fmt.Errorf("ColumnDefault: missing discriminator field %q", "kind")
	}

	var v ColumnDefaultUnion
	switch peek.Type {
	case "generator":
		v = &GeneratorDefault{}
	case "literal":
		v = &LiteralDefault{}
	default:
		return fmt.Errorf("ColumnDefault: unknown type %q", peek.Type)
	}

	if err := json.Unmarshal(data, v); err != nil {
		return fmt.Errorf("ColumnDefault: invalid %q payload: %w", peek.Type, err)
	}

	w.ColumnDefaultUnion = v
	return nil
}

type GeneratorDefault struct {
	// A builtin generator; `uuid` requires text and `now_ms` requires int64.
	Func string `json:"func"`
	Kind string `json:"kind"`
}

func (GeneratorDefault) isColumnDefault() {}

func (GeneratorDefault) ColumnDefaultType() string { return "generator" }

type LiteralDefault struct {
	Kind string `json:"kind"`
	// A non-null JSON string, number, or boolean.
	Value json.RawMessage `json:"value"`
}

func (LiteralDefault) isColumnDefault() {}

func (LiteralDefault) ColumnDefaultType() string { return "literal" }

// The closed set of physical scalar column types.
type ColumnType string

const (
	ColumnTypeText    ColumnType = "text"
	ColumnTypeInt64   ColumnType = "int64"
	ColumnTypeFloat64 ColumnType = "float64"
	ColumnTypeBool    ColumnType = "bool"
)

var ColumnTypeValues = []ColumnType{
	ColumnTypeText,
	ColumnTypeInt64,
	ColumnTypeFloat64,
	ColumnTypeBool,
}

// A human-authored stable logical identity. Table IDs are unique for the
// lifetime of the database; column IDs are unique for the lifetime of their
// table. Zero is reserved for Direct-mode allocation and is represented by
// omitting an ID from a new definition, never by sending zero.
type SchemaID = int

// The logical definition of a column. `id` is stable within the table and
// optional only for Direct-mode allocation. `format` is uninterpreted
// semantic metadata for tooling and code generation. Default compatibility
// with the column type is checked during catalog preflight.
type ColumnDefinition struct {
	Default  *ColumnDefault `json:"default,omitempty"`
	Format   *string        `json:"format,omitempty"`
	ID       *SchemaID      `json:"id,omitempty"`
	Name     CatalogName    `json:"name"`
	Nullable *bool          `json:"nullable,omitempty"`
	Type     ColumnType     `json:"type"`
}

// An opaque durable schema-transition identity returned by Rad. Transition
// identities are unique for the lifetime of the database and are never
// reused, including after terminal-record reclamation. They are stable to
// store and log and may be compared for equality, but callers must not
// interpret their representation.
type TransitionID = string

// The target value semantics for one physical column replacement. The
// logical ID and name come from the source column and cannot change here.
// `nullable` is required so a physical transition cannot accidentally
// tighten or loosen nullability through an omitted default.
type ColumnReplacementDefinition struct {
	Conversion    *string        `json:"conversion,omitempty"`
	Default       *ColumnDefault `json:"default,omitempty"`
	Format        *string        `json:"format,omitempty"`
	Nullable      bool           `json:"nullable"`
	Prerequisites []TransitionID `json:"prerequisites,omitempty"`
	Type          ColumnType     `json:"type"`
}

// One constraint declaration plus its historical-validation prerequisites.
// Additional constraint kinds can extend this tagged definition without
// changing the generic administrative transition-control surface.
type ConstraintValidationDefinition struct {
	ColumnID      SchemaID       `json:"column_id"`
	Kind          string         `json:"kind"`
	Name          CatalogName    `json:"name"`
	Prerequisites []TransitionID `json:"prerequisites,omitempty"`
}

// One foreign key. `columns` names columns on the new table; `ref_table`
// names a table visible at this statement (or the new table itself), and
// `ref_columns` must be that table's complete primary key in key order.
type ForeignKeyDefinition struct {
	Columns    []CatalogName `json:"columns"`
	Name       CatalogName   `json:"name"`
	RefColumns []CatalogName `json:"ref_columns"`
	RefTable   CatalogName   `json:"ref_table"`
}

// One secondary index; column order is the index key order.
type IndexDefinition struct {
	Columns []CatalogName `json:"columns"`
	Name    CatalogName   `json:"name"`
	Unique  *bool         `json:"unique,omitempty"`
}

// An LIR query document, the statement's relational body, and always a
// JSON object.
//
// At this (PIR) schema layer the value is structurally opaque: PIR does not
// describe the LIR grammar. The `raw` format is a code-generation annotation
// that maps the field to a raw-JSON wire type (bytes, so int64 literals keep
// full precision), not an additional JSON Schema validation constraint, a
// generic validator imposes no structure here and accepts any JSON, though
// an LIR document is always an object in practice. (A `type: object`
// constraint would be semantically accurate but defeats the raw-bytes
// generator mapping, so it is deliberately omitted.) Rad validates the
// document against the independent LIR schema in the second validation
// phase, and binds it against the catalog and prior statement results in
// the third. See the
// [published LIR specification](https://www.radengine.dev/schema/lir.json).
type Relation = json.RawMessage

// A statement's unique program-local name. Relational statement names label
// their result bindings; catalog statement names identify their diagnostic
// and summary entries but do not create bindings.
type StatementName = string

// The complete logical definition of a new table. It mirrors the catalog's
// canonical table definition and contains no opaque physical storage IDs.
// `id` is optional only for Direct-mode allocation. Column order and primary
// key order are significant; index and foreign-key order is not.
type TableDefinition struct {
	Columns     []ColumnDefinition     `json:"columns"`
	ForeignKeys []ForeignKeyDefinition `json:"foreign_keys,omitempty"`
	ID          *SchemaID              `json:"id,omitempty"`
	Indexes     []IndexDefinition      `json:"indexes,omitempty"`
	Name        CatalogName            `json:"name"`
	// The primary-key column names, in key order.
	PrimaryKey []CatalogName `json:"primary_key"`
}

// The catalog table a mutation statement targets.
type TableName = string

// PIR is Rad's program intermediate representation: the effectful layer above
// LIR. A client sends one program to `POST /execute`; the engine runs it as a
// single atomic unit. This schema is its normative specification. The type
// definitions and the prose in these descriptions are one artifact and move
// together.
//
// PIR is an internal contract with no external compatibility promise yet; it
// may change while the design is hardened.
//
// # Model
//
// A program is an ordered array of named statements executed sequentially
// within one implicit transaction. Relational statements evaluate their
// complete input relation against the transaction's initial snapshot plus the
// complete data and catalog effects of all preceding statements, but never
// observe their own effects while deriving that input.
//
// A mutation statement (`create`, `update`, `delete`) derives one logical
// mutation set from that complete relation, validates constraints against the
// post-statement state the whole set produces, then applies the set atomically.
// Input row encounter order has no semantic effect: there is no per-row
// interleaving of read, write, and check, so a create batch whose rows
// reference each other and an update that swaps two unique values both succeed
// when the end state is valid, and no partial statement effect is ever
// observable, not even to the rest of the program.
//
// Each successful relational statement produces one logical result relation.
// Its name enters the program binding namespace, and later relational
// statements consume it through an ordinary LIR `ref`, statement names and
// statement-local binding names share one namespace. A statement-result binding
// is produced exactly once and is never replayed: consuming it never
// re-executes its producing statement (unlike an ordinary LIR binding, whose
// single reference the planner may replay). References may only point
// backwards. The engine need not construct or retain a relational result when
// it is neither referenced by a later statement nor selected as the program
// result, an unreferenced 100,000-row delete may keep only its affected count.
//
// Catalog statements change the schema visible to every later statement in the
// same program. They have names for diagnostics and per-statement summaries,
// but produce no relation, do not enter the LIR binding namespace, and cannot be
// selected by `result`. Their effects are nevertheless ordinary transaction
// writes: a later failure rolls them back with the program, and no other client
// observes an intermediate catalog or data state.
//
// If any statement or the commit fails, the whole program fails and no effects
// become externally visible. A program with a selected relational result
// returns exactly that datum; a catalog-only program returns JSON null.
//
// There are two statement families. `query` reads data. `create`, `update`, and
// `delete` mutate data, and consume a *relation* rather than a literal row
// payload, a literal row is simply a one-row relation (LIR's `rows` node).
// `create_table`, `rename_table`, `delete_table`, `create_column`,
// `rename_column`, `change_column_default`, `delete_column`, `create_index`,
// `delete_index`, `start_index_build`, `start_column_replacement`,
// and `start_constraint_validation` mutate the catalog. Inspecting, listing,
// cancelling, and monitoring the durable work created by `start_*` statements
// are administrative operations exposed by the OpenAPI surface, not PIR
// statements.
//
// # Catalog changes
//
// PIR is the sole engine-level path for catalog-definition mutation and for
// starting durable schema work. A transport may keep ergonomic catalog
// endpoints such as `POST /tables`, but those endpoints are adapters: they
// resolve their name-based resource arguments, construct a one-statement PIR
// program, and execute it. They do not call a second catalog mutation path and
// acquire no independent transaction or revision semantics.
//
// Administrative lifecycle operations are deliberately separate. OpenAPI
// inspection/listing reads durable transition records, and cancellation is one
// atomic administrative catalog transaction. These operations do not compose
// with application data statements or gain PIR binding/result semantics.
//
// Existing tables and columns are addressed by their stable numeric schema IDs,
// not their mutable names. This makes a program deterministic across renames and
// prevents a stale name from selecting a different object. Indexes currently
// have no schema ID and are addressed by name within their table. New table and
// column definitions may omit IDs only when Direct mode is authorised to
// allocate them; a schema-managed migration must carry the IDs authored in
// `rad.schema.yaml`.
//
// Binding and preflight advance through a logical catalog in statement order.
// A data statement binds against all preceding catalog changes; a catalog
// statement validates against the catalog left by its predecessors. Renames
// therefore take effect immediately, tables and columns may be used only after
// creation and not after deletion, and a foreign key may target a table created
// earlier in the same program. Execution repeats that order against the real
// transaction. Data-dependent checks, such as building a unique index over
// existing rows, use the transaction-visible data left by preceding statements.
//
// Catalog authority is not granted by syntax. The caller supplies an execution
// policy that either forbids catalog statements, records each successful
// catalog statement as one revision, or groups the program's complete catalog
// work into one revision. Which entrypoints and catalog modes select each policy
// is product policy deliberately outside PIR. A failed program records no
// revisions.
//
// # The wire and its layers
//
// A program is `{ statements: [...], result? }`. Statements are an ordered
// array, never a map: statement order is the execution order, and a JSON
// object would not preserve it. Each relational statement carries an LIR
// document in its `relation` field, opaque to this schema. Validation runs in
// three phases:
//
//  1. this schema validates the program envelope and statement grammar;
//  2. the independent LIR schema validates each `relation` on its own;
//  3. the engine simulates catalog statements and binds relational statements
//     in program order, against the catalog visible at each statement and each
//     earlier relational statement's result schema.
//
// The first two phases are per-document and independent; the third is not,
// because a statement's relation may reference an earlier statement's result
// and its names resolve against an evolving catalog. Phase three is where
// backward references, result schemas, namespace collisions, catalog identity,
// the create/update/delete column shapes, type assignability, and the result
// selector are checked, so a malformed program is rejected before any effect
// is applied. Checks that depend on stored data remain execution-time checks and
// still roll back the complete program on failure.
type StatementUnion interface {
	StatementType() string
	isStatement()
}

type Statement struct {
	StatementUnion
}

func (w Statement) MarshalJSON() ([]byte, error) {
	if w.StatementUnion == nil {
		return []byte("null"), nil
	}
	return json.Marshal(w.StatementUnion)
}

func (w *Statement) UnmarshalJSON(data []byte) error {
	data = bytes.TrimSpace(data)
	if bytes.Equal(data, []byte("null")) {
		w.StatementUnion = nil
		return nil
	}

	var peek struct {
		Type string `json:"kind"`
	}
	if err := json.Unmarshal(data, &peek); err != nil {
		return fmt.Errorf("Statement: invalid JSON: %w", err)
	}
	if peek.Type == "" {
		return fmt.Errorf("Statement: missing discriminator field %q", "kind")
	}

	var v StatementUnion
	switch peek.Type {
	case "query":
		v = &QueryStatement{}
	case "create":
		v = &CreateStatement{}
	case "update":
		v = &UpdateStatement{}
	case "delete":
		v = &DeleteStatement{}
	case "create_table":
		v = &CreateTableStatement{}
	case "rename_table":
		v = &RenameTableStatement{}
	case "delete_table":
		v = &DeleteTableStatement{}
	case "create_column":
		v = &CreateColumnStatement{}
	case "rename_column":
		v = &RenameColumnStatement{}
	case "change_column_default":
		v = &ChangeColumnDefaultStatement{}
	case "delete_column":
		v = &DeleteColumnStatement{}
	case "create_index":
		v = &CreateIndexStatement{}
	case "delete_index":
		v = &DeleteIndexStatement{}
	case "start_index_build":
		v = &StartIndexBuildStatement{}
	case "start_column_replacement":
		v = &StartColumnReplacementStatement{}
	case "start_constraint_validation":
		v = &StartConstraintValidationStatement{}
	default:
		return fmt.Errorf("Statement: unknown type %q", peek.Type)
	}

	if err := json.Unmarshal(data, v); err != nil {
		return fmt.Errorf("Statement: invalid %q payload: %w", peek.Type, err)
	}

	w.StatementUnion = v
	return nil
}

// Read a relation and expose it. The `relation` is evaluated against the
// statement's start state, and its output relation becomes the statement's
// result. Later statements may consume it through the program binding
// namespace; when selected as the program result, the same relation is
// returned to the caller, shaped by the LIR root's cardinality.
type QueryStatement struct {
	Kind     string        `json:"kind"`
	Name     StatementName `json:"name"`
	Relation Relation      `json:"relation"`
}

func (QueryStatement) isStatement() {}

func (QueryStatement) StatementType() string { return "query" }

// Create one stored row in `table` for every row of `relation`. The
// relation's output columns map to target columns by name; omitted columns
// take their schema default (generators applied per row); unknown columns
// are rejected; cell types must be assignable to the catalog columns. The
// statement's result is the created rows, defaults included.
type CreateStatement struct {
	Kind     string        `json:"kind"`
	Name     StatementName `json:"name"`
	Relation Relation      `json:"relation"`
	Table    TableName     `json:"table"`
}

func (CreateStatement) isStatement() {}

func (CreateStatement) StatementType() string { return "create" }

// Update rows of `table`, identified and assigned by the schema of
// `relation`: its output must include the target's full primary key, which
// identifies each target row but is not assigned, plus at least one
// non-primary-key column to assign. Updating never changes row identity, so
// a primary-key-only input is rejected. Columns absent from the relation
// are left unchanged; a NULL assigns NULL. Each input row must identify
// exactly one existing row, and no target may be identified twice. The
// statement's result is the post-image of the updated rows.
type UpdateStatement struct {
	Kind     string        `json:"kind"`
	Name     StatementName `json:"name"`
	Relation Relation      `json:"relation"`
	Table    TableName     `json:"table"`
}

func (UpdateStatement) isStatement() {}

func (UpdateStatement) StatementType() string { return "update" }

// Delete rows of `table` identified by `relation`, whose output must be
// exactly the target's primary-key columns. Each input row must identify
// exactly one existing row, and no target may be identified twice, the
// same strict invariant as update, so an accidental join multiplication is
// exposed rather than silently deduplicated. The statement's result is the
// pre-image of the deleted rows.
type DeleteStatement struct {
	Kind     string        `json:"kind"`
	Name     StatementName `json:"name"`
	Relation Relation      `json:"relation"`
	Table    TableName     `json:"table"`
}

func (DeleteStatement) isStatement() {}

func (DeleteStatement) StatementType() string { return "delete" }

// Create one table from a complete logical definition. Columns, primary
// key, secondary indexes, and foreign keys are installed as one statement.
// A foreign key may reference the new table itself or a table visible after
// preceding statements; forward references are rejected.
//
// `table.id` and every `column.id` are stable schema identities. A
// schema-managed migration must provide them. Direct mode may omit them and
// the catalog allocates identities transactionally; allocated identities
// become part of the resulting canonical schema and are never reused.
type CreateTableStatement struct {
	Kind  string          `json:"kind"`
	Name  StatementName   `json:"name"`
	Table TableDefinition `json:"table"`
}

func (CreateTableStatement) isStatement() {}

func (CreateTableStatement) StatementType() string { return "create_table" }

// Rename the table identified by `table_id` to `to`. The identity, stored
// rows, indexes, and inbound foreign keys are unchanged because physical
// references use opaque IDs. The new name is visible to every later
// statement; the old name is not. A conflicting name is rejected.
type RenameTableStatement struct {
	Kind    string        `json:"kind"`
	Name    StatementName `json:"name"`
	TableID SchemaID      `json:"table_id"`
	To      CatalogName   `json:"to"`
}

func (RenameTableStatement) isStatement() {}

func (RenameTableStatement) StatementType() string { return "rename_table" }

// Logically delete the table identified by `table_id`. The table is absent
// for every later statement in the program. Rad durably schedules its rows,
// indexes, and obsolete physical definitions for automatic bounded
// reclamation; no user maintenance command is required. Schema and physical
// identities are never reused. A table referenced by another live table's
// foreign key, or owning a nonterminal physical transition, cannot be
// deleted until that dependency is removed or settled.
type DeleteTableStatement struct {
	Kind    string        `json:"kind"`
	Name    StatementName `json:"name"`
	TableID SchemaID      `json:"table_id"`
}

func (DeleteTableStatement) isStatement() {}

func (DeleteTableStatement) StatementType() string { return "delete_table" }

// Append `column` to the table identified by `table_id`. The definition's
// stable ID is required in a schema-managed migration and may be allocated
// in Direct mode. Because rows written before this statement have no stored
// value for the new column, it must be nullable or carry a literal default.
// A nullable added column may carry a generator default: old rows read NULL,
// while future omitted writes materialize a generated value. A non-nullable
// added column requires a literal historical missing value; Rad never runs a
// generator while decoding an old row.
//
// The new column is visible to later statements. This operation does not
// backfill stored rows; their missing value is interpreted according to the
// nullable/default rule above.
type CreateColumnStatement struct {
	Column  ColumnDefinition `json:"column"`
	Kind    string           `json:"kind"`
	Name    StatementName    `json:"name"`
	TableID SchemaID         `json:"table_id"`
}

func (CreateColumnStatement) isStatement() {}

func (CreateColumnStatement) StatementType() string { return "create_column" }

// Rename the column identified by `column_id` within `table_id` to `to`.
// The operation rewrites every catalog reference to that column, including
// primary keys, indexes, and foreign keys. Stored rows are untouched because
// their cells use the column's physical ID. Later statements see only the
// new name.
type RenameColumnStatement struct {
	ColumnID SchemaID      `json:"column_id"`
	Kind     string        `json:"kind"`
	Name     StatementName `json:"name"`
	TableID  SchemaID      `json:"table_id"`
	To       CatalogName   `json:"to"`
}

func (RenameColumnStatement) isStatement() {}

func (RenameColumnStatement) StatementType() string { return "rename_column" }

// Change the current insert default of `column_id` within `table_id`.
// `default`, when present, may be a literal or generator compatible with
// the column type. Omitting it removes the insert default.
//
// This changes only future inserts that omit the column. It does not
// reinterpret existing stored rows: the immutable missing-value rule
// established when the physical column was created remains unchanged.
// Publication advances the table write protocol, so a writer admitted
// under the old default must retry rather than commit an omitted value
// after the new policy becomes visible. Readers are not invalidated.
type ChangeColumnDefaultStatement struct {
	ColumnID SchemaID       `json:"column_id"`
	Default  *ColumnDefault `json:"default,omitempty"`
	Kind     string         `json:"kind"`
	Name     StatementName  `json:"name"`
	TableID  SchemaID       `json:"table_id"`
}

func (ChangeColumnDefaultStatement) isStatement() {}

func (ChangeColumnDefaultStatement) StatementType() string { return "change_column_default" }

// Logically delete the column identified by `column_id` within `table_id`.
// The column is absent for every later statement. Rad durably schedules
// its sparse stored cells and obsolete physical definitions for automatic
// bounded reclamation; no user maintenance command is required. A
// primary-key column, or a column still used by an index or foreign key,
// cannot be deleted.
type DeleteColumnStatement struct {
	ColumnID SchemaID      `json:"column_id"`
	Kind     string        `json:"kind"`
	Name     StatementName `json:"name"`
	TableID  SchemaID      `json:"table_id"`
}

func (DeleteColumnStatement) isStatement() {}

func (DeleteColumnStatement) StatementType() string { return "delete_column" }

// Create `index` on the table identified by `table_id` and backfill it from
// every transaction-visible row. Registration and backfill are one atomic
// statement: the planner never observes an index without its entries. A
// unique index whose existing keys are not unique rejects the complete
// program. Rows written by preceding statements participate in the
// backfill; rows written later are maintained through the new index.
type CreateIndexStatement struct {
	Index   IndexDefinition `json:"index"`
	Kind    string          `json:"kind"`
	Name    StatementName   `json:"name"`
	TableID SchemaID        `json:"table_id"`
}

func (CreateIndexStatement) isStatement() {}

func (CreateIndexStatement) StatementType() string { return "create_index" }

// Logically delete `index` from the table identified by `table_id`. The
// planner cannot select it for later statements. Rad advances the index
// access fence and durably schedules its stored entries for automatic
// bounded reclamation; no user maintenance command is required. Index names
// are unique only within a table and, unlike tables and columns, do not
// currently carry stable schema IDs.
type DeleteIndexStatement struct {
	Index   CatalogName   `json:"index"`
	Kind    string        `json:"kind"`
	Name    StatementName `json:"name"`
	TableID SchemaID      `json:"table_id"`
}

func (DeleteIndexStatement) isStatement() {}

func (DeleteIndexStatement) StatementType() string { return "delete_index" }

// Start a durable, planner-invisible index build on the table identified by
// `table_id`. The index may be unique or non-unique. Without an active
// prerequisite, the statement succeeds only when the building index
// definition, durable transition record, foreground delta-capture
// obligation, and affected table write-protocol fence have committed
// atomically. Once capture is published, no later affected mutation may
// commit without participating in that captured write protocol.
//
// Rad allocates the transition identity and the index's logical and
// physical identities; `index` contains no caller-authored identity. An
// index name already used by the table is rejected normally. Bounded
// backfill and validation continue outside the program transaction. The
// statement summary reports one affected transition and contains a
// transition control result whose identity can be inspected or cancelled.
// This deliberately does not change the synchronous `create_index`
// contract.
//
// `prerequisites` identifies transitions already known to the caller;
// `after` identifies earlier transition-start statements in this program.
// Both may be present. During program preflight and execution Rad resolves
// `after` to the transition identities allocated by those statements,
// combines and deduplicates the two sets into ordinary durable prerequisite
// edges, and rejects self, forward, missing, failed, cancelled, and
// non-transition references. `after` has no distinct durable meaning after
// resolution.
// Because every edge targets an already-created transition and transition
// identities are never reused or repurposed, the resulting graph is
// acyclic and an identity alone names the required successful publication.
//
// At transition creation Rad resolves `index.columns` to an ordered list
// of stable logical column IDs and persists that intent. While any
// prerequisite is unsatisfied, the transition remains `waiting`: its index
// name and permanently unique logical and physical identities are reserved,
// but no table definition, delta sink, or write-protocol fence is
// published. Creation linearizes when the waiting transition, reservation,
// identities, and dependency edges commit. Its control result reports
// `state: waiting` and the durable prerequisite identities.
//
// A prerequisite is satisfied only by successful `ready` publication.
// Failed, cancelled, or subsequently missing prerequisites
// deterministically fail the waiting transition without publishing
// physical work. Terminal transitions are never restarted in place; a
// retry creates a new identity and therefore cannot satisfy an old edge.
// Cancelling or failing an inert waiter releases its name reservation so a
// new request may retry that name, but the retired transition, logical
// index, and physical index identities are never reused; retained
// diagnostics therefore cannot be mistaken for the new request.
//
// Once every prerequisite is ready, activation resolves the persisted
// logical column IDs—not the original column names—to their current
// physical representations and projects their current presentation names.
// In one catalog transaction it revalidates the table, logical columns,
// name reservation, prerequisite states, and current write protocol, then
// atomically publishes the planner-invisible building definition, delta
// capture obligation, transition state, write-protocol fence, and base
// position. Activation linearizes at that commit. Names are never used as
// lookup keys again during activation.
type StartIndexBuildStatement struct {
	// Earlier transition-start statements whose successful ready
	// publication is required before activation. Ordinary program order
	// only guarantees that those statements start first; it does not wait
	// for their bounded background work. Rad resolves each backward name
	// to the transition identity allocated by that statement in the same
	// transaction and merges it into `prerequisites`.
	//
	After         []StatementName `json:"after,omitempty"`
	Index         IndexDefinition `json:"index"`
	Kind          string          `json:"kind"`
	Name          StatementName   `json:"name"`
	Prerequisites []TransitionID  `json:"prerequisites,omitempty"`
	TableID       SchemaID        `json:"table_id"`
}

func (StartIndexBuildStatement) isStatement() {}

func (StartIndexBuildStatement) StatementType() string { return "start_index_build" }

// Start a durable physical replacement of `column_id` on `table_id`.
// Without an active prerequisite, success means the source and target
// physical definitions, transition record, conversion/dual-write
// obligation, and affected table write-protocol fence committed
// atomically. The source remains the only bindable logical representation
// until bounded backfill and validation atomically publish the target.
//
// The logical column retains its stable schema ID and current name. Rad
// allocates a distinct target physical ID and never decodes source bytes as
// the target type. `strict_builtin` is deterministic and deliberately
// rejects lossy or locale-dependent conversions. V1 rejects replacement of
// columns used by primary keys, indexes, or foreign keys until their
// physical access paths can participate in the same transition.
//
// `prerequisites` are older transition identities recorded as a durable
// acyclic dependency graph. Ready prerequisites permit immediate
// activation. A non-ready prerequisite creates a `waiting` transition
// containing only the logical request and dependency edges: it publishes
// no target physical definition or foreground write obligation. Once every
// prerequisite is ready, Rad re-resolves `column_id` through the current
// catalog, allocates the target, and atomically activates dual-write.
// Failed or cancelled prerequisites deterministically fail the waiter.
// Missing or already failed/cancelled prerequisites are rejected when the
// statement starts. The statement summary reports one affected transition
// and its control identity.
type StartColumnReplacementStatement struct {
	// Earlier transition-start statements whose successful ready
	// publication is required before activation. Program order alone
	// starts those operations but does not await their background work.
	// Rad resolves these names atomically and merges their identities into
	// `replacement.prerequisites` as ordinary durable edges.
	//
	After       []StatementName             `json:"after,omitempty"`
	ColumnID    SchemaID                    `json:"column_id"`
	Kind        string                      `json:"kind"`
	Name        StatementName               `json:"name"`
	Replacement ColumnReplacementDefinition `json:"replacement"`
	TableID     SchemaID                    `json:"table_id"`
}

func (StartColumnReplacementStatement) isStatement() {}

func (StartColumnReplacementStatement) StatementType() string { return "start_column_replacement" }

// Declare and start validating one constraint on `table_id`. Without an
// active prerequisite, success means the durable transition and foreground
// enforcement obligation committed atomically: writers admitted before
// enforcement conflict, and every later affected writer must obey the rule
// while historical rows are scanned.
//
// V1 supports a single-column `not_null` constraint. A successful
// validation atomically publishes canonical non-nullability. Historical
// validation failure explicitly removes foreground enforcement while
// retaining failed constraint and transition diagnostics; it never leaves
// a silently half-valid rule active. `prerequisites` have the same waiting
// semantics as column replacement. A waiting constraint remains declared
// but inert and publishes no foreground check. Activation re-resolves its
// stable logical column ID after every prerequisite is ready, so a
// preceding replacement may safely change the physical column identity.
type StartConstraintValidationStatement struct {
	// Earlier transition-start statements whose successful ready
	// publication is required before activation. Program order alone
	// starts those operations but does not await their background work.
	// Rad resolves these names atomically and merges their identities into
	// `constraint.prerequisites` as ordinary durable edges.
	//
	After      []StatementName                `json:"after,omitempty"`
	Constraint ConstraintValidationDefinition `json:"constraint"`
	Kind       string                         `json:"kind"`
	Name       StatementName                  `json:"name"`
	TableID    SchemaID                       `json:"table_id"`
}

func (StartConstraintValidationStatement) isStatement() {}

func (StartConstraintValidationStatement) StatementType() string {
	return "start_constraint_validation"
}

// A complete execution program: an ordered list of statements plus an
// optional selector naming the statement whose result is returned.
//
// `statements` executes in document order within one transaction. A
// statement's result becomes available, under its name, to every later
// statement, the program binding namespace, consumed through LIR `ref`
// nodes. References may only point backwards; the engine enforces that at
// bind time.
//
// `result` names the relational statement whose relation is returned to the
// caller. It cannot name a catalog statement. It may be omitted when the
// program contains only catalog statements (the returned datum is JSON
// null), or when the program consists of exactly one relational statement
// (that statement is the result). Every other program must name its result
// explicitly, so appending a statement never silently changes the response.
// These rules span fields the schema cannot express alone; the engine
// enforces them during preflight.
//
// All statement names are unique, including catalog statement names.
// Relational statement names and statement-local binding names share one
// non-shadowing namespace: within any statement's LIR document, no local
// binding may carry the name of a relational statement in the program.
// Local bindings in separate statements may still repeat, their scopes
// are distinct. (Also enforced by the engine, not the schema.)
type Program struct {
	// The name of the relational statement whose result relation is
	// returned. Catalog statements cannot be selected.
	//
	Result *string `json:"result,omitempty"`
	// The program's statements, in execution order. Names must be unique.
	// Each relational statement creates a fresh program binding available
	// to later relational statements; catalog statements do not.
	//
	Statements []Statement `json:"statements"`
}
