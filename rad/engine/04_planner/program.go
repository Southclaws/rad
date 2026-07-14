package planner

// Program binding and planning. A PIR program is an ordered list of
// statements sharing one transaction. Each statement's relation binds with
// every earlier statement's result available as a program binding — a
// first-class relational value referenced by an ordinary `ref` node — and
// the whole program binds into one dense slot space so that a statement
// result's frames (keyed by its result schema's slots) remap cleanly through
// the refs in later statements.
//
// Binding and planning are pure over the catalog schema, which no statement
// changes (mutations touch rows, not table definitions), so the whole
// program is bound and planned up front; execution then runs the plans in
// order, threading effects through the transaction.
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
	Plan       *PhysPlan
	ResultOut  lir.RowType
	ResultCard lir.RootCard
}

// BindProgram binds and plans every statement in order. Each statement sees
// the results of all preceding statements as program bindings; a statement
// name may not be referenced before it is defined (backward-only), which
// falls out of registering each result only after binding it. Names must be
// unique.
func BindProgram(ctx context.Context, cat Catalog, stmts []ProgramStmt) ([]BoundStatement, error) {
	// Reserve every statement name up front so a statement-local binding
	// cannot shadow a statement defined later in the program, not just an
	// earlier one.
	reserved := make(map[string]bool, len(stmts))
	for _, s := range stmts {
		if s.Name == "" {
			return nil, reject.Inputf("planner: statement name must not be empty")
		}
		if reserved[s.Name] {
			return nil, reject.Inputf("planner: duplicate statement name %q", s.Name)
		}
		reserved[s.Name] = true
	}

	b := &binder{ctx: ctx, cat: cat, labels: map[string]bool{}, bindings: map[string]*bound.Binding{}, reserved: reserved}
	program := map[string]*bound.Binding{}
	out := make([]BoundStatement, 0, len(stmts))

	for _, s := range stmts {

		var (
			bq         *bound.Query
			err        error
			resultOut  lir.RowType
			resultCard lir.RootCard
			resultRoot bound.Relation
		)
		if s.Mutation {
			bq, err = b.bindBag(s.Rel, program)
			if err != nil {
				return nil, statementErr(s.Name, err)
			}
			// A mutation's result is the target table's rows, whatever the
			// input relation's shape. Build a fresh-slot view of the table to
			// serve as the result schema (never planned — program results are
			// injected as frames, not re-evaluated).
			schema, serr := b.tableSchema(s.Table)
			if serr != nil {
				return nil, statementErr(s.Name, serr)
			}
			resultRoot = schema
			resultOut = schema.Output()
			resultCard = lir.CardMany
		} else {
			bq, err = b.bindQuery(s.Rel, program)
			if err != nil {
				return nil, statementErr(s.Name, err)
			}
			resultRoot = bq.Root
			resultOut = bq.Root.Output()
			resultCard = bq.Card
		}

		pp := PlanQuery(bq)
		b.nextSlot = pp.Slots // planner-allocated crossing slots stay unique program-wide

		program[s.Name] = &bound.Binding{Name: s.Name, Root: resultRoot}
		out = append(out, BoundStatement{Name: s.Name, Plan: pp, ResultOut: resultOut, ResultCard: resultCard})
	}
	return out, nil
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
		return nil, reject.Inputf("planner: unknown table %q", table)
	}
	slots := make([]lir.SlotID, len(tbl.Columns))
	for i := range slots {
		slots[i] = b.nextSlot
		b.nextSlot++
	}
	return bound.NewScan(tbl, "", slots), nil
}

// statementErr annotates a statement-body error with the statement's name;
// %w keeps the reject classification intact through the wrap.
func statementErr(name string, err error) error {
	return fmt.Errorf("planner: statement %q: %w", name, err)
}
