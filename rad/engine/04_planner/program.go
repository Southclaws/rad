package planner

// Program binding and planning. A PIR program is an ordered list of
// statements sharing one transaction. Each statement's relation binds with
// every earlier statement's result available as a program binding — a
// first-class relational value referenced by an ordinary `ref` node — and
// the whole program binds into one dense slot space so that a statement
// result's frames (keyed by its root's output slots) remap cleanly through
// the refs in later statements.
//
// Binding and planning are pure over the catalog schema, which no statement
// changes (mutations touch rows, not table definitions), so the whole
// program is bound and planned up front; execution then runs the plans in
// order, threading effects through the transaction. The planner is blind to
// statement kind — it plans the relation; the executor interprets the kind.

import (
	"context"
	"fmt"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// ProgramStmt is one statement's binding input: its program-scope name and
// its relation. Kind and target table are the executor's concern, carried
// alongside by the caller.
type ProgramStmt struct {
	Name string
	Rel  lir.Query
}

// BoundStatement is one statement bound and planned within the program's
// shared slot space.
type BoundStatement struct {
	Name string
	Plan *PhysPlan
}

// BindProgram binds and plans every statement in order. Each statement sees
// the results of all preceding statements as program bindings; a statement
// name may not be referenced before it is defined (backward-only), which
// falls out of registering each result only after binding it. Names must be
// unique.
func BindProgram(ctx context.Context, cat Catalog, stmts []ProgramStmt) ([]BoundStatement, error) {
	b := &binder{ctx: ctx, cat: cat, labels: map[string]bool{}, bindings: map[string]*bound.Binding{}}
	program := map[string]*bound.Binding{}
	out := make([]BoundStatement, 0, len(stmts))

	for _, s := range stmts {
		if s.Name == "" {
			return nil, reject.Inputf("planner: statement name must not be empty")
		}
		if _, dup := program[s.Name]; dup {
			return nil, reject.Inputf("planner: duplicate statement name %q", s.Name)
		}
		bq, err := b.bindQuery(s.Rel, program)
		if err != nil {
			return nil, statementErr(s.Name, err)
		}
		pp := PlanQuery(bq)
		b.nextSlot = pp.Slots // planner-allocated crossing slots stay unique program-wide

		// The result enters the program binding namespace under the
		// statement's name; its root's output schema gives later refs their
		// canonical slots.
		program[s.Name] = &bound.Binding{Name: s.Name, Root: bq.Root}
		out = append(out, BoundStatement{Name: s.Name, Plan: pp})
	}
	return out, nil
}

// statementErr annotates a statement-body error with the statement's name;
// %w keeps the reject classification intact through the wrap.
func statementErr(name string, err error) error {
	return fmt.Errorf("planner: statement %q: %w", name, err)
}
