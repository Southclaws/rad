package bind

// Program binding and planning. A PIR program is an ordered list of
// statements sharing one transaction. Each statement's relation binds with
// every earlier statement's result available as a program binding — a
// first-class relational value referenced by an ordinary `ref` node — and
// the whole program binds into one dense slot space so that a statement
// result's frames (keyed by its result schema's slots) remap cleanly through
// the refs in later statements.
//
// ProgramBinder is stateful so the executor may bind each relational
// statement only after applying preceding catalog statements. Its Catalog
// reader is transaction-backed and therefore observes that evolving schema.
//
// A statement's result schema is what later refs observe: for a query it is
// the relation's root output; for a mutation it is the target table's full
// row type (created rows, or the post-/pre-image), independent of the input
// relation's shape. Mutations therefore bind their input as a bag (no
// observable-order rule) and carry the target table in.

import (
	"context"
	"fmt"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
	"github.com/Southclaws/rad/rad/engine/04_planner/physical"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// ProgramStmt is one statement's binding input: its program-scope name, its
// relation, and — for a mutation — the target table whose row type is the
// statement's result schema.
type ProgramStmt struct {
	Name     string
	Rel      lir.Query
	Mutation bool
	Table    string
}

// BoundStatement is one statement bound and planned within the program's
// shared slot space. Plan evaluates the input relation; ResultOut is the
// statement result's schema (the row type later refs observe, and the shape
// the executor keys result frames by); ResultCard shapes the result when the
// statement is the program result.
type BoundStatement struct {
	Name       string
	Plan       *physical.PhysPlan
	ResultOut  lir.RowType
	ResultCard lir.RootCard
}

// ProgramBinder binds relational statements in program order while retaining
// the result bindings and dense slot space established by earlier statements.
type ProgramBinder struct {
	b       *binder
	program map[string]*bound.Binding
	names   []string
	next    int
}

// NewProgramBinder reserves the complete statement namespace so local LIR
// bindings cannot make reference resolution depend on statement order.
func NewProgramBinder(ctx context.Context, cat Catalog, names []string) (*ProgramBinder, error) {
	reserved := make(map[string]bool, len(names))
	for _, name := range names {
		if name == "" {
			return nil, reject.Inputf("planner: statement name must not be empty")
		}
		if reserved[name] {
			return nil, reject.Inputf("planner: duplicate statement name %q", name)
		}
		reserved[name] = true
	}
	return &ProgramBinder{
		b: &binder{
			ctx:      ctx,
			cat:      cat,
			labels:   map[string]bool{},
			bindings: map[string]*bound.Binding{},
			reserved: reserved,
		},
		program: map[string]*bound.Binding{},
		names:   append([]string(nil), names...),
	}, nil
}

// Bind binds and plans the next relational statement against the catalog and
// program results visible at this point, then publishes its result binding.
func (p *ProgramBinder) Bind(s ProgramStmt) (BoundStatement, error) {
	if p.next >= len(p.names) {
		return BoundStatement{}, reject.Inputf("planner: unexpected statement %q", s.Name)
	}
	if want := p.names[p.next]; s.Name != want {
		return BoundStatement{}, reject.Inputf("planner: expected statement %q, got %q", want, s.Name)
	}

	var (
		bq         *bound.Query
		err        error
		resultOut  lir.RowType
		resultCard lir.RootCard
		resultRoot bound.Relation
	)
	if s.Mutation {
		bq, err = p.b.bindBag(s.Rel, p.program)
		if err != nil {
			return BoundStatement{}, statementErr(s.Name, err)
		}
		resultCard = lir.CardMany
	} else {
		bq, err = p.b.bindQuery(s.Rel, p.program)
		if err != nil {
			return BoundStatement{}, statementErr(s.Name, err)
		}
		resultRoot = bq.Root
		resultOut = bq.Root.Output()
		resultCard = bq.Card
	}

	pp := planner.PlanQuery(bq)
	p.b.nextSlot = pp.Slots
	if s.Mutation {
		schema, err := p.b.tableSchema(s.Table)
		if err != nil {
			return BoundStatement{}, statementErr(s.Name, err)
		}
		resultRoot = schema
		resultOut = schema.Output()
	}
	p.program[s.Name] = &bound.Binding{Name: s.Name, Root: resultRoot}
	p.next++
	return BoundStatement{Name: s.Name, Plan: pp, ResultOut: resultOut, ResultCard: resultCard}, nil
}

// tableSchema builds a fresh-slot relation whose output is the table's full
// row type — the schema a mutation statement's result exposes. It allocates
// from the shared slot counter so the result frames remap through later refs.
func (b *binder) tableSchema(table string) (bound.Relation, error) {
	if table == "" {
		return nil, reject.Inputf("planner: mutation statement needs a target table")
	}
	tbl, ok, err := b.cat.GetTable(b.ctx, table)
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, reject.Fail(reject.ReasonUnknownTable, "planner: unknown table %q", table)
	}
	return bound.NewScan(tbl, "", b.freshSlots(len(tbl.Columns))), nil
}

// statementErr annotates a statement-body error with the statement's name;
// %w keeps the reject classification intact through the wrap.
func statementErr(name string, err error) error {
	return fmt.Errorf("planner: statement %q: %w", name, err)
}
