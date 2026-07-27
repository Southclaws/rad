package change

import (
	"context"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// TableBySchemaID resolves a table through this mutation's current catalog
// view, including changes made earlier in the same transaction.
func (m *Mutation) TableBySchemaID(ctx context.Context, id model.SchemaID) (model.Table, error) {
	table, ok, err := store.New(m.view).GetTableBySchemaID(ctx, id)
	if err != nil {
		return model.Table{}, err
	}
	if !ok {
		return model.Table{}, reject.Inputf("catalog: table schema ID %d does not exist", id)
	}
	return table, nil
}

func (m *Mutation) columnBySchemaID(ctx context.Context, tableID, columnID model.SchemaID) (model.Table, model.Column, error) {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return model.Table{}, model.Column{}, err
	}
	if columnID == 0 || columnID > model.MaxSchemaID {
		return model.Table{}, model.Column{}, reject.Inputf("catalog: invalid column schema ID %d", columnID)
	}
	for _, column := range table.Columns {
		if column.SchemaID == columnID {
			return table, column, nil
		}
	}
	return model.Table{}, model.Column{}, reject.Inputf(
		"catalog: column schema ID %d does not exist on table schema ID %d", columnID, tableID)
}

// ColumnBySchemaID resolves a column through this mutation's current catalog
// view, including changes made earlier in the same transaction.
func (m *Mutation) ColumnBySchemaID(
	ctx context.Context,
	tableID, columnID model.SchemaID,
) (model.Table, model.Column, error) {
	return m.columnBySchemaID(ctx, tableID, columnID)
}

func (m *Mutation) RenameTableBySchemaID(ctx context.Context, tableID model.SchemaID, to string) error {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return err
	}
	return m.RenameTable(ctx, table.Name, to)
}

func (m *Mutation) DeleteTableBySchemaID(ctx context.Context, tableID model.SchemaID) error {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return err
	}
	return m.DeleteTable(ctx, table.Name)
}

func (m *Mutation) CreateColumnBySchemaID(ctx context.Context, tableID model.SchemaID, def model.ColumnDef) (model.Table, error) {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return model.Table{}, err
	}
	return m.CreateColumn(ctx, table.Name, def)
}

func (m *Mutation) RenameColumnBySchemaID(ctx context.Context, tableID, columnID model.SchemaID, to string) (model.Table, error) {
	table, column, err := m.columnBySchemaID(ctx, tableID, columnID)
	if err != nil {
		return model.Table{}, err
	}
	return m.RenameColumn(ctx, table.Name, column.Name, to)
}

func (m *Mutation) ChangeColumnInsertDefaultBySchemaID(
	ctx context.Context,
	tableID, columnID model.SchemaID,
	value *model.Default,
) (model.Table, error) {
	table, column, err := m.columnBySchemaID(ctx, tableID, columnID)
	if err != nil {
		return model.Table{}, err
	}
	return m.ChangeColumnInsertDefault(ctx, table.Name, column.Name, value)
}

func (m *Mutation) DeleteColumnBySchemaID(ctx context.Context, tableID, columnID model.SchemaID) (model.Table, error) {
	table, column, err := m.columnBySchemaID(ctx, tableID, columnID)
	if err != nil {
		return model.Table{}, err
	}
	return m.DeleteColumn(ctx, table.Name, column.Name)
}

func (m *Mutation) DeleteIndexBySchemaID(ctx context.Context, tableID model.SchemaID, index string) error {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return err
	}
	return m.DeleteIndex(ctx, table.Name, index)
}
