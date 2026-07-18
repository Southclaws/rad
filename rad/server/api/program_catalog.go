package api

import (
	"bytes"
	"encoding/json"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

func schemaID(id pirwire.SchemaID) model.SchemaID {
	return model.SchemaID(id)
}

func optionalSchemaID(id *pirwire.SchemaID) model.SchemaID {
	if id == nil {
		return 0
	}
	return schemaID(*id)
}

func pirTableDef(in pirwire.TableDefinition) (model.TableDef, error) {
	out := model.TableDef{
		ID: optionalSchemaID(in.ID), Name: in.Name, PrimaryKey: in.PrimaryKey,
	}
	for _, column := range in.Columns {
		definition, err := pirColumnDef(column)
		if err != nil {
			return model.TableDef{}, err
		}
		out.Columns = append(out.Columns, definition)
	}
	for _, index := range in.Indexes {
		out.Indexes = append(out.Indexes, pirIndexDef(index))
	}
	for _, foreignKey := range in.ForeignKeys {
		out.ForeignKeys = append(out.ForeignKeys, model.ForeignKeyDef{
			Name: foreignKey.Name, Columns: foreignKey.Columns,
			RefTable: foreignKey.RefTable, RefColumns: foreignKey.RefColumns,
		})
	}
	return out, nil
}

func pirColumnDef(in pirwire.ColumnDefinition) (model.ColumnDef, error) {
	out := model.ColumnDef{
		ID: optionalSchemaID(in.ID), Name: in.Name, Type: model.Type(in.Type),
	}
	if in.Nullable != nil {
		out.Nullable = *in.Nullable
	}
	if in.Format != nil {
		out.Format = *in.Format
	}
	defaultValue, err := pirDefault(in.Name, out.Type, in.Default)
	if err != nil {
		return model.ColumnDef{}, err
	}
	out.Default = defaultValue
	return out, nil
}

func pirDefault(name string, typ model.Type, in *pirwire.ColumnDefault) (*model.Default, error) {
	if in == nil || in.ColumnDefaultUnion == nil {
		return nil, nil
	}
	switch value := in.ColumnDefaultUnion.(type) {
	case *pirwire.GeneratorDefault:
		return catDefault(name, typ, &protocol.ColumnDefault{Func: value.Func})
	case *pirwire.LiteralDefault:
		decoder := json.NewDecoder(bytes.NewReader(value.Value))
		decoder.UseNumber()
		var literal any
		if err := decoder.Decode(&literal); err != nil {
			return nil, wireErrf("column %q: decode literal default: %v", name, err)
		}
		return catDefault(name, typ, &protocol.ColumnDefault{Value: literal})
	default:
		return nil, wireErrf("column %q: unknown default variant %T", name, in.ColumnDefaultUnion)
	}
}

func pirIndexDef(in pirwire.IndexDefinition) model.IndexDef {
	out := model.IndexDef{Name: in.Name, Columns: in.Columns}
	if in.Unique != nil {
		out.Unique = *in.Unique
	}
	return out
}
