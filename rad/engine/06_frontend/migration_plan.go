package frontend

import (
	"context"
	"fmt"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/migrate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
)

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
	Current     catalog.Revision
	Desired     catalog.Schema
	DesiredHash string
	Steps       []migrate.Step
	Program     exec.Program
	Destructive []SchemaFinding
	Blocking    []SchemaFinding
}

// PlanMigration computes the semantic catalog transition and data-aware
// findings without changing catalog, rows, revisions, or local state.
func (db *DB) PlanMigration(ctx context.Context, desired *schema.Schema) (MigrationPlan, error) {
	revision, currentTables, err := db.eng.CatalogSnapshot(ctx)
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
	program, err := migrationProgram(currentTables, steps)
	if err != nil {
		return MigrationPlan{}, err
	}
	plan := MigrationPlan{
		Current: revision, Desired: canonical, DesiredHash: desiredHash,
		Steps: steps, Program: program,
	}
	plan.Destructive, plan.Blocking, err = db.preflightMigrationData(ctx, currentTables, desired, steps)
	if err != nil {
		return MigrationPlan{}, err
	}
	return plan, nil
}

func (db *DB) preflightMigrationData(
	ctx context.Context,
	current []catalog.Table,
	desired *schema.Schema,
	steps []migrate.Step,
) ([]SchemaFinding, []SchemaFinding, error) {
	currentBySchemaID := make(map[catalog.SchemaID]catalog.Table, len(current))
	for _, table := range current {
		currentBySchemaID[table.SchemaID] = table
	}
	desiredByName := make(map[string]catalog.TableDef, len(desired.Tables))
	for _, table := range desired.Tables {
		desiredByName[table.Def.Name] = table.Def
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
			columns, ok := existingColumnNames(table, definition, value.Def.Columns)
			if !ok {
				continue
			}
			duplicates, err := db.countDuplicateKeys(ctx, table.Name, columns)
			if err != nil {
				return nil, nil, err
			}
			if duplicates > 0 {
				blocking = append(blocking, SchemaFinding{
					Kind: "unique_index_duplicates", Table: value.Table, Rows: duplicates,
					Summary: fmt.Sprintf("index %s cannot become unique because %d duplicate keys exist", value.Def.Name, duplicates),
				})
			}
		}
	}
	return destructive, blocking, nil
}

func existingColumnNames(current catalog.Table, desired catalog.TableDef, names []string) ([]string, bool) {
	desiredByName := make(map[string]catalog.ColumnDef, len(desired.Columns))
	currentByID := make(map[catalog.SchemaID]catalog.Column, len(current.Columns))
	for _, column := range desired.Columns {
		desiredByName[column.Name] = column
	}
	for _, column := range current.Columns {
		currentByID[column.SchemaID] = column
	}
	out := make([]string, len(names))
	for i, name := range names {
		desiredColumn, exists := desiredByName[name]
		if !exists {
			return nil, false
		}
		currentColumn, exists := currentByID[desiredColumn.ID]
		if !exists {
			return nil, false
		}
		out[i] = currentColumn.Name
	}
	return out, true
}

func (db *DB) countRows(ctx context.Context, table, nonNullColumn string) (uint64, error) {
	iterator, err := db.eng.ScanTable(ctx, table)
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

func (db *DB) countDuplicateKeys(ctx context.Context, table string, columns []string) (uint64, error) {
	iterator, err := db.eng.ScanTable(ctx, table)
	if err != nil {
		return 0, err
	}
	defer iterator.Close()
	seen := map[string]bool{}
	var duplicates uint64
	for {
		row, ok, err := iterator.Next()
		if err != nil || !ok {
			return duplicates, err
		}
		values := make([]lir.Value, len(columns))
		hasNull := false
		for i, column := range columns {
			values[i] = row[column]
			hasNull = hasNull || values[i].Null
		}
		if hasNull {
			continue
		}
		key, err := exec.EncodeTuple(values)
		if err != nil {
			return 0, err
		}
		if seen[string(key)] {
			duplicates++
		}
		seen[string(key)] = true
	}
}
