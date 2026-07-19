// Package sql compiles PostgreSQL statements into Rad wire programs: a
// pgparser AST binds against a catalog snapshot and lowers to PIR statements
// over LIR relations, the same protocol documents any client sends to
// /execute. It is the dialect frontend; transport (the Postgres wire
// listener) lives in rad/server/pgwire.
package sql

import (
	"fmt"
	"strings"

	"github.com/pgplex/pgparser/nodes"
	"github.com/pgplex/pgparser/parser"

	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

// Prepared is a parsed, bound statement: result shape and parameter types
// are known, so a wire frontend can answer Describe before any values are
// bound. Compile with concrete arguments produces the executable program.
type Prepared struct {
	Columns []ResultColumn
	Params  []ParamType
	Tag     string

	stmt    nodes.Node
	schema  *Schema
	tracker *paramTracker
	static  *Compiled
}

// Compiled is one executable statement: a PIR program plus everything the
// wire layer needs to render its outcome. A nil Program is a no-op (BEGIN,
// SET, already-satisfied DDL); Static short-circuits execution with canned
// rows (SHOW).
type Compiled struct {
	Program  *pirwire.Program
	Columns  []ResultColumn
	Card     string
	Tag      string
	TagStmts []string
	Static   [][]any
	DDL      bool
}

// Parse splits SQL text into statements.
func Parse(text string) ([]nodes.Node, error) {
	list, err := parser.Parse(text)
	if err != nil {
		return nil, err
	}
	if list == nil {
		return nil, nil
	}
	return list.Items, nil
}

// Prepare binds one statement against the schema snapshot, inferring
// parameter types and the result shape.
func Prepare(schema *Schema, stmt nodes.Node) (*Prepared, error) {
	tracker := &paramTracker{}
	compiled, err := compileStmt(schema, stmt, tracker, nil)
	if err != nil {
		return nil, err
	}
	p := &Prepared{
		Columns: compiled.Columns,
		Params:  append([]ParamType(nil), tracker.types...),
		Tag:     compiled.Tag,
		stmt:    stmt,
		schema:  schema,
		tracker: tracker,
	}
	if compiled.Program == nil {
		// Statements without parameters or data dependencies compile once.
		p.static = compiled
	}
	return p, nil
}

// Compile lowers the statement with bound argument values inlined as typed
// literals.
func (p *Prepared) Compile(args []lirwire.Value) (*Compiled, error) {
	if p.static != nil {
		return p.static, nil
	}
	return compileStmt(p.schema, p.stmt, p.tracker, args)
}

// program accumulates one SQL statement's compilation: PIR statements, the
// result selector, and wire-facing metadata.
type program struct {
	schema  *Schema
	tracker *paramTracker
	args    []lirwire.Value

	statements []pirwire.Statement
	result     string
	columns    []ResultColumn
	card       string
	tag        string
	tagStmts   []string
	static     [][]any
	noop       bool
	ddl        bool
}

func (p *program) newRelCC() *cc {
	return newCC(p.schema, p.tracker, p.args)
}

// relation marshals one finished LIR graph as a PIR statement relation,
// validating it against the LIR schema on the way out.
func (c *cc) relation(root, card string) (pirwire.Relation, error) {
	raw, err := protocol.MarshalQuery(c.query(root, card))
	if err != nil {
		return nil, err
	}
	return pirwire.Relation(raw), nil
}

func compileStmt(schema *Schema, stmt nodes.Node, tracker *paramTracker, args []lirwire.Value) (*Compiled, error) {
	p := &program{schema: schema, tracker: tracker, args: args}
	var err error
	switch v := stmt.(type) {
	case *nodes.SelectStmt:
		err = p.lowerSelectStmt(v)
	case *nodes.InsertStmt:
		err = p.lowerInsert(v)
	case *nodes.UpdateStmt:
		err = p.lowerUpdate(v)
	case *nodes.DeleteStmt:
		err = p.lowerDelete(v)
	case *nodes.CreateStmt:
		p.ddl = true
		err = p.lowerCreateTable(v)
	case *nodes.IndexStmt:
		p.ddl = true
		err = p.lowerCreateIndex(v)
	case *nodes.DropStmt:
		p.ddl = true
		err = p.lowerDrop(v)
	case *nodes.AlterTableStmt:
		p.ddl = true
		err = p.lowerAlterTable(v)
	case *nodes.TruncateStmt:
		err = p.lowerTruncate(v)
	case *nodes.TransactionStmt:
		p.lowerTransaction(v)
	case *nodes.VariableSetStmt:
		p.tag = "SET"
		p.noop = true
	case *nodes.VariableShowStmt:
		p.lowerShow(v)
	default:
		err = unsupportedf("statement %T", stmt)
	}
	if err != nil {
		return nil, err
	}

	out := &Compiled{
		Columns:  p.columns,
		Card:     p.card,
		Tag:      p.tag,
		TagStmts: p.tagStmts,
		Static:   p.static,
		DDL:      p.ddl,
	}
	if len(p.statements) > 0 {
		prog := pirwire.Prog(p.result, p.statements...)
		if err := protocol.ValidatePIRJSON(mustMarshalProgram(prog)); err != nil {
			return nil, fmt.Errorf("compiled program invalid: %w", err)
		}
		out.Program = &prog
	}
	return out, nil
}

func mustMarshalProgram(prog pirwire.Program) []byte {
	raw, err := protocol.MarshalProgram(prog)
	if err != nil {
		return []byte("{}")
	}
	return raw
}

func (p *program) lowerSelectStmt(sel *nodes.SelectStmt) error {
	c := p.newRelCC()
	out, err := c.lowerSelect(&env{}, sel, modeRoot)
	if err != nil {
		return err
	}
	rel, err := c.relation(out.root, out.card)
	if err != nil {
		return err
	}
	p.statements = append(p.statements, pirwire.Query("q", rel))
	p.result = "q"
	p.card = out.card
	p.tag = "SELECT"
	p.columns = make([]ResultColumn, len(out.cols))
	for i, cd := range out.cols {
		name := cd.wire
		if name == "" {
			name = cd.name
		}
		p.columns[i] = ResultColumn{Name: name, Key: cd.name, Scalar: cd.typ.scalar, Format: cd.typ.format}
	}
	return nil
}

func (p *program) lowerTransaction(ts *nodes.TransactionStmt) {
	// Transaction control is a wire-level fiction: every statement commits
	// its own atomic program, so these succeed without doing anything.
	// ROLLBACK therefore cannot undo prior statements — a known and accepted
	// divergence for this frontend.
	switch ts.Kind {
	case nodes.TRANS_STMT_BEGIN, nodes.TRANS_STMT_START:
		p.tag = "BEGIN"
	case nodes.TRANS_STMT_COMMIT:
		p.tag = "COMMIT"
	case nodes.TRANS_STMT_ROLLBACK:
		p.tag = "ROLLBACK"
	case nodes.TRANS_STMT_SAVEPOINT:
		p.tag = "SAVEPOINT"
	case nodes.TRANS_STMT_RELEASE:
		p.tag = "RELEASE"
	case nodes.TRANS_STMT_ROLLBACK_TO:
		p.tag = "ROLLBACK"
	default:
		p.tag = "COMMIT"
	}
	p.noop = true
}

// showValues answers the SHOW variables clients and migration tooling probe.
var showValues = map[string]string{
	"server_version":              "17.0",
	"server_version_num":          "170000",
	"transaction_isolation":       "serializable",
	"default_transaction_isolation": "serializable",
	"standard_conforming_strings": "on",
	"client_encoding":             "UTF8",
	"server_encoding":             "UTF8",
	"integer_datetimes":           "on",
	"timezone":                    "UTC",
	"is_superuser":                "on",
	"search_path":                 "public",
}

func (p *program) lowerShow(vs *nodes.VariableShowStmt) {
	name := strings.ToLower(vs.Name)
	value := showValues[name]
	p.tag = "SHOW"
	p.columns = []ResultColumn{{Name: name, Key: name, Scalar: lirwire.ScalarTypeText}}
	p.static = [][]any{{value}}
}
