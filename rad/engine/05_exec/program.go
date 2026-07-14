package exec

// Program execution: the effectful layer above the relation-graph read path.
// A program is an ordered list of statements run as one atomic unit. Every
// statement evaluates against the transaction's snapshot plus the effects of
// all preceding statements — the statement snapshot invariant, applied
// per-statement inside one transaction — and exposes its result as a
// program binding that later statements consume through an ordinary `ref`.
//
// Binding and planning happen up front (pure over the schema, which no
// statement changes); execution then runs the plans in document order. Each
// statement runs in its own executor seeded with the accumulated program
// results, so a statement's local bindings never leak into another's — only
// statement results, keyed by name, cross the boundary.

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// StatementKind selects a statement's behaviour.
type StatementKind string

const (
	StmtQuery  StatementKind = "query"
	StmtCreate StatementKind = "create"
	StmtUpdate StatementKind = "update"
	StmtDelete StatementKind = "delete"
)

// ProgramStatement is one unbound statement: its program-scope name, its
// kind, the target table for mutations, and its LIR relation.
type ProgramStatement struct {
	Name  string
	Kind  StatementKind
	Table string
	Rel   lir.Query
}

// Program is an ordered list of statements plus the name of the statement
// whose result is returned. Result may be empty only for a single-statement
// program, whose sole statement is the result.
type Program struct {
	Statements []ProgramStatement
	Result     string
}

// StatementResult is the lightweight per-statement outcome: its name and the
// number of rows it produced (created, updated, deleted, or read).
type StatementResult struct {
	Name     string
	Affected int
}

// ProgramResult is a program's outcome: the declared result statement's datum
// plus a per-statement summary.
type ProgramResult struct {
	Result     lir.Datum
	Statements []StatementResult
}

// ExecuteProgram runs a program as one atomic transaction: all effects commit
// together or none do. A commit-time conflict is retryable via IsConflict.
func (e *Engine) ExecuteProgram(ctx context.Context, prog Program) (ProgramResult, error) {
	resultName, err := resultStatement(prog)
	if err != nil {
		return ProgramResult{}, err
	}
	var out ProgramResult
	err = e.Txn(ctx, func(tx *Tx) error {
		r, err := e.runProgram(ctx, tx.txn, prog, resultName)
		out = r
		return err
	})
	return out, err
}

// resultStatement resolves which statement's result is returned: the explicit
// selector, or the sole statement of a one-statement program.
func resultStatement(prog Program) (string, error) {
	if prog.Result != "" {
		return prog.Result, nil
	}
	if len(prog.Statements) == 1 {
		return prog.Statements[0].Name, nil
	}
	return "", reject.Inputf("exec: a program with %d statements must name its result", len(prog.Statements))
}

func (e *Engine) runProgram(ctx context.Context, view kv.KV, prog Program, resultName string) (ProgramResult, error) {
	stmts := make([]planner.ProgramStmt, len(prog.Statements))
	for i, s := range prog.Statements {
		stmts[i] = planner.ProgramStmt{
			Name:     s.Name,
			Rel:      s.Rel,
			Mutation: s.Kind != StmtQuery,
			Table:    s.Table,
		}
	}
	planned, err := planner.BindProgram(ctx, catalog.NewReader(view), stmts)
	if err != nil {
		return ProgramResult{}, err
	}

	// Accumulated statement results, injected into each statement's executor
	// so its refs resolve to earlier results.
	program := map[string][]Frame{}
	summary := make([]StatementResult, len(prog.Statements))
	var result lir.Datum
	haveResult := false

	for i, bs := range planned {
		stmt := prog.Statements[i]
		frames, err := e.runStatement(ctx, view, stmt, bs, program)
		if err != nil {
			return ProgramResult{}, err
		}
		program[bs.Name] = frames
		summary[i] = StatementResult{Name: bs.Name, Affected: len(frames)}
		if bs.Name == resultName {
			d, err := shapeFrames(bs.ResultCard, bs.ResultOut, frames)
			if err != nil {
				return ProgramResult{}, err
			}
			result, haveResult = d, true
		}
	}
	if !haveResult {
		return ProgramResult{}, reject.Inputf("exec: result names unknown statement %q", resultName)
	}
	return ProgramResult{Result: result, Statements: summary}, nil
}

