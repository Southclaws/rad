package migration

import (
	"context"
	"fmt"
	"slices"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/migrate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
)

type Service struct {
	engine  *exec.Engine
	catalog *catalog.Catalog
}

func New(engine *exec.Engine, catalog *catalog.Catalog) *Service {
	return &Service{engine: engine, catalog: catalog}
}

// SchemaFinding is a data-dependent fact discovered before a migration.
// Destructive findings require consent; blocking findings describe a target
// schema that cannot be committed against the current rows.
type SchemaFinding struct {
	Kind    string
	Summary string
	Table   string
	Column  string
	Rows    uint64
}

// MigrationPlan is the server-owned interpretation of one desired schema.
// Program is the exact catalog PIR the executor will run if the plan is
// accepted.
type MigrationPlan struct {
	Current     model.Revision
	Desired     model.Schema
	DesiredHash string
	Steps       []migrate.Step
	Program     execprogram.Program
	Transitions []model.TransitionControl
	Destructive []SchemaFinding
	Blocking    []SchemaFinding
}

type schemaColumnKey struct {
	table  model.SchemaID
	column model.SchemaID
}

// PlanMigration computes the semantic catalog transition and data-aware
// findings without changing catalog, rows, revisions, or local state.
func (db *Service) PlanMigration(ctx context.Context, desired *schema.Schema) (MigrationPlan, error) {
	revision, currentTables, transitions, err := db.engine.SchemaMigrationSnapshot(ctx)
	if err != nil {
		return MigrationPlan{}, err
	}
	canonical := desired.Canonical()
	desiredHash, err := canonical.Hash()
	if err != nil {
		return MigrationPlan{}, err
	}
	steps, err := migrate.Diff(currentTables, desired)
	if err != nil {
		return MigrationPlan{}, err
	}
	programSteps, recovered, transitionFindings := recoverMigrationTransitions(canonical, steps, transitions)
	program, err := migrationProgram(currentTables, programSteps)
	if err != nil {
		return MigrationPlan{}, err
	}
	plan := MigrationPlan{
		Current: revision, Desired: canonical, DesiredHash: desiredHash,
		Steps: steps, Program: program, Transitions: recovered,
	}
	structureFindings := migrationStructureFindings(currentTables, canonical, steps)
	plan.Destructive, plan.Blocking, err = db.preflightMigrationData(ctx, currentTables, desired, steps)
	if err != nil {
		return MigrationPlan{}, err
	}
	plan.Blocking = append(structureFindings, plan.Blocking...)
	plan.Blocking = append(plan.Blocking, transitionFindings...)
	return plan, nil
}

func migrationStructureFindings(
	current []model.Table,
	desired model.Schema,
	steps []migrate.Step,
) []SchemaFinding {
	desiredByName := make(map[string]model.TableDef, len(desired.Tables))
	currentBySchemaID := make(map[model.SchemaID]model.Table, len(current))
	for _, table := range desired.Tables {
		desiredByName[table.Name] = table
	}
	for _, table := range current {
		currentBySchemaID[table.SchemaID] = table
	}
	deletedIndexes := map[string]bool{}
	for _, step := range steps {
		switch value := step.(type) {
		case migrate.DeleteIndex:
			deletedIndexes[value.Table+"\x00"+value.Index] = true
		}
	}

	var findings []SchemaFinding
	for _, step := range steps {
		replacement, ok := step.(migrate.ReplaceColumn)
		if !ok {
			continue
		}
		definition, ok := desiredByName[replacement.Table]
		if !ok {
			continue
		}
		table, ok := currentBySchemaID[definition.ID]
		if !ok {
			continue
		}
		column, ok := columnBySchemaID(table, replacement.ColumnID)
		if !ok {
			continue
		}
		dependency := ""
		switch {
		case slices.Contains(table.PrimaryKey, column.Name):
			dependency = "the primary key"
		default:
			for _, index := range table.Indexes {
				if !slices.Contains(index.ColumnIDs, column.ID) && !slices.Contains(index.Columns, column.Name) {
					continue
				}
				if !deletedIndexes[replacement.Table+"\x00"+index.Name] {
					dependency = fmt.Sprintf("index %s", index.Name)
					break
				}
			}
		}
		if dependency == "" {
			for _, constraint := range table.Constraints {
				if replacement.Def.Nullable &&
					constraint.Kind == model.ConstraintNotNull &&
					constraint.State == model.ConstraintValid &&
					slices.Contains(constraint.ColumnIDs, column.ID) {
					dependency = fmt.Sprintf("valid constraint %s", constraint.Name)
					break
				}
			}
		}
		if dependency == "" {
			for _, candidate := range current {
				for _, foreignKey := range candidate.ForeignKeys {
					usesLocal := candidate.ID == table.ID &&
						(slices.Contains(foreignKey.Columns, column.Name) || slices.Contains(foreignKey.RefColumns, column.Name))
					usesReference := foreignKey.RefTableID == table.ID && slices.Contains(foreignKey.RefColumns, column.Name)
					if usesLocal || usesReference {
						dependency = fmt.Sprintf("foreign key %s", foreignKey.Name)
						break
					}
				}
				if dependency != "" {
					break
				}
			}
		}
		if dependency != "" {
			findings = append(findings, SchemaFinding{
				Kind: "column_replacement_dependency", Table: replacement.Table, Column: replacement.Column,
				Summary: fmt.Sprintf("column %s.%s cannot be replaced while %s depends on its physical representation", replacement.Table, replacement.Column, dependency),
			})
		}
	}
	return findings
}

