package api

import (
	"encoding/json"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/protocol"
)

func catTableDef(d protocol.TableDef) (model.TableDef, error) {
	def := model.TableDef{ID: model.SchemaID(d.ID), Name: d.Name, PrimaryKey: d.PrimaryKey}
	for _, column := range d.Columns {
		definition, err := catColumnDef(column)
		if err != nil {
			return model.TableDef{}, err
		}
		def.Columns = append(def.Columns, definition)
	}
	for _, index := range d.Indexes {
		def.Indexes = append(def.Indexes, model.IndexDef{
			Name: index.Name, Columns: index.Columns, Unique: index.Unique,
		})
	}
	for _, foreignKey := range d.ForeignKeys {
		def.ForeignKeys = append(def.ForeignKeys, model.ForeignKeyDef{
			Name: foreignKey.Name, Columns: foreignKey.Columns,
			RefTable: foreignKey.RefTable, RefColumns: foreignKey.RefColumns,
		})
	}
	return def, nil
}

func catColumnDef(c protocol.ColumnDef) (model.ColumnDef, error) {
	def := model.ColumnDef{
		ID: model.SchemaID(c.ID), Name: c.Name, Type: model.Type(c.Type),
		Nullable: c.Nullable, Format: c.Format,
	}
	defaultValue, err := catDefault(c.Name, def.Type, c.Default)
	if err != nil {
		return model.ColumnDef{}, err
	}
	def.Default = defaultValue
	return def, nil
}

// catDefault coerces a wire column default into the engine's typed form.
// Generator validity is the catalog's rule; literal values are coerced here
// because only the wire layer knows the incoming JSON representation.
func catDefault(colName string, typ model.Type, in *protocol.ColumnDefault) (*model.Default, error) {
	if in == nil {
		return nil, nil
	}
	if in.Func != "" && in.Value != nil {
		return nil, wireErrf("column %q: default sets both func and value", colName)
	}
	if in.Func != "" {
		return &model.Default{Func: model.DefaultFunc(in.Func)}, nil
	}
	if in.Value == nil {
		return nil, wireErrf("column %q: default must set func or value", colName)
	}
	switch typ {
	case model.TypeText:
		value, ok := in.Value.(string)
		if !ok {
			return nil, wireErrf("column %q: default expects a string, got %T", colName, in.Value)
		}
		return &model.Default{Text: value}, nil
	case model.TypeInt64:
		value, ok := in.Value.(json.Number)
		if !ok {
			return nil, wireErrf("column %q: default expects a number, got %T", colName, in.Value)
		}
		integer, err := value.Int64()
		if err != nil {
			return nil, wireErrf("column %q: default expects an integer, got %v", colName, value)
		}
		return &model.Default{Int64: integer}, nil
	case model.TypeFloat64:
		value, ok := in.Value.(json.Number)
		if !ok {
			return nil, wireErrf("column %q: default expects a number, got %T", colName, in.Value)
		}
		floating, err := value.Float64()
		if err != nil {
			return nil, wireErrf("column %q: default expects a float, got %v", colName, value)
		}
		return &model.Default{Float64: floating}, nil
	case model.TypeBool:
		value, ok := in.Value.(bool)
		if !ok {
			return nil, wireErrf("column %q: default expects a boolean, got %T", colName, in.Value)
		}
		return &model.Default{Bool: value}, nil
	}
	return nil, wireErrf("column %q has unsupported type %q", colName, typ)
}
