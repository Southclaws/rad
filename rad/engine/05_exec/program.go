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

	StmtCreateTable  StatementKind = "create_table"
	StmtRenameTable  StatementKind = "rename_table"
	StmtDeleteTable  StatementKind = "delete_table"
	StmtCreateColumn StatementKind = "create_column"
	StmtRenameColumn StatementKind = "rename_column"
	StmtDeleteColumn StatementKind = "delete_column"
	StmtCreateIndex  StatementKind = "create_index"
	StmtDeleteIndex  StatementKind = "delete_index"
)

// ProgramStatement is one lowered PIR statement. Relational kinds use Table
// and Rel; catalog kinds use the stable identities and canonical catalog
// definitions relevant to their operation.
type ProgramStatement struct {
	Name string
	Kind StatementKind

	Table string
	Rel   lir.Query

	TableID   catalog.SchemaID
	ColumnID  catalog.SchemaID
	To        string
	TableDef  catalog.TableDef
	Column    catalog.ColumnDef
	Index     catalog.IndexDef
	IndexName string
}

// Program is an ordered list of statements plus the name of the relational
// statement whose result is returned. Catalog-only programs omit Result and
// return a null datum.
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

// ExecOptions tune a program run's observability and side effects. The zero
// value permits relational statements and forbids catalog statements.
type ExecOptions struct {
	// DryRun binds and plans every statement but executes none.
	DryRun bool
	// CollectPlan preserves each statement's plan even if execution fails.
	CollectPlan bool
	// Catalog selects whether catalog statements are authorised and how their
	// successful changes become schema revisions.
	Catalog CatalogPolicy
	// ExpectedCatalog, when set by an internal reconciler, prevents a program
	// planned from a stale catalog snapshot from committing over newer schema.
	ExpectedCatalog *catalog.Schema
}

// StatementPlan is one statement's query-plan view — the observability artifact
// that rides the transport response, never the IR.
type StatementPlan struct {
	Name string
	View *planner.PlanView
}

// ProgramResult is a program's outcome: the declared result statement's datum,
// a per-statement summary, and (when requested) the per-statement plan views.
type ProgramResult struct {
	Result     lir.Datum
	Statements []StatementResult
	Plans      []StatementPlan
}

