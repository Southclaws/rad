package api

// The /execute endpoint: run a PIR program. The raw body is validated and
// decoded by protocol.UnmarshalProgram (envelope plus every relational
// statement's LIR document), lowered into the engine's relational or catalog
// statement types, and run as one atomic transaction. A decode/validation
// failure is a 400; a bind or execution failure is a 422 (or 409 for a
// serializable race), classified by the engine's typed errors.

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"

	"github.com/Southclaws/rad/rad/api"
	"github.com/Southclaws/rad/rad/api/oas"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
	"github.com/Southclaws/rad/rad/engine/06_frontend/resultjson"
	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

func (a *dbAPI) Execute(ctx context.Context, req oas.Program, params oas.ExecuteParams) (oas.ExecuteRes, error) {
	prog, err := protocol.UnmarshalProgram(req)
	if err != nil {
		op := api.ProblemToOAS(protocol.NewProblem(protocol.CodeInvalid, http.StatusBadRequest, err.Error()))
		return (*oas.ExecuteBadRequest)(&op), nil
	}

	// show-plan and dry-run are orthogonal transport knobs (not part of the
	// IR): show-plan attaches the per-statement query plan; dry-run binds and
	// plans but executes nothing.
	showPlan := params.ShowPlan.Or(false)
	dryRun := params.DryRun.Or(false)
	opts := execprogram.Options{
		DryRun: dryRun, CollectPlan: showPlan,
		Catalog: a.executeCatalogPolicy(),
	}

	ep, err := programToEngine(prog)
	if err == nil {
		var res execprogram.Result
		if res, err = a.db.ExecuteProgram(ctx, ep, opts); err == nil {
			return programResult(res, showPlan, dryRun)
		}
	}
	// A failure after planning still produced plans (kept on the result), but
	// surfacing them on the error Problem is deferred to the error-propagation
	// work, which attaches them per problem class.
	if p := clientProblem(err); p != nil {
		op := api.ProblemToOAS(*p)
		if p.Code == protocol.CodeConflict {
			return (*oas.ExecuteConflict)(&op), nil
		}
		return (*oas.ExecuteUnprocessableEntity)(&op), nil
	}
	return nil, err
}

// executeCatalogPolicy is the deliberately narrow seam for the still-open
// product decision about which ordinary HTTP callers receive catalog
// authority. PIR itself carries no authority. For now this preserves the
// existing mode behaviour while the execution mechanism settles.
func (a *dbAPI) executeCatalogPolicy() execprogram.CatalogPolicy {
	if a.mode == model.ModeDirect {
		return execprogram.CatalogRevisionPerStatement
	}
	return execprogram.CatalogForbidden
}

// programToEngine lowers each generated wire variant into the engine's PIR
// statement. Relational payloads are decoded from their opaque LIR bytes;
// catalog definitions map into the canonical catalog types. Backward-only
// references, namespace collisions, and result selection are enforced by the
// binder.
func programToEngine(p pirwire.Program) (execprogram.Program, error) {
	stmts := make([]execprogram.Statement, len(p.Statements))
	for i, s := range p.Statements {
		stmt, err := statementToEngine(s)
		if err != nil {
			return execprogram.Program{}, err
		}
		stmts[i] = stmt
	}
	return execprogram.Program{Statements: stmts, Result: optString(p.Result)}, nil
}

