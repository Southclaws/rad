package api

import (
	"context"
	"encoding/json"
	"fmt"
	"math"
	"net/http"

	"github.com/Southclaws/rad/rad/api"
	"github.com/Southclaws/rad/rad/api/oas"
	"github.com/Southclaws/rad/rad/engine/02_catalog/migrate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
	"github.com/Southclaws/rad/rad/engine/06_frontend/migration"
	"github.com/Southclaws/rad/rad/engine/reject"
	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

func (a *dbAPI) GetSchema(ctx context.Context) (*oas.SchemaState, error) {
	revision, err := a.cat.Revision(ctx)
	if err != nil {
		return nil, err
	}
	version, err := wireSchemaVersion(revision.Version)
	if err != nil {
		return nil, err
	}
	return &oas.SchemaState{
		SchemaVersion: version, SchemaHash: revision.Hash, Schema: schemaDocument(revision.Schema),
	}, nil
}

func (a *dbAPI) SchemaDiff(ctx context.Context, req oas.OptSchemaRequest) (oas.SchemaDiffRes, error) {
	plan, err := a.db.PlanMigrationFile(ctx, "rad.schema.yaml", []byte(req.Or(oas.SchemaRequest{}).Schema))
	if err != nil {
		return schemaDiffProblem(err)
	}
	program, err := migrationProgramWire(plan.Program)
	if err != nil {
		return nil, err
	}
	var raw []byte
	if len(plan.Program.Statements) == 0 {
		raw = []byte(`{"statements":[]}`)
	} else {
		raw, err = protocol.MarshalProgram(program)
		if err != nil {
			return nil, err
		}
	}
	version, err := wireSchemaVersion(plan.Current.Version)
	if err != nil {
		return nil, err
	}
	return &oas.SchemaDiffResult{
		CurrentVersion: version,
		CurrentHash:    plan.Current.Hash,
		DesiredHash:    plan.DesiredHash,
		Changes:        schemaChanges(plan.Steps),
		Program:        oas.Value(raw),
		Destructive:    schemaFindings(plan.Destructive),
		Blocking:       schemaFindings(plan.Blocking),
	}, nil
}

func schemaDiffProblem(err error) (oas.SchemaDiffRes, error) {
	if problem := clientProblem(err); problem != nil {
		out := api.ProblemToOAS(*problem)
		return &out, nil
	}
	return nil, err
}

func (a *dbAPI) SchemaMigrate(ctx context.Context, req oas.OptSchemaMigrateRequest) (oas.SchemaMigrateRes, error) {
	request := req.Or(oas.SchemaMigrateRequest{})
	var plan migration.MigrationPlan
	plan, err := a.db.PlanMigrationFile(ctx, "rad.schema.yaml", []byte(request.Schema))
	identityMatches := err == nil && plan.Current.Version == uint64(request.CurrentVersion) &&
		plan.Current.Hash == request.CurrentHash
	recoversCommittedRequest := err == nil && len(plan.Blocking) == 0 && len(plan.Program.Statements) == 0 &&
		(plan.Current.Hash == plan.DesiredHash || len(plan.Transitions) > 0)
	if err == nil && !identityMatches && !recoversCommittedRequest {
		err = reject.Fail(reject.ReasonSerializableConflict,
			"schema changed since preflight: expected version %d (%s), found version %d (%s)",
			request.CurrentVersion, request.CurrentHash, plan.Current.Version, plan.Current.Hash)
	}
	var result migration.MigrationResult
	if err == nil {
		result, err = a.db.ApplyMigrationPlan(ctx, plan, request.AcceptDataLoss.Or(false))
	}
	if err != nil {
		if problem := clientProblem(err); problem != nil {
			out := api.ProblemToOAS(*problem)
			if problem.Code == protocol.CodeConflict {
				return (*oas.SchemaMigrateConflict)(&out), nil
			}
			return (*oas.SchemaMigrateUnprocessableEntity)(&out), nil
		}
		return nil, err
	}
	version, err := wireSchemaVersion(result.Revision)
	if err != nil {
		return nil, err
	}
	return &oas.SchemaMigrateResult{
		SchemaVersion: version,
		SchemaHash:    result.Hash,
		Schema:        schemaDocument(result.Schema),
		Changes:       schemaChanges(result.Plan.Steps),
		DesiredHash:   result.Plan.DesiredHash,
		State:         oas.SchemaMigrateResultState(result.State),
		TransitionIds: result.TransitionIDs,
	}, nil
}

func (a *dbAPI) SchemaCompatibility(
	ctx context.Context,
	req oas.OptSchemaCompatibilityRequest,
) (oas.SchemaCompatibilityRes, error) {
	request := req.Or(oas.SchemaCompatibilityRequest{})
	revision, err := a.cat.Revision(ctx)
	if err != nil {
		return nil, err
	}
	clientVersion := uint64(request.SchemaVersion)
	if clientVersion < revision.Version {
		return compatibilityProblem("schema_client_outdated", fmt.Sprintf(
			"this client was generated for schema version %d, but the database is currently on version %d",
			clientVersion, revision.Version)), nil
	}
	if clientVersion > revision.Version {
		return compatibilityProblem("schema_server_outdated", fmt.Sprintf(
			"this client expects schema version %d, but the database is currently on version %d",
			clientVersion, revision.Version)), nil
	}
	if request.SchemaHash != revision.Hash {
		return compatibilityProblem("schema_history_diverged", fmt.Sprintf(
			"client and server both report schema version %d, but their schema hashes differ (client %s, server %s)",
			clientVersion, request.SchemaHash, revision.Hash)), nil
	}
	return &oas.NoContent{}, nil
}

func compatibilityProblem(reason, detail string) *oas.Problem {
	problem := protocol.NewProblem(protocol.CodeInvalid, http.StatusUnprocessableEntity, detail).WithReason(reason)
	out := api.ProblemToOAS(problem)
	return &out
}

func wireSchemaVersion(version uint64) (int64, error) {
	if version > math.MaxInt64 {
		return 0, fmt.Errorf("catalog: schema version %d exceeds the wire format", version)
	}
	return int64(version), nil
}

func schemaChanges(steps []migrate.Step) []oas.SchemaChange {
	out := make([]oas.SchemaChange, len(steps))
	for i, step := range steps {
		change := oas.SchemaChange{Kind: migrationStepKind(step), Summary: step.String()}
		switch value := step.(type) {
		case migrate.RenameTable:
			change.Table = oas.NewOptString(value.To)
		case migrate.CreateTable:
			change.Table = oas.NewOptString(value.Def.Name)
		case migrate.DeleteTable:
			change.Table = oas.NewOptString(value.Table)
		case migrate.RenameColumn:
			change.Table = oas.NewOptString(value.Table)
			change.Column = oas.NewOptString(value.To)
		case migrate.CreateColumn:
			change.Table = oas.NewOptString(value.Table)
			change.Column = oas.NewOptString(value.Def.Name)
		case migrate.ChangeColumnDefault:
			change.Table = oas.NewOptString(value.Table)
			change.Column = oas.NewOptString(value.Column)
		case migrate.DeleteColumn:
			change.Table = oas.NewOptString(value.Table)
			change.Column = oas.NewOptString(value.Column)
		case migrate.CreateIndex:
			change.Table = oas.NewOptString(value.Table)
		case migrate.ReplaceColumn:
			change.Table = oas.NewOptString(value.Table)
			change.Column = oas.NewOptString(value.Column)
		case migrate.ValidateNotNull:
			change.Table = oas.NewOptString(value.Table)
			change.Column = oas.NewOptString(value.Column)
		case migrate.DeleteIndex:
			change.Table = oas.NewOptString(value.Table)
		}
		out[i] = change
	}
	return out
}

func migrationStepKind(step migrate.Step) string {
	switch step.(type) {
	case migrate.RenameTable:
		return "rename_table"
	case migrate.CreateTable:
		return "create_table"
	case migrate.DeleteTable:
		return "delete_table"
	case migrate.RenameColumn:
		return "rename_column"
	case migrate.CreateColumn:
		return "create_column"
	case migrate.ChangeColumnDefault:
		return "change_column_default"
	case migrate.DeleteColumn:
		return "delete_column"
	case migrate.CreateIndex:
		return "start_index_build"
	case migrate.ReplaceColumn:
		return "start_column_replacement"
	case migrate.ValidateNotNull:
		return "start_constraint_validation"
	case migrate.DeleteIndex:
		return "delete_index"
	default:
		return "unknown"
	}
}

func schemaFindings(findings []migration.SchemaFinding) []oas.SchemaFinding {
	out := make([]oas.SchemaFinding, len(findings))
	for i, finding := range findings {
		value := oas.SchemaFinding{Kind: finding.Kind, Summary: finding.Summary}
		if finding.Table != "" {
			value.Table = oas.NewOptString(finding.Table)
		}
		if finding.Column != "" {
			value.Column = oas.NewOptString(finding.Column)
		}
		if finding.Rows > 0 {
			value.Rows = oas.NewOptInt64(int64(finding.Rows))
		}
		out[i] = value
	}
	return out
}

func migrationProgramWire(program execprogram.Program) (pirwire.Program, error) {
	statements := make([]pirwire.Statement, len(program.Statements))
	for i, statement := range program.Statements {
		wire, err := catalogStatementWire(statement)
		if err != nil {
			return pirwire.Program{}, err
		}
		statements[i] = wire
	}
	return pirwire.Prog("", statements...), nil
}

func catalogStatementWire(statement execprogram.Statement) (pirwire.Statement, error) {
	switch statement.Kind {
	case execprogram.CreateTable:
		definition, err := tableDefinitionWire(statement.TableDef)
		if err != nil {
			return pirwire.Statement{}, err
		}
		return pirwire.CreateTable(statement.Name, definition), nil
	case execprogram.RenameTable:
		return pirwire.RenameTable(statement.Name, pirwire.SchemaID(statement.TableID), statement.To), nil
	case execprogram.DeleteTable:
		return pirwire.DeleteTable(statement.Name, pirwire.SchemaID(statement.TableID)), nil
	case execprogram.CreateColumn:
		definition, err := columnDefinitionWire(statement.Column)
		if err != nil {
			return pirwire.Statement{}, err
		}
		return pirwire.CreateColumn(statement.Name, pirwire.SchemaID(statement.TableID), definition), nil
	case execprogram.RenameColumn:
		return pirwire.RenameColumn(
			statement.Name, pirwire.SchemaID(statement.TableID), pirwire.SchemaID(statement.ColumnID), statement.To,
		), nil
	case execprogram.ChangeColumnDefault:
		var value *pirwire.ColumnDefault
		if statement.InsertDefault != nil {
			if statement.InsertDefault.Typed == nil || statement.InsertDefault.Type == "" {
				return pirwire.Statement{}, fmt.Errorf(
					"schema diff: default change %q lacks a typed default",
					statement.Name,
				)
			}
			var err error
			value, err = columnDefaultWire(
				statement.InsertDefault.Type,
				statement.InsertDefault.Typed,
			)
			if err != nil {
				return pirwire.Statement{}, err
			}
		}
		return pirwire.ChangeColumnDefault(
			statement.Name,
			pirwire.SchemaID(statement.TableID),
			pirwire.SchemaID(statement.ColumnID),
			value,
		), nil
	case execprogram.DeleteColumn:
		return pirwire.DeleteColumn(
			statement.Name, pirwire.SchemaID(statement.TableID), pirwire.SchemaID(statement.ColumnID),
		), nil
	case execprogram.CreateIndex:
		return pirwire.CreateIndex(statement.Name, pirwire.SchemaID(statement.TableID), pirwire.IndexDefinition{
			Name: statement.Index.Name, Columns: statement.Index.Columns, Unique: boolPointer(statement.Index.Unique),
		}), nil
	case execprogram.StartIndexBuild:
		wire := pirwire.StartIndexBuild(statement.Name, pirwire.SchemaID(statement.TableID), pirwire.IndexDefinition{
			Name: statement.Index.Name, Columns: statement.Index.Columns, Unique: boolPointer(statement.Index.Unique),
		}, statement.Prerequisites...)
		return pirwire.WithAfter(wire, statement.After...), nil
	case execprogram.StartColumnReplacement:
		replacement, err := columnReplacementWire(statement.Replacement)
		if err != nil {
			return pirwire.Statement{}, err
		}
		wire := pirwire.StartColumnReplacement(
			statement.Name,
			pirwire.SchemaID(statement.TableID),
			pirwire.SchemaID(statement.ColumnID),
			replacement,
		)
		return pirwire.WithAfter(wire, statement.After...), nil
	case execprogram.StartConstraintValidation:
		wire := pirwire.StartConstraintValidation(
			statement.Name,
			pirwire.SchemaID(statement.TableID),
			pirwire.ConstraintValidationDefinition{
				Name:          statement.Constraint.Name,
				Kind:          string(statement.Constraint.Kind),
				ColumnID:      pirwire.SchemaID(statement.Constraint.ColumnID),
				Prerequisites: statement.Constraint.Prerequisites,
			},
		)
		return pirwire.WithAfter(wire, statement.After...), nil
	case execprogram.DeleteIndex:
		return pirwire.DeleteIndex(statement.Name, pirwire.SchemaID(statement.TableID), statement.IndexName), nil
	default:
		return pirwire.Statement{}, fmt.Errorf("schema diff: unexpected statement kind %q", statement.Kind)
	}
}

func tableDefinitionWire(definition model.TableDef) (pirwire.TableDefinition, error) {
	id := pirwire.SchemaID(definition.ID)
	out := pirwire.TableDefinition{ID: &id, Name: definition.Name, PrimaryKey: definition.PrimaryKey}
	for _, column := range definition.Columns {
		wire, err := columnDefinitionWire(column)
		if err != nil {
			return pirwire.TableDefinition{}, err
		}
		out.Columns = append(out.Columns, wire)
	}
	for _, index := range definition.Indexes {
		out.Indexes = append(out.Indexes, pirwire.IndexDefinition{
			Name: index.Name, Columns: index.Columns, Unique: boolPointer(index.Unique),
		})
	}
	for _, foreignKey := range definition.ForeignKeys {
		out.ForeignKeys = append(out.ForeignKeys, pirwire.ForeignKeyDefinition{
			Name: foreignKey.Name, Columns: foreignKey.Columns,
			RefTable: foreignKey.RefTable, RefColumns: foreignKey.RefColumns,
		})
	}
	return out, nil
}

func columnDefinitionWire(definition model.ColumnDef) (pirwire.ColumnDefinition, error) {
	id := pirwire.SchemaID(definition.ID)
	out := pirwire.ColumnDefinition{
		ID: &id, Name: definition.Name, Type: pirwire.ColumnType(definition.Type),
		Nullable: boolPointer(definition.Nullable),
	}
	if definition.Format != "" {
		out.Format = &definition.Format
	}
	var err error
	out.Default, err = columnDefaultWire(definition.Type, definition.Default)
	if err != nil {
		return pirwire.ColumnDefinition{}, err
	}
	return out, nil
}

func columnReplacementWire(definition model.ColumnReplacementDef) (pirwire.ColumnReplacementDefinition, error) {
	conversion := string(definition.Conversion)
	out := pirwire.ColumnReplacementDefinition{
		Type: pirwire.ColumnType(definition.Type), Nullable: definition.Nullable,
		Prerequisites: definition.Prerequisites,
	}
	if conversion != "" {
		out.Conversion = &conversion
	}
	if definition.Format != "" {
		out.Format = &definition.Format
	}
	var err error
	out.Default, err = columnDefaultWire(definition.Type, definition.Default)
	if err != nil {
		return pirwire.ColumnReplacementDefinition{}, err
	}
	return out, nil
}

func columnDefaultWire(
	typ model.Type,
	value *model.Default,
) (*pirwire.ColumnDefault, error) {
	if value == nil {
		return nil, nil
	}
	if value.Func != "" {
		return &pirwire.ColumnDefault{ColumnDefaultUnion: &pirwire.GeneratorDefault{
			Kind: "generator", Func: string(value.Func),
		}}, nil
	}
	literal := defaultInfo(model.Column{Type: typ, InsertDefault: value}).Value
	raw, err := json.Marshal(literal)
	if err != nil {
		return nil, err
	}
	return &pirwire.ColumnDefault{ColumnDefaultUnion: &pirwire.LiteralDefault{
		Kind: "literal", Value: raw,
	}}, nil
}

func boolPointer(value bool) *bool {
	if !value {
		return nil
	}
	return &value
}
