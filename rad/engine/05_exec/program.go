package exec

// Program execution: the effectful layer above the relation-graph read path.
// A program is an ordered list of statements run as one atomic unit. Every
// statement evaluates against the transaction's snapshot plus the effects of
// all preceding statements — the statement snapshot invariant, applied
// per-statement inside one transaction — and exposes its result as a
// program binding that later statements consume through an ordinary `ref`.
//
// Catalog statements change the transaction-visible schema. Relational
// statements therefore bind and execute in document order, after every
// preceding catalog effect. Each relational statement runs in its own
// executor seeded with the accumulated program results, so local bindings
// never leak across statement boundaries.

import (
	"context"
	"fmt"
	"maps"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/change"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
	"github.com/Southclaws/rad/rad/engine/04_planner/bind"
	"github.com/Southclaws/rad/rad/engine/04_planner/explain"
	"github.com/Southclaws/rad/rad/engine/05_exec/mutate"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
	"github.com/Southclaws/rad/rad/engine/05_exec/query"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// ExecuteProgram runs a program as one atomic transaction: all effects commit
// together or none do. A commit-time conflict is retryable via IsConflict.
func (e *Engine) ExecuteProgram(ctx context.Context, prog execprogram.Program, opts execprogram.Options) (execprogram.Result, error) {
	resultName, err := resultStatement(prog)
	if err != nil {
		return execprogram.Result{}, err
	}
	if err := validateCatalogPolicy(prog, opts.Catalog); err != nil {
		return execprogram.Result{}, err
	}
	plans, err := e.preflightProgram(ctx, prog, opts.CollectPlan, opts.ExpectedCatalog)
	if err != nil {
		return execprogram.Result{Plans: plans}, err
	}
	if opts.DryRun {
		return execprogram.Result{Plans: plans}, nil
	}
	tx, err := e.Begin(ctx)
	if err != nil {
		return execprogram.Result{}, err
	}
	defer tx.Rollback()
	runOpts := opts
	runOpts.DryRun = false
	runOpts.CollectPlan = false
	out, err := e.runProgram(ctx, tx, prog, resultName, runOpts)
	out.Plans = plans
	if err != nil {
		return out, err
	}
	if err := tx.Commit(ctx); err != nil {
		return out, err
	}
	return out, nil
}

// ExecuteProgram runs one PIR program inside an already-open transaction.
// It does not commit: preceding programs' writes remain visible, and the
// caller decides whether the complete transaction commits or rolls back.
//
// Unlike Engine.ExecuteProgram, this path cannot preflight in a separate
// rollback-only transaction without losing the caller's uncommitted catalog
// and data view. A program error therefore requires the owner to roll back the
// enclosing transaction before reusing it. SQL drivers enforce that rule as
// PostgreSQL's failed-transaction state.
func (tx *Tx) ExecuteProgram(ctx context.Context, prog execprogram.Program, opts execprogram.Options) (execprogram.Result, error) {
	if opts.DryRun {
		return execprogram.Result{}, reject.Inputf("exec: dry-run is not supported inside an explicit transaction")
	}
	resultName, err := resultStatement(prog)
	if err != nil {
		return execprogram.Result{}, err
	}
	if err := validateCatalogPolicy(prog, opts.Catalog); err != nil {
		return execprogram.Result{}, err
	}
	return tx.e.runProgram(ctx, tx, prog, resultName, opts)
}

func validateCatalogPolicy(prog execprogram.Program, policy execprogram.CatalogPolicy) error {
	switch policy {
	case execprogram.CatalogForbidden:
		for _, stmt := range prog.Statements {
			if stmt.Kind.Catalog() {
				return reject.Inputf("exec: catalog statement %q is not authorised", stmt.Name)
			}
		}
		return nil
	case execprogram.CatalogRevisionPerStatement, execprogram.CatalogRevisionPerProgram:
		return nil
	default:
		return reject.Inputf("exec: unknown catalog policy %d", policy)
	}
}

// preflightProgram validates catalog transitions and binds every relational
// statement in order against a rollback-only transaction. It deliberately
// skips data execution and index backfills: checks depending on stored rows
// belong to the real execution pass.
func (e *Engine) preflightProgram(ctx context.Context, prog execprogram.Program, collectPlan bool, expected *model.Revision) ([]execprogram.StatementPlan, error) {
	tx, err := e.Begin(ctx)
	if err != nil {
		return nil, err
	}
	defer tx.Rollback()
	if err := expectCatalog(ctx, tx.txn, expected); err != nil {
		return nil, err
	}

	relNames := make([]string, 0, len(prog.Statements))
	for _, stmt := range prog.Statements {
		if !stmt.Kind.Catalog() {
			relNames = append(relNames, stmt.Name)
		}
	}
	binder, err := bind.NewProgramBinder(ctx, store.New(tx.txn), relNames)
	if err != nil {
		return nil, err
	}
	var plans []execprogram.StatementPlan
	_, err = change.Apply(ctx, tx.txn, func(change *change.Mutation) error {
		for _, stmt := range prog.Statements {
			if stmt.Kind.Catalog() {
				if err := tx.preflightCatalogStatement(ctx, change, stmt); err != nil {
					return fmt.Errorf("exec: statement %q: %w", stmt.Name, err)
				}
				continue
			}
			bound, err := binder.Bind(bind.ProgramStmt{
				Name: stmt.Name, Rel: stmt.Rel,
				Mutation: stmt.Kind != execprogram.Query, Table: stmt.Table,
			})
			if err != nil {
				return err
			}
			if collectPlan {
				plans = append(plans, execprogram.StatementPlan{Name: bound.Name, View: explain.NewPlanView(bound.Plan)})
			}
		}
		return nil
	})
	return plans, err
}

// resultStatement resolves which statement's result is returned: the explicit
// selector, or the sole statement of a one-statement program.
func resultStatement(prog execprogram.Program) (string, error) {
	if len(prog.Statements) == 0 {
		return "", reject.Inputf("exec: a program needs at least one statement")
	}
	names := make(map[string]bool, len(prog.Statements))
	for _, stmt := range prog.Statements {
		if stmt.Name == "" {
			return "", reject.Inputf("exec: statement name must not be empty")
		}
		if names[stmt.Name] {
			return "", reject.Inputf("exec: duplicate statement name %q", stmt.Name)
		}
		names[stmt.Name] = true
		if !stmt.Kind.Valid() {
			return "", reject.Inputf("exec: unknown statement kind %q", stmt.Kind)
		}
	}
	if prog.Result != "" {
		for _, stmt := range prog.Statements {
			if stmt.Name != prog.Result {
				continue
			}
			if stmt.Kind.Catalog() {
				return "", reject.Inputf("exec: result names catalog statement %q", prog.Result)
			}
			return prog.Result, nil
		}
		return "", reject.Inputf("exec: result names unknown statement %q", prog.Result)
	}
	if len(prog.Statements) == 1 && !prog.Statements[0].Kind.Catalog() {
		return prog.Statements[0].Name, nil
	}
	allCatalog := true
	for _, stmt := range prog.Statements {
		allCatalog = allCatalog && stmt.Kind.Catalog()
	}
	if allCatalog {
		return "", nil
	}
	return "", reject.Inputf("exec: a program with %d statements must name its result", len(prog.Statements))
}

func (e *Engine) runProgram(ctx context.Context, tx *Tx, prog execprogram.Program, resultName string, opts execprogram.Options) (execprogram.Result, error) {
	if err := expectCatalog(ctx, tx.txn, opts.ExpectedCatalog); err != nil {
		return execprogram.Result{}, err
	}
	relNames := make([]string, 0, len(prog.Statements))
	for _, stmt := range prog.Statements {
		if !stmt.Kind.Catalog() {
			relNames = append(relNames, stmt.Name)
		}
	}
	binder, err := bind.NewProgramBinder(ctx, store.New(tx.txn), relNames)
	if err != nil {
		return execprogram.Result{}, err
	}

	var plans []execprogram.StatementPlan
	program := map[string][]lireval.Env{}
	summary := make([]execprogram.StatementResult, len(prog.Statements))
	var result lir.Datum
	haveResult := false

	run := func(activeChange *change.Mutation) error {
		for i, stmt := range prog.Statements {
			if stmt.Kind.Catalog() {
				apply := func(active *change.Mutation) error {
					return tx.runCatalogStatement(ctx, active, stmt)
				}
				var err error
				if activeChange != nil {
					err = apply(activeChange)
				} else {
					_, err = change.Apply(ctx, tx.txn, apply)
				}
				if err != nil {
					return fmt.Errorf("exec: statement %q: %w", stmt.Name, err)
				}
				if !opts.DryRun {
					summary[i] = execprogram.StatementResult{Name: stmt.Name, Affected: 1}
				}
				continue
			}

			bs, err := binder.Bind(bind.ProgramStmt{
				Name: stmt.Name, Rel: stmt.Rel,
				Mutation: stmt.Kind != execprogram.Query, Table: stmt.Table,
			})
			if err != nil {
				return err
			}
			if opts.CollectPlan {
				plans = append(plans, execprogram.StatementPlan{Name: bs.Name, View: explain.NewPlanView(bs.Plan)})
			}
			if opts.DryRun {
				continue
			}
			frames, err := e.runStatement(ctx, tx.txn, stmt, bs, program)
			if err != nil {
				return err
			}
			program[bs.Name] = frames
			summary[i] = execprogram.StatementResult{Name: bs.Name, Affected: len(frames)}
			if bs.Name == resultName {
				d, err := shapeFrames(bs.ResultCard, bs.ResultOut, frames)
				if err != nil {
					return err
				}
				result, haveResult = d, true
			}
		}
		return nil
	}

	switch opts.Catalog {
	case execprogram.CatalogRevisionPerProgram:
		_, err = change.Apply(ctx, tx.txn, run)
	case execprogram.CatalogForbidden, execprogram.CatalogRevisionPerStatement:
		err = run(nil)
	default:
		err = reject.Inputf("exec: unknown catalog policy %d", opts.Catalog)
	}
	if err != nil {
		return execprogram.Result{Plans: plans}, err
	}
	if opts.DryRun {
		return execprogram.Result{Plans: plans}, nil
	}
	if resultName == "" {
		return execprogram.Result{Statements: summary, Plans: plans}, nil
	}
	if !haveResult {
		return execprogram.Result{Plans: plans}, reject.Inputf("exec: result names unknown statement %q", resultName)
	}
	return execprogram.Result{Result: result, Statements: summary, Plans: plans}, nil
}

func expectCatalog(ctx context.Context, view kv.KV, expected *model.Revision) error {
	if expected == nil {
		return nil
	}
	reader := store.New(view)
	revision, err := reader.Revision(ctx)
	if err != nil {
		return err
	}
	if revision.Version != expected.Version || revision.Hash != expected.Hash {
		return fmt.Errorf("exec: catalog changed since program planning: %w", kv.ErrConflict)
	}
	actual, err := reader.Schema(ctx)
	if err != nil {
		return err
	}
	equal, err := expected.Schema.Equal(actual)
	if err != nil {
		return err
	}
	if !equal {
		return fmt.Errorf("exec: catalog changed since program planning: %w", kv.ErrConflict)
	}
	return nil
}

// runStatement evaluates one statement against the transaction view, with the
// accumulated program results available as bindings, and returns its result
// frames keyed by the statement's result schema. A query returns the
// relation's rows; a mutation evaluates its input relation, applies it as one
// set, and returns the affected rows (created, post-image, or pre-image).
func (e *Engine) runStatement(ctx context.Context, view kv.KV, stmt execprogram.Statement, bs bind.BoundStatement, program map[string][]lireval.Env) ([]lireval.Env, error) {
	ex := query.New(view, query.Limits{MaxIterations: e.recur.MaxIterations, MaxRows: e.recur.MaxRows})
	seeded := map[string][]lireval.Env{}
	maps.Copy(seeded, program)
	ex.SeedBindings(seeded)
	if stmt.Kind == execprogram.Query {
		return ex.RunFrames(ctx, bs.Plan)
	}

	// Evaluate the input relation fully before touching storage — the
	// statement snapshot: the mutation cannot observe its own writes.
	inputFrames, err := ex.RunFrames(ctx, bs.Plan)
	if err != nil {
		return nil, err
	}
	inputRows, err := framesToRows(bs.Plan.Out, inputFrames)
	if err != nil {
		return nil, err
	}

	tbl, err := tableIn(ctx, view, stmt.Table)
	if err != nil {
		return nil, err
	}
	var affected []lir.Row
	switch stmt.Kind {
	case execprogram.Create:
		affected, err = mutate.Create(ctx, view, tbl, inputRows)
	case execprogram.Update:
		affected, err = mutate.Update(ctx, view, tbl, bs.Plan.Out, inputRows)
	case execprogram.Delete:
		affected, err = mutate.Delete(ctx, view, tbl, bs.Plan.Out, inputRows)
	default:
		return nil, reject.Inputf("exec: unknown statement kind %q", stmt.Kind)
	}
	if err != nil {
		return nil, err
	}
	return rowsToFrames(bs.ResultOut, affected), nil
}

func framesToRows(out lir.RowType, frames []lireval.Env) ([]lir.Row, error) {
	rows := make([]lir.Row, len(frames))
	for i, f := range frames {
		row := make(lir.Row, len(out.Fields))
		for _, fld := range out.Fields {
			v, err := f.ScalarAt(fld.Slot, fld.Name, fld.Type)
			if err != nil {
				return nil, err
			}
			row[fld.Name] = v
		}
		rows[i] = row
	}
	return rows, nil
}

// rowsToFrames renders result rows as frames keyed by the result schema's
// slots, so later statements' refs remap them.
func rowsToFrames(out lir.RowType, rows []lir.Row) []lireval.Env {
	frames := make([]lireval.Env, len(rows))
	for i, row := range rows {
		f := lireval.Env{}
		for _, fld := range out.Fields {
			f.SetScalar(fld.Slot, row[fld.Name])
		}
		frames[i] = f
	}
	return frames
}

// shapeFrames materialises frames according to the root cardinality.
func shapeFrames(card lir.RootCard, out lir.RowType, frames []lireval.Env) (lir.Datum, error) {
	return query.ShapeFrames(card, out, frames)
}
