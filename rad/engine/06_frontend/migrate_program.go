package frontend

import (
	"fmt"
	"slices"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/migrate"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
)

// migrationProgram lowers a name-oriented migration plan into stable-ID PIR
// statements. The small logical state machine resolves each step against the
// schema left by its predecessors, including table and column renames.
func migrationProgram(current []catalog.Table, steps []migrate.Step) (exec.Program, error) {
	tables := make(map[string]catalog.Table, len(current))
	for _, table := range current {
		table.Columns = slices.Clone(table.Columns)
		tables[table.Name] = table
	}

	statements := make([]exec.ProgramStatement, 0, len(steps))
	for i, step := range steps {
		name := fmt.Sprintf("migration_%d", i+1)
		var statement exec.ProgramStatement
		switch value := step.(type) {
		case migrate.RenameTable:
			table, err := migrationTable(tables, value.From)
			if err != nil {
				return exec.Program{}, err
			}
			statement = exec.ProgramStatement{Name: name, Kind: exec.StmtRenameTable, TableID: table.SchemaID, To: value.To}
			delete(tables, value.From)
			table.Name = value.To
			tables[value.To] = table
		case migrate.RenameColumn:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return exec.Program{}, err
			}
			column, ok := table.Column(value.From)
			if !ok {
				return exec.Program{}, fmt.Errorf("migration: column %q.%q does not exist", value.Table, value.From)
			}
			statement = exec.ProgramStatement{
				Name: name, Kind: exec.StmtRenameColumn, TableID: table.SchemaID,
				ColumnID: column.SchemaID, To: value.To,
			}
			for i := range table.Columns {
				if table.Columns[i].SchemaID == column.SchemaID {
					table.Columns[i].Name = value.To
				}
			}
			tables[value.Table] = table
		case migrate.CreateTable:
			statement = exec.ProgramStatement{Name: name, Kind: exec.StmtCreateTable, TableDef: value.Def}
			tables[value.Def.Name] = tableFromDefinition(value.Def)
		case migrate.CreateColumn:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return exec.Program{}, err
			}
			statement = exec.ProgramStatement{Name: name, Kind: exec.StmtCreateColumn, TableID: table.SchemaID, Column: value.Def}
			table.Columns = append(table.Columns, catalog.Column{SchemaID: value.Def.ID, Name: value.Def.Name})
			tables[value.Table] = table
		case migrate.CreateIndex:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return exec.Program{}, err
			}
			statement = exec.ProgramStatement{Name: name, Kind: exec.StmtCreateIndex, TableID: table.SchemaID, Index: value.Def}
		case migrate.DeleteIndex:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return exec.Program{}, err
			}
			statement = exec.ProgramStatement{Name: name, Kind: exec.StmtDeleteIndex, TableID: table.SchemaID, IndexName: value.Index}
		case migrate.DeleteColumn:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return exec.Program{}, err
			}
			column, ok := table.Column(value.Column)
			if !ok {
				return exec.Program{}, fmt.Errorf("migration: column %q.%q does not exist", value.Table, value.Column)
			}
			statement = exec.ProgramStatement{
				Name: name, Kind: exec.StmtDeleteColumn,
				TableID: table.SchemaID, ColumnID: column.SchemaID,
			}
			table.Columns = slices.DeleteFunc(table.Columns, func(candidate catalog.Column) bool {
				return candidate.SchemaID == column.SchemaID
			})
			tables[value.Table] = table
		case migrate.DeleteTable:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return exec.Program{}, err
			}
			statement = exec.ProgramStatement{Name: name, Kind: exec.StmtDeleteTable, TableID: table.SchemaID}
			delete(tables, value.Table)
		default:
			return exec.Program{}, fmt.Errorf("migration: unknown step %T", step)
		}
		statements = append(statements, statement)
	}
	return exec.Program{Statements: statements}, nil
}

func migrationTable(tables map[string]catalog.Table, name string) (catalog.Table, error) {
	table, ok := tables[name]
	if !ok {
		return catalog.Table{}, fmt.Errorf("migration: table %q does not exist", name)
	}
	return table, nil
}

func tableFromDefinition(def catalog.TableDef) catalog.Table {
	table := catalog.Table{SchemaID: def.ID, Name: def.Name}
	for _, column := range def.Columns {
		table.Columns = append(table.Columns, catalog.Column{SchemaID: column.ID, Name: column.Name})
	}
	return table
}
