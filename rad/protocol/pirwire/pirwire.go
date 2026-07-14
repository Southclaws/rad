package pirwire

import (
	"bytes"
	"encoding/json"
	"fmt"
)

// An LIR query document — the statement's relational body, and always a
// JSON object.
//
// At this (PIR) schema layer the value is structurally opaque: PIR does not
// describe the LIR grammar. The `raw` format is a code-generation annotation
// that maps the field to a raw-JSON wire type (bytes, so int64 literals keep
// full precision), not an additional JSON Schema validation constraint — a
// generic validator imposes no structure here and accepts any JSON, though
// an LIR document is always an object in practice. (A `type: object`
// constraint would be semantically accurate but defeats the raw-bytes
// generator mapping, so it is deliberately omitted.) Rad validates the
// document against the independent LIR schema in the second validation
// phase, and binds it against the catalog and prior statement results in
// the third. See the LIR specification at
// https://www.radengine.dev/schema/lir.json.
type Relation = json.RawMessage

// A statement's name: unique within the program, and the label under which
// its result enters the program binding namespace for later statements.
type StatementName = string

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
// within one implicit transaction. Each statement evaluates its complete input
// relation against the transaction's initial snapshot plus the complete effects
// of all preceding statements, but never observes its own effects while
// deriving that input.
//
// A mutation statement (`create`, `update`, `delete`) derives one logical
// mutation set from that complete relation, validates constraints against the
// post-statement state the whole set produces, then applies the set atomically.
// Input row encounter order has no semantic effect: there is no per-row
// interleaving of read, write, and check, so a create batch whose rows
// reference each other and an update that swaps two unique values both succeed
// when the end state is valid, and no partial statement effect is ever
// observable — not even to the rest of the program.
//
// Each successful statement produces one logical result relation. Its name
// enters the program binding namespace, and later statements consume it through
// an ordinary LIR `ref` — statement names and statement-local binding names
// share one namespace. A statement-result binding is produced exactly once and
// is never replayed: consuming it never re-executes its producing statement
// (unlike an ordinary LIR binding, whose single reference the planner may
// replay). References may only point backwards. A statement always has a
// logical result relation, but the engine need not construct or retain it when
// it is neither referenced by a later statement nor selected as the program
// result — an unreferenced 100,000-row delete may keep only its affected count.
//
// If any statement or the commit fails, the whole program fails and no effects
// become externally visible. Exactly one statement result is returned to the
// caller.
//
// There are four statement kinds. `query` reads. `create`, `update`, and
// `delete` mutate, and consume a *relation* rather than a literal row payload —
// a literal row is simply a one-row relation (LIR's `rows` node). LIR stays a
// pure relational language with no execution-order semantics; ordering and
// effects live here, in PIR.
//
// # The wire and its layers
//
// A program is `{ statements: [...], result? }`. Statements are an ordered
// array, never a map: statement order is the execution order, and a JSON
// object would not preserve it. Each statement carries an LIR document in its
// `relation` field, opaque to this schema. Validation runs in three phases:
//
//  1. this schema validates the program envelope and statement grammar;
//  2. the independent LIR schema validates each `relation` on its own;
//  3. the engine binds and type-checks the whole program in statement order,
//     against the catalog and each earlier statement's result schema.
//
// The first two phases are per-document and independent; the third is not,
// because a statement's relation may reference an earlier statement's result.
// Phase three is where backward references, result schemas, namespace
// collisions, the create/update/delete column shapes, type assignability, and
// the result selector are checked — so a malformed program is rejected before
// any effect is applied.
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
// non-primary-key column to assign. PIR v1 does not change row identity, so
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
// exactly one existing row, and no target may be identified twice — the
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

// A complete execution program: an ordered list of statements plus an
// optional selector naming the statement whose result is returned.
//
// `statements` executes in document order within one transaction. A
// statement's result becomes available, under its name, to every later
// statement — the program binding namespace, consumed through LIR `ref`
// nodes. References may only point backwards; the engine enforces that at
// bind time.
//
// `result` names the statement whose relation is returned to the caller.
// It may be omitted only when the program has exactly one statement, which
// is then the result; a program with more than one statement must name its
// result explicitly, so appending a statement never silently changes the
// response. (This rule spans fields the schema cannot express alone; the
// server enforces it.)
//
// Statement names and statement-local binding names share one
// non-shadowing namespace: within any statement's LIR document, no local
// binding may carry the name of any statement in the program. Local
// bindings in separate statements may still repeat — their scopes are
// distinct. (Also enforced by the engine, not the schema.)
type Program struct {
	// The name of the statement whose result relation is returned.
	// Optional only for a single-statement program.
	//
	Result *string `json:"result,omitempty"`
	// The program's statements, in execution order. Names must be unique;
	// each is a fresh program binding available to later statements.
	//
	Statements []Statement `json:"statements"`
}