// runStatement evaluates one statement against the transaction view, with the
// accumulated program results available as bindings, and returns its result
// frames keyed by the statement's result schema. A query returns the
// relation's rows; a mutation evaluates its input relation, applies it as one
// set, and returns the affected rows (created, post-image, or pre-image).
func (e *Engine) runStatement(ctx context.Context, view kv.KV, stmt ProgramStatement, bs planner.BoundStatement, program map[string][]Frame) ([]Frame, error) {
	ex := newExecutor(view)
	for name, frames := range program {
		ex.bindings[name] = frames
	}
	if stmt.Kind == StmtQuery {
		return ex.runPlan(ctx, bs.Plan)
	}

	// Evaluate the input relation fully before touching storage — the
	// statement snapshot: the mutation cannot observe its own writes.
	inputFrames, err := ex.runPlan(ctx, bs.Plan)
	if err != nil {
		return nil, err
	}
	inputRows := framesToRows(bs.Plan.Out, inputFrames)

	tbl, err := tableIn(ctx, view, stmt.Table)
	if err != nil {
		return nil, err
	}
	var affected []lir.Row
	switch stmt.Kind {
	case StmtCreate:
		affected, err = e.applyCreate(ctx, view, tbl, inputRows)
	case StmtUpdate:
		affected, err = e.applyUpdate(ctx, view, tbl, bs.Plan.Out, inputRows)
	case StmtDelete:
		affected, err = e.applyDelete(ctx, view, tbl, bs.Plan.Out, inputRows)
	default:
		return nil, reject.Inputf("exec: unknown statement kind %q", stmt.Kind)
	}
	if err != nil {
		return nil, err
	}
	return rowsToFrames(bs.ResultOut, affected), nil
}

// framesToRows lifts input frames into scalar rows keyed by column name, per
// the input relation's schema.
func framesToRows(out lir.RowType, frames []Frame) []lir.Row {
	rows := make([]lir.Row, len(frames))
	for i, f := range frames {
		row := make(lir.Row, len(out.Fields))
		for _, fld := range out.Fields {
			v, err := f.ScalarAt(fld.Slot, fld.Name, fld.Type)
			if err != nil {
				v = lir.Null(fld.Type.Kind.CatalogType())
			}
			row[fld.Name] = v
		}
		rows[i] = row
	}
	return rows
}

// rowsToFrames renders result rows as frames keyed by the result schema's
// slots, so later statements' refs remap them.
func rowsToFrames(out lir.RowType, rows []lir.Row) []Frame {
	frames := make([]Frame, len(rows))
	for i, row := range rows {
		f := newFrame(bound.Env{})
		for _, fld := range out.Fields {
			f.SetScalar(fld.Slot, row[fld.Name])
		}
		frames[i] = f
	}
	return frames
}

// runPlan commits the plan's local bindings and drains its root, returning
// every result frame (keyed by the root's output slots).
func (ex *executor) runPlan(ctx context.Context, plan *planner.PhysPlan) ([]Frame, error) {
	if err := ex.commit(ctx, plan.Bindings); err != nil {
		return nil, err
	}
	op, err := ex.build(ctx, plan.Root, bound.Env{})
	if err != nil {
		return nil, err
	}
	defer op.Close()
	return drainOp(ctx, op)
}

// shapeFrames materialises drained frames into a datum per the root
// cardinality, mirroring Execute's switch over a materialised slice.
func shapeFrames(card lir.RootCard, out lir.RowType, frames []Frame) (lir.Datum, error) {
	switch card {
	case lir.CardFirst:
		if len(frames) == 0 {
			return lir.NullDatum(), nil
		}
		return frameToObject(out, frames[0]), nil
	case lir.CardExactlyOne:
		if len(frames) == 0 {
			return lir.Datum{}, reject.Runtimef("exec: expected exactly one row, got none")
		}
		if len(frames) > 1 {
			return lir.Datum{}, reject.Runtimef("exec: expected exactly one row, got more")
		}
		return frameToObject(out, frames[0]), nil
	case lir.CardScalar:
		if len(frames) == 0 {
			return lir.NullDatum(), nil
		}
		if d, has := frames[0][out.Fields[0].Slot]; has {
			return d, nil
		}
		return lir.NullDatum(), nil
	default: // many
		elems := make([]lir.Datum, len(frames))
		for i, f := range frames {
			elems[i] = frameToObject(out, f)
		}
		return lir.ArrayDatum(elems), nil
	}
}
