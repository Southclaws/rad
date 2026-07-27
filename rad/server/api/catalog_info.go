package api

import (
	"context"

	"github.com/Southclaws/rad/rad/api"
	"github.com/Southclaws/rad/rad/api/oas"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/protocol"
)

// defaultInfo renders a column default for introspection; the column's type
// selects which literal field carries the value.
func defaultInfo(column model.Column) *protocol.ColumnDefault {
	defaultValue := column.InsertDefault
	if defaultValue == nil {
		return nil
	}
	if defaultValue.Func != "" {
		return &protocol.ColumnDefault{Func: string(defaultValue.Func)}
	}
	out := &protocol.ColumnDefault{}
	switch column.Type {
	case model.TypeText:
		out.Value = defaultValue.Text
	case model.TypeInt64:
		out.Value = defaultValue.Int64
	case model.TypeFloat64:
		out.Value = defaultValue.Float64
	case model.TypeBool:
		out.Value = defaultValue.Bool
	}
	return out
}

// tableInfo renders one table for introspection, resolving foreign-key
// physical table IDs back to names.
func (a *dbAPI) tableInfo(ctx context.Context, table model.Table) (protocol.TableInfo, error) {
	info := protocol.TableInfo{
		ID: uint32(table.SchemaID), Name: table.Name, PrimaryKey: table.PrimaryKey,
	}
	for _, column := range table.Columns {
		info.Columns = append(info.Columns, protocol.ColumnInfo{
			ID: uint32(column.SchemaID), Name: column.Name, Type: string(column.Type),
			Nullable: column.Nullable, Format: column.Format, Default: defaultInfo(column),
		})
	}
	for _, index := range table.Indexes {
		info.Indexes = append(info.Indexes, protocol.IndexDef{
			Name: index.Name, Columns: index.Columns, Unique: index.Unique,
		})
	}
	for _, foreignKey := range table.ForeignKeys {
		refName := table.Name
		if foreignKey.RefTableID != table.ID {
			ref, ok, err := a.cat.GetTableByID(ctx, foreignKey.RefTableID)
			if err != nil {
				return protocol.TableInfo{}, err
			}
			if ok {
				refName = ref.Name
			} else {
				refName = foreignKey.RefTableID
			}
		}
		info.ForeignKeys = append(info.ForeignKeys, protocol.ForeignKeyDef{
			Name: foreignKey.Name, Columns: foreignKey.Columns,
			RefTable: refName, RefColumns: foreignKey.RefColumns,
		})
	}
	return info, nil
}

func (a *dbAPI) tableOAS(ctx context.Context, table model.Table) (*oas.TableInfo, error) {
	info, err := a.tableInfo(ctx, table)
	if err != nil {
		return nil, err
	}
	out := api.TableToOAS(info)
	return &out, nil
}

func schemaDocument(canonical model.Schema) oas.SchemaDocument {
	tables := make([]oas.TableDef, len(canonical.Tables))
	for i, table := range canonical.Tables {
		definition := protocol.TableDef{
			ID: uint32(table.ID), Name: table.Name, PrimaryKey: table.PrimaryKey,
		}
		for _, column := range table.Columns {
			definition.Columns = append(definition.Columns, protocol.ColumnDef{
				ID: uint32(column.ID), Name: column.Name, Type: string(column.Type),
				Nullable: column.Nullable, Format: column.Format,
				Default: defaultInfo(model.Column{Type: column.Type, InsertDefault: column.Default}),
			})
		}
		for _, index := range table.Indexes {
			definition.Indexes = append(definition.Indexes, protocol.IndexDef{
				Name: index.Name, Columns: index.Columns, Unique: index.Unique,
			})
		}
		for _, foreignKey := range table.ForeignKeys {
			definition.ForeignKeys = append(definition.ForeignKeys, protocol.ForeignKeyDef{
				Name: foreignKey.Name, Columns: foreignKey.Columns,
				RefTable: foreignKey.RefTable, RefColumns: foreignKey.RefColumns,
			})
		}
		tables[i] = api.TableDefToOAS(definition)
	}
	return oas.SchemaDocument{Tables: tables}
}