func recoverMigrationTransitions(
	desired model.Schema,
	steps []migrate.Step,
	transitions []model.SchemaTransition,
) ([]migrate.Step, []model.TransitionControl, []SchemaFinding) {
	desiredTables := make(map[string]model.TableDef, len(desired.Tables))
	for _, table := range desired.Tables {
		desiredTables[table.Name] = table
	}
	var program []migrate.Step
	var recovered []model.TransitionControl
	var blocking []SchemaFinding
	for _, step := range steps {
		matched := false
		conflict := false
		for _, transition := range transitions {
			if transitionTerminal(transition.State) {
				continue
			}
			switch value := step.(type) {
			case migrate.ReplaceColumn:
				definition, ok := desiredTables[value.Table]
				if !ok || transition.Kind != model.TransitionColumnReplacement ||
					transition.TableSchemaID != definition.ID ||
					!slices.Contains(transition.AffectedColumnIDs, value.ColumnID) {
					continue
				}
				if replacementRequestEqual(transition.ReplacementRequest, value) {
					matched = true
					recovered = append(recovered, transition.Control())
				} else {
					conflict = true
				}
			case migrate.ValidateNotNull:
				definition, ok := desiredTables[value.Table]
				if !ok || transition.Kind != model.TransitionConstraintValidation ||
					transition.TableSchemaID != definition.ID ||
					!slices.Contains(transition.AffectedColumnIDs, value.Def.ColumnID) {
					continue
				}
				if transition.Constraint != nil && transition.ConstraintRequest != nil &&
					transition.Constraint.Name == value.Def.Name &&
					transition.Constraint.Kind == value.Def.Kind &&
					transition.ConstraintRequest.ColumnSchemaID == value.Def.ColumnID {
					matched = true
					recovered = append(recovered, transition.Control())
				} else {
					conflict = true
				}
			case migrate.CreateIndex:
				definition, ok := desiredTables[value.Table]
				if !ok || transition.Kind != model.TransitionIndexBuild ||
					transition.TableSchemaID != definition.ID || transition.IndexRequest == nil ||
					transition.IndexRequest.Name != value.Def.Name {
					continue
				}
				columnIDs, ok := desiredIndexColumnIDs(definition, value.Def.Columns)
				if ok && slices.Equal(transition.IndexRequest.ColumnSchemaIDs, columnIDs) &&
					transition.IndexRequest.Unique == value.Def.Unique {
					matched = true
					recovered = append(recovered, transition.Control())
				} else {
					conflict = true
				}
			}
			if matched || conflict {
				break
			}
		}
		if matched {
			continue
		}
		if conflict {
			blocking = append(blocking, SchemaFinding{
				Kind: "active_schema_transition_conflict", Summary: fmt.Sprintf("%s conflicts with active schema work", step),
			})
			continue
		}
		program = append(program, step)
	}
	return program, recovered, blocking
}

func transitionTerminal(state model.TransitionState) bool {
	switch state {
	case model.TransitionReady, model.TransitionFailed, model.TransitionCancelled:
		return true
	default:
		return false
	}
}

func replacementRequestEqual(request *model.ColumnReplacementRequest, step migrate.ReplaceColumn) bool {
	if request == nil || request.ColumnSchemaID != step.ColumnID || request.Type != step.Def.Type ||
		request.Nullable != step.Def.Nullable || request.Format != step.Def.Format ||
		request.Conversion != step.Def.Conversion {
		return false
	}
	return defaultsEqual(request.Default, step.Def.Default)
}

func defaultsEqual(a, b *model.Default) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
}

func desiredIndexColumnIDs(table model.TableDef, names []string) ([]model.SchemaID, bool) {
	byName := make(map[string]model.SchemaID, len(table.Columns))
	for _, column := range table.Columns {
		byName[column.Name] = column.ID
	}
	ids := make([]model.SchemaID, len(names))
	for i, name := range names {
		id, ok := byName[name]
		if !ok {
			return nil, false
		}
		ids[i] = id
	}
	return ids, true
}

