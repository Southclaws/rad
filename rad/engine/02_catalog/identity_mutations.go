package catalog

import (
	"context"

	"github.com/Southclaws/rad/rad/engine/reject"
)

// TableBySchemaID resolves a table through this mutation's current catalog
// view, including changes made earlier in the same transaction.
func (m *Mutation) TableBySchemaID(ctx context.Context, id SchemaID) (Table, error) {
	table, ok, err := getTableBySchemaID(ctx, m.view, id)
	if err != nil {
		return Table{}, err
	}
	if !ok {
		return Table{}, reject.Inputf("catalog: table schema ID %d does not exist", id)
	}
	return table, nil
}

// ColumnBySchemaID resolves a column within a stable table identity.
func (m *Mutation) ColumnBySchemaID(ctx context.Context, tableID, columnID SchemaID) (Table, Column, error) {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return Table{}, Column{}, err
	}
	if columnID == 0 || columnID > MaxSchemaID {
		return Table{}, Column{}, reject.Inputf("catalog: invalid column schema ID %d", columnID)
	}
	for _, column := range table.Columns {
		if column.SchemaID == columnID {
			return table, column, nil
		}
	}
	return Table{}, Column{}, reject.Inputf(
		"catalog: column schema ID %d does not exist on table schema ID %d", columnID, tableID)
}

func (m *Mutation) RenameTableBySchemaID(ctx context.Context, tableID SchemaID, to string) error {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return err
	}
	return m.RenameTable(ctx, table.Name, to)
}

func (m *Mutation) DeleteTableBySchemaID(ctx context.Context, tableID SchemaID) error {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return err
	}
	return m.DeleteTable(ctx, table.Name)
}

func (m *Mutation) CreateColumnBySchemaID(ctx context.Context, tableID SchemaID, def ColumnDef) (Table, error) {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return Table{}, err
	}
	return m.CreateColumn(ctx, table.Name, def)
}

func (m *Mutation) RenameColumnBySchemaID(ctx context.Context, tableID, columnID SchemaID, to string) (Table, error) {
	table, column, err := m.ColumnBySchemaID(ctx, tableID, columnID)
	if err != nil {
		return Table{}, err
	}
	return m.RenameColumn(ctx, table.Name, column.Name, to)
}

func (m *Mutation) DeleteColumnBySchemaID(ctx context.Context, tableID, columnID SchemaID) (Table, error) {
	table, column, err := m.ColumnBySchemaID(ctx, tableID, columnID)
	if err != nil {
		return Table{}, err
	}
	return m.DeleteColumn(ctx, table.Name, column.Name)
}

func (m *Mutation) DeleteIndexBySchemaID(ctx context.Context, tableID SchemaID, index string) error {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return err
	}
	return m.DeleteIndex(ctx, table.Name, index)
}
