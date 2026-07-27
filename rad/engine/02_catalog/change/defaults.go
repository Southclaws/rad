package change

import (
	"context"
	"slices"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// ChangeColumnInsertDefault replaces the value applied to future inserts that
// omit a column. It never changes the immutable value used to decode an absent
// physical cell in an existing row.
func (c *Service) ChangeColumnInsertDefault(
	ctx context.Context,
	tableName string,
	columnName string,
	value *model.Default,
) (model.Table, error) {
	var out model.Table
	err := c.mutate(ctx, func(change *Mutation) error {
		var err error
		out, err = change.ChangeColumnInsertDefault(ctx, tableName, columnName, value)
		return err
	})
	return out, err
}

// ChangeColumnInsertDefault changes one column's current insert policy inside
// this catalog change.
func (m *Mutation) ChangeColumnInsertDefault(
	ctx context.Context,
	tableName string,
	columnName string,
	value *model.Default,
) (model.Table, error) {
	table, ok, err := store.New(m.view).GetTable(ctx, tableName)
	if err != nil {
		return model.Table{}, err
	}
	if !ok {
		return model.Table{}, reject.Inputf("catalog: table %q does not exist", tableName)
	}
	column, ok := table.Column(columnName)
	if !ok {
		return model.Table{}, reject.Inputf(
			"catalog: column %q does not exist in table %q",
			columnName,
			tableName,
		)
	}
	if err := validateColumnDef(model.ColumnDef{
		ID:       column.SchemaID,
		Name:     column.Name,
		Type:     column.Type,
		Nullable: column.Nullable,
		Format:   column.Format,
		Default:  value,
	}); err != nil {
		return model.Table{}, err
	}
	if equalDefault(column.InsertDefault, value) {
		return table, nil
	}
	transitions, err := store.ListTransitions(ctx, m.view)
	if err != nil {
		return model.Table{}, err
	}
	for _, transition := range transitions {
		if transition.TableID != table.ID ||
			transition.Kind != model.TransitionColumnReplacement {
			continue
		}
		switch transition.State {
		case model.TransitionReady, model.TransitionFailed, model.TransitionCancelled:
			continue
		}
		affectedColumns, err := affectedColumnSchemaIDs(table, transition)
		if err != nil {
			return model.Table{}, err
		}
		if slices.Contains(affectedColumns, column.SchemaID) {
			return model.Table{}, reject.Inputf(
				"catalog: cannot change insert default for column %q during active replacement transition %q",
				columnName,
				transition.ID,
			)
		}
	}

	return m.mutateTable(ctx, tableName, func(view kv.KV, table *model.Table) error {
		for i := range table.Columns {
			if table.Columns[i].ID == column.ID {
				table.Columns[i].InsertDefault = cloneDefault(value)
				break
			}
		}
		protocol, err := store.ReadWriteProtocol(ctx, view, *table)
		if err != nil {
			return err
		}
		protocol.Generation++
		table.WriteProtocolGeneration = protocol.Generation
		return store.SaveWriteProtocol(ctx, view, protocol)
	})
}