func (db *Service) preflightMigrationData(
	ctx context.Context,
	current []model.Table,
	desired *schema.Schema,
	steps []migrate.Step,
) ([]SchemaFinding, []SchemaFinding, error) {
	currentBySchemaID := make(map[model.SchemaID]model.Table, len(current))
	for _, table := range current {
		currentBySchemaID[table.SchemaID] = table
	}
	desiredByName := make(map[string]model.TableDef, len(desired.Tables))
	for _, table := range desired.Tables {
		desiredByName[table.Def.Name] = table.Def
	}
	replacements := map[schemaColumnKey]model.ColumnReplacementDef{}
	for _, step := range steps {
		replacement, ok := step.(migrate.ReplaceColumn)
		if !ok {
			continue
		}
		table, ok := desiredByName[replacement.Table]
		if ok {
			replacements[schemaColumnKey{table: table.ID, column: replacement.ColumnID}] = replacement.Def
		}
	}

	var destructive, blocking []SchemaFinding
	for _, step := range steps {
		switch value := step.(type) {
		case migrate.DeleteTable:
			rows, err := db.countRows(ctx, value.Table, "")
			if err != nil {
				return nil, nil, err
			}
			if rows > 0 {
				destructive = append(destructive, SchemaFinding{
					Kind: "delete_table", Table: value.Table, Rows: rows,
					Summary: fmt.Sprintf("table %s will be deleted (%d rows)", value.Table, rows),
				})
			}
		case migrate.DeleteColumn:
			definition, ok := desiredByName[value.Table]
			if !ok {
				return nil, nil, fmt.Errorf("migration preflight: desired table %q is missing", value.Table)
			}
			table, ok := currentBySchemaID[definition.ID]
			if !ok {
				return nil, nil, fmt.Errorf("migration preflight: current table ID %d is missing", definition.ID)
			}
			rows, err := db.countRows(ctx, table.Name, value.Column)
			if err != nil {
				return nil, nil, err
			}
			if rows > 0 {
				destructive = append(destructive, SchemaFinding{
					Kind: "delete_column", Table: value.Table, Column: value.Column, Rows: rows,
					Summary: fmt.Sprintf("column %s.%s will be deleted (%d rows contain a value)", value.Table, value.Column, rows),
				})
			}
		case migrate.CreateIndex:
			if !value.Def.Unique {
				continue
			}
			definition, ok := desiredByName[value.Table]
			if !ok {
				continue
			}
			table, ok := currentBySchemaID[definition.ID]
			if !ok {
				continue
			}
			duplicates, comparable, err := db.countDesiredDuplicateKeys(
				ctx,
				table,
				definition,
				value.Def.Columns,
				replacements,
			)
			if err != nil {
				return nil, nil, err
			}
			if !comparable {
				continue
			}
			if duplicates > 0 {
				blocking = append(blocking, SchemaFinding{
					Kind: "unique_index_duplicates", Table: value.Table, Rows: duplicates,
					Summary: fmt.Sprintf("index %s cannot become unique because %d duplicate keys exist", value.Def.Name, duplicates),
				})
			}
		case migrate.ReplaceColumn:
			definition, ok := desiredByName[value.Table]
			if !ok {
				return nil, nil, fmt.Errorf("migration preflight: desired table %q is missing", value.Table)
			}
			table, ok := currentBySchemaID[definition.ID]
			if !ok {
				return nil, nil, fmt.Errorf("migration preflight: current table ID %d is missing", definition.ID)
			}
			source, ok := columnBySchemaID(table, value.ColumnID)
			if !ok {
				return nil, nil, fmt.Errorf("migration preflight: current column ID %d on table %q is missing", value.ColumnID, table.Name)
			}
			failures, err := db.countConversionFailures(ctx, table.Name, source, value.Def)
			if err != nil {
				return nil, nil, err
			}
			if failures > 0 {
				blocking = append(blocking, SchemaFinding{
					Kind: "column_conversion", Table: value.Table, Column: value.Column, Rows: failures,
					Summary: fmt.Sprintf("column %s.%s has %d values that cannot be converted to %s", value.Table, value.Column, failures, value.Def.Type),
				})
			}
		case migrate.ValidateNotNull:
			definition, ok := desiredByName[value.Table]
			if !ok {
				return nil, nil, fmt.Errorf("migration preflight: desired table %q is missing", value.Table)
			}
			table, ok := currentBySchemaID[definition.ID]
			if !ok {
				return nil, nil, fmt.Errorf("migration preflight: current table ID %d is missing", definition.ID)
			}
			column, ok := columnBySchemaID(table, value.Def.ColumnID)
			if !ok {
				return nil, nil, fmt.Errorf("migration preflight: current column ID %d on table %q is missing", value.Def.ColumnID, table.Name)
			}
			nonNull, err := db.countRows(ctx, table.Name, column.Name)
			if err != nil {
				return nil, nil, err
			}
			total, err := db.countRows(ctx, table.Name, "")
			if err != nil {
				return nil, nil, err
			}
			if nulls := total - nonNull; nulls > 0 {
				blocking = append(blocking, SchemaFinding{
					Kind: "not_null_existing_nulls", Table: value.Table, Column: value.Column, Rows: nulls,
					Summary: fmt.Sprintf("column %s.%s contains %d NULL values", value.Table, value.Column, nulls),
				})
			}
		}
	}
	return destructive, blocking, nil
}