// ExecuteProgram runs a program as one atomic transaction: all effects commit
// together or none do. A commit-time conflict is retryable via IsConflict.
func (e *Engine) ExecuteProgram(ctx context.Context, prog Program, opts ExecOptions) (ProgramResult, error) {
	resultName, err := resultStatement(prog)
	if err != nil {
		return ProgramResult{}, err
	}
	if err := validateCatalogPolicy(prog, opts.Catalog); err != nil {
		return ProgramResult{}, err
	}
	plans, err := e.preflightProgram(ctx, prog, opts.CollectPlan, opts.ExpectedCatalog)
	if err != nil {
		return ProgramResult{Plans: plans}, err
	}
	if opts.DryRun {
		return ProgramResult{Plans: plans}, nil
	}
	tx, err := e.Begin(ctx)
	if err != nil {
		return ProgramResult{}, err
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

func validateCatalogPolicy(prog Program, policy CatalogPolicy) error {
	switch policy {
	case CatalogForbidden:
		for _, stmt := range prog.Statements {
			if stmt.Kind.catalog() {
				return reject.Inputf("exec: catalog statement %q is not authorised", stmt.Name)
			}
		}
		return nil
	case CatalogRevisionPerStatement, CatalogRevisionPerProgram:
		return nil
	default:
		return reject.Inputf("exec: unknown catalog policy %d", policy)
	}
}

// preflightProgram validates catalog transitions and binds every relational
// statement in order against a rollback-only transaction. It deliberately
// skips data execution and index backfills: checks depending on stored rows
// belong to the real execution pass.
func (e *Engine) preflightProgram(ctx context.Context, prog Program, collectPlan bool, expected *catalog.Schema) ([]StatementPlan, error) {
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
		if !stmt.Kind.catalog() {
			relNames = append(relNames, stmt.Name)
		}
	}
	binder, err := planner.NewProgramBinder(ctx, catalog.NewReader(tx.txn), relNames)
	if err != nil {
		return nil, err
	}
	var plans []StatementPlan
	_, err = catalog.MutateIn(ctx, tx.txn, func(change *catalog.Mutation) error {
		for _, stmt := range prog.Statements {
			if stmt.Kind.catalog() {
				if err := tx.preflightCatalogStatement(ctx, change, stmt); err != nil {
					return fmt.Errorf("exec: statement %q: %w", stmt.Name, err)
				}
				continue
			}
			bound, err := binder.Bind(planner.ProgramStmt{
				Name: stmt.Name, Rel: stmt.Rel,
				Mutation: stmt.Kind != StmtQuery, Table: stmt.Table,
			})
			if err != nil {
				return err
			}
			if collectPlan {
				plans = append(plans, StatementPlan{Name: bound.Name, View: planner.NewPlanView(bound.Plan)})
			}
		}
		return nil
	})
	return plans, err
}

// resultStatement resolves which statement's result is returned: the explicit
// selector, or the sole statement of a one-statement program.
func resultStatement(prog Program) (string, error) {
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
		if !stmt.Kind.valid() {
			return "", reject.Inputf("exec: unknown statement kind %q", stmt.Kind)
		}
	}
	if prog.Result != "" {
		for _, stmt := range prog.Statements {
			if stmt.Name != prog.Result {
				continue
			}
			if stmt.Kind.catalog() {
				return "", reject.Inputf("exec: result names catalog statement %q", prog.Result)
			}
			return prog.Result, nil
		}
		return "", reject.Inputf("exec: result names unknown statement %q", prog.Result)
	}
	if len(prog.Statements) == 1 && !prog.Statements[0].Kind.catalog() {
		return prog.Statements[0].Name, nil
	}
	allCatalog := true
	for _, stmt := range prog.Statements {
		allCatalog = allCatalog && stmt.Kind.catalog()
	}
	if allCatalog {
		return "", nil
	}
	return "", reject.Inputf("exec: a program with %d statements must name its result", len(prog.Statements))
}