func statementToEngine(s pirwire.Statement) (execprogram.Statement, error) {
	switch x := s.StatementUnion.(type) {
	case *pirwire.QueryStatement:
		return relationalStatement(x.Name, execprogram.Query, "", x.Relation)
	case *pirwire.CreateStatement:
		return relationalStatement(x.Name, execprogram.Create, x.Table, x.Relation)
	case *pirwire.UpdateStatement:
		return relationalStatement(x.Name, execprogram.Update, x.Table, x.Relation)
	case *pirwire.DeleteStatement:
		return relationalStatement(x.Name, execprogram.Delete, x.Table, x.Relation)
	case *pirwire.CreateTableStatement:
		def, err := pirTableDef(x.Table)
		if err != nil {
			return execprogram.Statement{}, fmt.Errorf("statement %q: %w", x.Name, err)
		}
		return execprogram.Statement{Name: x.Name, Kind: execprogram.CreateTable, TableDef: def}, nil
	case *pirwire.RenameTableStatement:
		return execprogram.Statement{Name: x.Name, Kind: execprogram.RenameTable, TableID: schemaID(x.TableID), To: x.To}, nil
	case *pirwire.DeleteTableStatement:
		return execprogram.Statement{Name: x.Name, Kind: execprogram.DeleteTable, TableID: schemaID(x.TableID)}, nil
	case *pirwire.CreateColumnStatement:
		def, err := pirColumnDef(x.Column)
		if err != nil {
			return execprogram.Statement{}, fmt.Errorf("statement %q: %w", x.Name, err)
		}
		return execprogram.Statement{Name: x.Name, Kind: execprogram.CreateColumn, TableID: schemaID(x.TableID), Column: def}, nil
	case *pirwire.RenameColumnStatement:
		return execprogram.Statement{Name: x.Name, Kind: execprogram.RenameColumn, TableID: schemaID(x.TableID), ColumnID: schemaID(x.ColumnID), To: x.To}, nil
	case *pirwire.DeleteColumnStatement:
		return execprogram.Statement{Name: x.Name, Kind: execprogram.DeleteColumn, TableID: schemaID(x.TableID), ColumnID: schemaID(x.ColumnID)}, nil
	case *pirwire.CreateIndexStatement:
		return execprogram.Statement{Name: x.Name, Kind: execprogram.CreateIndex, TableID: schemaID(x.TableID), Index: pirIndexDef(x.Index)}, nil
	case *pirwire.DeleteIndexStatement:
		return execprogram.Statement{Name: x.Name, Kind: execprogram.DeleteIndex, TableID: schemaID(x.TableID), IndexName: x.Index}, nil
	default:
		return execprogram.Statement{}, wireErrf("unknown PIR statement variant %T", s.StatementUnion)
	}
}

func relationalStatement(name string, kind execprogram.Kind, table string, raw []byte) (execprogram.Statement, error) {
	var wire lirwire.Query
	if err := json.Unmarshal(raw, &wire); err != nil {
		return execprogram.Statement{}, fmt.Errorf("statement %q: decode relation: %w", name, err)
	}
	relation, err := lowerQuery(wire)
	if err != nil {
		return execprogram.Statement{}, fmt.Errorf("statement %q: %w", name, err)
	}
	return execprogram.Statement{Name: name, Kind: kind, Table: table, Rel: relation}, nil
}

// programResult shapes a program outcome into the wire response: the result
// datum as raw JSON, the per-statement summary, and — when show-plan was set —
// the per-statement query plan as free-form JSON under `plan`. The `plan` field
// is always emitted valid (JSON null when absent), since the response type
// carries it unconditionally.
func programResult(res execprogram.Result, showPlan, dryRun bool) (oas.ExecuteRes, error) {
	result := []byte("null") // dry-run executes nothing, so there is no result
	if !dryRun {
		raw, err := json.Marshal(resultjson.Datum(res.Result))
		if err != nil {
			return nil, fmt.Errorf("encode result datum: %w", err)
		}
		result = raw
	}
	stmts := make([]oas.StatementResult, len(res.Statements))
	for i, s := range res.Statements {
		stmts[i] = oas.StatementResult{Name: s.Name, Affected: s.Affected}
	}

	plan := []byte("null")
	if showPlan {
		raw, err := json.Marshal(planEnvelope(res.Plans))
		if err != nil {
			return nil, fmt.Errorf("encode query plan: %w", err)
		}
		plan = raw
	}
	return &oas.ProgramResult{Result: oas.Value(result), Statements: stmts, Plan: oas.Value(plan)}, nil
}

// planEnvelope renders the per-statement query plans as free-form JSON: each
// statement's structured PlanView plus a rendered text form. Its shape is
// transport metadata, deliberately not part of the OpenAPI or IR contract.
func planEnvelope(plans []execprogram.StatementPlan) map[string]any {
	stmts := make([]map[string]any, len(plans))
	for i, p := range plans {
		stmts[i] = map[string]any{"name": p.Name, "view": p.View, "text": p.View.String()}
	}
	return map[string]any{"statements": stmts}
}