func columnBySchemaID(table model.Table, id model.SchemaID) (model.Column, bool) {
	for _, column := range table.Columns {
		if column.SchemaID == id {
			return column, true
		}
	}
	return model.Column{}, false
}

func (db *Service) countConversionFailures(
	ctx context.Context,
	table string,
	source model.Column,
	target model.ColumnReplacementDef,
) (uint64, error) {
	iterator, err := db.engine.ScanTable(ctx, table)
	if err != nil {
		return 0, err
	}
	defer iterator.Close()
	var failures uint64
	for {
		row, ok, err := iterator.Next()
		if err != nil || !ok {
			return failures, err
		}
		if _, err := codec.ConvertColumnValue(row[source.Name], model.Column{
			Type: target.Type, Nullable: target.Nullable, Format: target.Format,
		}, target.Conversion); err != nil {
			failures++
		}
	}
}

func (db *Service) countDesiredDuplicateKeys(
	ctx context.Context,
	current model.Table,
	desired model.TableDef,
	names []string,
	replacements map[schemaColumnKey]model.ColumnReplacementDef,
) (uint64, bool, error) {
	desiredByName := make(map[string]model.ColumnDef, len(desired.Columns))
	currentByID := make(map[model.SchemaID]model.Column, len(current.Columns))
	for _, column := range desired.Columns {
		desiredByName[column.Name] = column
	}
	for _, column := range current.Columns {
		currentByID[column.SchemaID] = column
	}
	type valueSource struct {
		column      model.Column
		replacement *model.ColumnReplacementDef
		historical  *lir.Value
	}
	sources := make([]valueSource, len(names))
	for i, name := range names {
		desiredColumn, exists := desiredByName[name]
		if !exists {
			return 0, false, nil
		}
		currentColumn, exists := currentByID[desiredColumn.ID]
		if !exists {
			value, err := historicalMissingValue(desiredColumn)
			if err != nil {
				return 0, false, err
			}
			sources[i].historical = &value
			continue
		}
		sources[i].column = currentColumn
		if replacement, ok := replacements[schemaColumnKey{table: current.SchemaID, column: desiredColumn.ID}]; ok {
			copy := replacement
			sources[i].replacement = &copy
		}
	}

	iterator, err := db.engine.ScanTable(ctx, current.Name)
	if err != nil {
		return 0, false, err
	}
	defer iterator.Close()
	seen := map[string]bool{}
	var duplicates uint64
	for {
		row, ok, err := iterator.Next()
		if err != nil || !ok {
			return duplicates, true, err
		}
		values := make([]lir.Value, len(sources))
		hasNull := false
		invalid := false
		for i, source := range sources {
			if source.historical != nil {
				values[i] = *source.historical
			} else {
				values[i] = row[source.column.Name]
			}
			if source.replacement != nil {
				values[i], err = codec.ConvertColumnValue(values[i], model.Column{
					Type: source.replacement.Type, Nullable: source.replacement.Nullable,
					Format: source.replacement.Format,
				}, source.replacement.Conversion)
				if err != nil {
					invalid = true
					break
				}
			}
			hasNull = hasNull || values[i].Null
		}
		if invalid || hasNull {
			continue
		}
		key, err := codec.EncodeTuple(values)
		if err != nil {
			return 0, false, err
		}
		if seen[string(key)] {
			duplicates++
		}
		seen[string(key)] = true
	}
}

func historicalMissingValue(column model.ColumnDef) (lir.Value, error) {
	var missing *model.Default
	if column.Default != nil && column.Default.Func == "" {
		missing = column.Default
	}
	return codec.DecodeMissingValue(model.Column{Type: column.Type, MissingValue: missing})
}

func (db *Service) countRows(ctx context.Context, table, nonNullColumn string) (uint64, error) {
	iterator, err := db.engine.ScanTable(ctx, table)
	if err != nil {
		return 0, err
	}
	defer iterator.Close()
	var count uint64
	for {
		row, ok, err := iterator.Next()
		if err != nil || !ok {
			return count, err
		}
		if nonNullColumn == "" || !row[nonNullColumn].Null {
			count++
		}
	}
}
