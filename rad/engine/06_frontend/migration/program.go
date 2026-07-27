package migration

import (
	"fmt"
	"slices"

	"github.com/Southclaws/rad/rad/engine/02_catalog/migrate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	execprogram "github.com/Southclaws/rad/rad/engine/05_exec/program"
)

// migrationProgram lowers a name-oriented migration plan into stable-ID PIR
// statements. The small logical state machine resolves each step against the
// schema left by its predecessors, including table and column renames.
func migrationProgram(current []model.Table, steps []migrate.Step) (execprogram.Program, error) {
	tables := make(map[string]model.Table, len(current))
	for _, table := range current {
		table.Columns = slices.Clone(table.Columns)
		tables[table.Name] = table
	}

	type columnKey struct {
		table  model.SchemaID
		column model.SchemaID
	}
	replacementByColumn := map[columnKey]string{}
	statements := make([]execprogram.Statement, 0, len(steps))
	for i, step := range steps {
		name := fmt.Sprintf("migration_%d", i+1)
		var statement execprogram.Statement
		switch value := step.(type) {
		case migrate.RenameTable:
			table, err := migrationTable(tables, value.From)
			if err != nil {
				return execprogram.Program{}, err
			}
			statement = execprogram.Statement{Name: name, Kind: execprogram.RenameTable, TableID: table.SchemaID, To: value.To}
			delete(tables, value.From)
			table.Name = value.To
			tables[value.To] = table
		case migrate.RenameColumn:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return execprogram.Program{}, err
			}
			column, ok := table.Column(value.From)
			if !ok {
				return execprogram.Program{}, fmt.Errorf("migration: column %q.%q does not exist", value.Table, value.From)
			}
			statement = execprogram.Statement{
				Name: name, Kind: execprogram.RenameColumn, TableID: table.SchemaID,
				ColumnID: column.SchemaID, To: value.To,
			}
			for i := range table.Columns {
				if table.Columns[i].SchemaID == column.SchemaID {
					table.Columns[i].Name = value.To
				}
			}
			tables[value.Table] = table
		case migrate.CreateTable:
			statement = execprogram.Statement{Name: name, Kind: execprogram.CreateTable, TableDef: value.Def}
			tables[value.Def.Name] = tableFromDefinition(value.Def)
		case migrate.CreateColumn:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return execprogram.Program{}, err
			}
			statement = execprogram.Statement{Name: name, Kind: execprogram.CreateColumn, TableID: table.SchemaID, Column: value.Def}
			table.Columns = append(table.Columns, model.Column{SchemaID: value.Def.ID, Name: value.Def.Name})
			tables[value.Table] = table
		case migrate.ChangeColumnDefault:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return execprogram.Program{}, err
			}
			column, ok := table.Column(value.Column)
			if !ok {
				return execprogram.Program{}, fmt.Errorf(
					"migration: column %q.%q does not exist",
					value.Table,
					value.Column,
				)
			}
			statement = execprogram.Statement{
				Name: name, Kind: execprogram.ChangeColumnDefault,
				TableID: table.SchemaID, ColumnID: column.SchemaID,
				InsertDefault: execprogram.TypedDefaultForType(value.Default, column.Type),
			}
			for i := range table.Columns {
				if table.Columns[i].SchemaID == column.SchemaID {
					table.Columns[i].InsertDefault = cloneDefault(value.Default)
				}
			}
			tables[value.Table] = table
		case migrate.CreateIndex:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return execprogram.Program{}, err
			}
			var after []string
			for _, columnName := range value.Def.Columns {
				column, ok := table.Column(columnName)
				if !ok {
					return execprogram.Program{}, fmt.Errorf("migration: index %q column %q.%q does not exist", value.Def.Name, value.Table, columnName)
				}
				if prerequisite := replacementByColumn[columnKey{table: table.SchemaID, column: column.SchemaID}]; prerequisite != "" {
					after = append(after, prerequisite)
				}
			}
			slices.Sort(after)
			after = slices.Compact(after)
			statement = execprogram.Statement{
				Name: name, Kind: execprogram.StartIndexBuild, TableID: table.SchemaID,
				Index: value.Def, After: after,
			}
		case migrate.ReplaceColumn:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return execprogram.Program{}, err
			}
			column, ok := table.Column(value.Column)
			if !ok {
				return execprogram.Program{}, fmt.Errorf("migration: column %q.%q does not exist", value.Table, value.Column)
			}
			statement = execprogram.Statement{
				Name: name, Kind: execprogram.StartColumnReplacement,
				TableID: table.SchemaID, ColumnID: column.SchemaID, Replacement: value.Def,
			}
			replacementByColumn[columnKey{table: table.SchemaID, column: column.SchemaID}] = name
		case migrate.ValidateNotNull:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return execprogram.Program{}, err
			}
			var after []string
			if prerequisite := replacementByColumn[columnKey{table: table.SchemaID, column: value.Def.ColumnID}]; prerequisite != "" {
				after = []string{prerequisite}
			}
			statement = execprogram.Statement{
				Name: name, Kind: execprogram.StartConstraintValidation,
				TableID: table.SchemaID, Constraint: value.Def, After: after,
			}
		case migrate.DeleteIndex:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return execprogram.Program{}, err
			}
			statement = execprogram.Statement{Name: name, Kind: execprogram.DeleteIndex, TableID: table.SchemaID, IndexName: value.Index}
		case migrate.DeleteColumn:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return execprogram.Program{}, err
			}
			column, ok := table.Column(value.Column)
			if !ok {
				return execprogram.Program{}, fmt.Errorf("migration: column %q.%q does not exist", value.Table, value.Column)
			}
			statement = execprogram.Statement{
				Name: name, Kind: execprogram.DeleteColumn,
				TableID: table.SchemaID, ColumnID: column.SchemaID,
			}
			table.Columns = slices.DeleteFunc(table.Columns, func(candidate model.Column) bool {
				return candidate.SchemaID == column.SchemaID
			})
			tables[value.Table] = table
		case migrate.DeleteTable:
			table, err := migrationTable(tables, value.Table)
			if err != nil {
				return execprogram.Program{}, err
			}
			statement = execprogram.Statement{Name: name, Kind: execprogram.DeleteTable, TableID: table.SchemaID}
			delete(tables, value.Table)
		default:
			return execprogram.Program{}, fmt.Errorf("migration: unknown step %T", step)
		}
		statements = append(statements, statement)
	}
	return execprogram.Program{Statements: statements}, nil
}

func migrationTable(tables map[string]model.Table, name string) (model.Table, error) {
	table, ok := tables[name]
	if !ok {
		return model.Table{}, fmt.Errorf("migration: table %q does not exist", name)
	}
	return table, nil
}

func cloneDefault(value *model.Default) *model.Default {
	if value == nil {
		return nil
	}
	copy := *value
	return &copy
}

func tableFromDefinition(def model.TableDef) model.Table {
	table := model.Table{SchemaID: def.ID, Name: def.Name}
	for _, column := range def.Columns {
		table.Columns = append(table.Columns, model.Column{SchemaID: column.ID, Name: column.Name})
	}
	return table
}