func (e *Engine) runProgram(ctx context.Context, tx *Tx, prog Program, resultName string, opts ExecOptions) (ProgramResult, error) {
	if err := expectCatalog(ctx, tx.txn, opts.ExpectedCatalog); err != nil {
		return ProgramResult{}, err
	}
	relNames := make([]string, 0, len(prog.Statements))
	for _, stmt := range prog.Statements {
		if !stmt.Kind.catalog() {
			relNames = append(relNames, stmt.Name)
		}
	}
	binder, err := planner.NewProgramBinder(ctx, catalog.NewReader(tx.txn), relNames)
	if err != nil {
		return ProgramResult{}, err
	}

	var plans []StatementPlan
	program := map[string][]frame{}
	summary := make([]StatementResult, len(prog.Statements))
	var result lir.Datum
	haveResult := false

	run := func(change *catalog.Mutation) error {
		for i, stmt := range prog.Statements {
			if stmt.Kind.catalog() {
				apply := func(active *catalog.Mutation) error {
					return tx.runCatalogStatement(ctx, active, stmt)
				}
				var err error
				if change != nil {
					err = apply(change)
				} else {
					_, err = catalog.MutateIn(ctx, tx.txn, apply)
				}
				if err != nil {
					return fmt.Errorf("exec: statement %q: %w", stmt.Name, err)
				}
				if !opts.DryRun {
					summary[i] = StatementResult{Name: stmt.Name, Affected: 1}
				}
				continue
			}

			bs, err := binder.Bind(planner.ProgramStmt{
				Name: stmt.Name, Rel: stmt.Rel,
				Mutation: stmt.Kind != StmtQuery, Table: stmt.Table,
			})
			if err != nil {
				return err
			}
			if opts.CollectPlan {
				plans = append(plans, StatementPlan{Name: bs.Name, View: planner.NewPlanView(bs.Plan)})
			}
			if opts.DryRun {
				continue
			}
			frames, err := e.runStatement(ctx, tx.txn, stmt, bs, program)
			if err != nil {
				return err
			}
			program[bs.Name] = frames
			summary[i] = StatementResult{Name: bs.Name, Affected: len(frames)}
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
	case CatalogRevisionPerProgram:
		_, err = catalog.MutateIn(ctx, tx.txn, run)
	case CatalogForbidden, CatalogRevisionPerStatement:
		err = run(nil)
	default:
		err = reject.Inputf("exec: unknown catalog policy %d", opts.Catalog)
	}
	if err != nil {
		return ProgramResult{Plans: plans}, err
	}
	if opts.DryRun {
		return ProgramResult{Plans: plans}, nil
	}
	if resultName == "" {
		return ProgramResult{Statements: summary, Plans: plans}, nil
	}
	if !haveResult {
		return ProgramResult{Plans: plans}, reject.Inputf("exec: result names unknown statement %q", resultName)
	}
	return ProgramResult{Result: result, Statements: summary, Plans: plans}, nil
}

func expectCatalog(ctx context.Context, view kv.KV, expected *catalog.Schema) error {
	if expected == nil {
		return nil
	}
	actual, err := catalog.NewReader(view).Schema(ctx)
	if err != nil {
		return err
	}
	equal, err := expected.Equal(actual)
	if err != nil {
		return err
	}
	if !equal {
		return fmt.Errorf("exec: catalog changed since program planning: %w", kv.ErrConflict)
	}
	return nil
}

func (k StatementKind) catalog() bool {
	switch k {
	case StmtCreateTable, StmtRenameTable, StmtDeleteTable,
		StmtCreateColumn, StmtRenameColumn, StmtDeleteColumn,
		StmtCreateIndex, StmtDeleteIndex:
		return true
	default:
		return false
	}
}

func (k StatementKind) valid() bool {
	switch k {
	case StmtQuery, StmtCreate, StmtUpdate, StmtDelete:
		return true
	default:
		return k.catalog()
	}
}

// runStatement evaluates one statement against the transaction view, with the
// accumulated program results available as bindings, and returns its result
// frames keyed by the statement's result schema. A query returns the
// relation's rows; a mutation evaluates its input relation, applies it as one
// set, and returns the affected rows (created, post-image, or pre-image).
func (e *Engine) runStatement(ctx context.Context, view kv.KV, stmt ProgramStatement, bs planner.BoundStatement, program map[string][]frame) ([]frame, error) {
	ex := newExecutor(view, e.recur)
	maps.Copy(ex.bindings, program)
	if stmt.Kind == StmtQuery {
		return ex.runPlan(ctx, bs.Plan)
	}

	// Evaluate the input relation fully before touching storage — the
	// statement snapshot: the mutation cannot observe its own writes.
	inputFrames, err := ex.runPlan(ctx, bs.Plan)
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
	case StmtCreate:
		affected, err = applyCreate(ctx, view, tbl, inputRows)
	case StmtUpdate:
		affected, err = applyUpdate(ctx, view, tbl, bs.Plan.Out, inputRows)
	case StmtDelete:
		affected, err = applyDelete(ctx, view, tbl, bs.Plan.Out, inputRows)
	default:
		return nil, reject.Inputf("exec: unknown statement kind %q", stmt.Kind)
	}
	if err != nil {
		return nil, err
	}
	return rowsToFrames(bs.ResultOut, affected), nil
}

func framesToRows(out lir.RowType, frames []frame) ([]lir.Row, error) {
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
func rowsToFrames(out lir.RowType, rows []lir.Row) []frame {
	frames := make([]frame, len(rows))
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
func (ex *executor) runPlan(ctx context.Context, plan *planner.PhysPlan) ([]frame, error) {
	if err := ex.commit(ctx, plan.Bindings); err != nil {
		return nil, err
	}
	op, err := ex.build(ctx, plan.Root, bound.Env{})
	if err != nil {
		return nil, err
	}
	return drainAndClose(ctx, op)
}

// shapeFrames materialises frames according to the root cardinality.
func shapeFrames(card lir.RootCard, out lir.RowType, frames []frame) (lir.Datum, error) {
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
		return frameScalar(out, frames[0]), nil
	default: // many
		return framesToArray(out, frames), nil
	}
}
