package pirwire

import (
	"bytes"
	"encoding/json"
	"fmt"
)

// An LIR query document — the statement's relational body. This schema
// treats it as an opaque value and does not describe the LIR grammar; the
// server validates it against the independent LIR schema. See the LIR
// specification at https://www.radengine.dev/schema/lir.json.
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
// within one implicit transaction. Each statement evaluates against the
// transaction's initial snapshot plus the complete effects of all preceding
// statements, but never observes its own effects while deriving its input. A
// statement exposes its result as a relational value that later statements may
// consume, by name, through an ordinary LIR `ref` — statement names share the
// binding namespace. If any statement or the commit fails, the whole program
// fails and no effects become externally visible. Exactly one statement result
// is returned to the caller.
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
// `relation` field, opaque to this schema. Validation is two-phase: this
// schema checks the program envelope and statement grammar, and the LIR schema
// validates each `relation` independently. Cross-statement references (a `ref`
// to a statement name) and the backward-only reference rule are resolved by the
// engine at bind time, not by either schema.
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
// statement's start state and its rows become the statement's result, shaped
// by the LIR root's cardinality when this statement is the program result.
type QueryStatement struct {
	Kind     string        `json:"kind"`
	Name     StatementName `json:"name"`
	Relation Relation      `json:"relation"`
}

func (QueryStatement) isStatement() {}

func (QueryStatement) StatementType() string { return "query" }

// Insert the rows of `relation` into `table`. The relation's output columns
// map to target columns by name; omitted columns take their schema default
// (generators applied per row); unknown columns are rejected; cell types
// must be assignable to the catalog columns. The statement's result is the
// created rows, defaults included.
type CreateStatement struct {
	Kind     string        `json:"kind"`
	Name     StatementName `json:"name"`
	Relation Relation      `json:"relation"`
	Table    TableName     `json:"table"`
}

func (CreateStatement) isStatement() {}

func (CreateStatement) StatementType() string { return "create" }

// Update rows of `table`, identified and assigned by the schema of
// `relation`: its output must include the target's full primary key (which
// identifies the rows) plus the columns to assign. Columns absent from the
// relation are left unchanged; a NULL assigns NULL. Each input row must
// identify exactly one existing row, and no target may be identified twice.
// The statement's result is the post-image of the updated rows.
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
// exactly one existing row. The statement's result is the pre-image of the
// deleted rows.
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
