package api

import (
	"encoding/json"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/protocol"
)

func catTableDef(d protocol.TableDef) (catalog.TableDef, error) {
	def := catalog.TableDef{ID: catalog.SchemaID(d.ID), Name: d.Name, PrimaryKey: d.PrimaryKey}
	for _, column := range d.Columns {
		definition, err := catColumnDef(column)
		if err != nil {
			return catalog.TableDef{}, err
		}
		def.Columns = append(def.Columns, definition)
	}
	for _, index := range d.Indexes {
		def.Indexes = append(def.Indexes, catalog.IndexDef{
			Name: index.Name, Columns: index.Columns, Unique: index.Unique,
		})
	}
	for _, foreignKey := range d.ForeignKeys {
		def.ForeignKeys = append(def.ForeignKeys, catalog.ForeignKeyDef{
			Name: foreignKey.Name, Columns: foreignKey.Columns,
			RefTable: foreignKey.RefTable, RefColumns: foreignKey.RefColumns,
		})
	}
	return def, nil
}

func catColumnDef(c protocol.ColumnDef) (catalog.ColumnDef, error) {
	def := catalog.ColumnDef{
		ID: catalog.SchemaID(c.ID), Name: c.Name, Type: catalog.Type(c.Type),
		Nullable: c.Nullable, Format: c.Format,
	}
	defaultValue, err := catDefault(c.Name, def.Type, c.Default)
	if err != nil {
		return catalog.ColumnDef{}, err
	}
	def.Default = defaultValue
	return def, nil
}

// catDefault coerces a wire column default into the engine's typed form.
// Generator validity is the catalog's rule; literal values are coerced here
// because only the wire layer knows the incoming JSON representation.
func catDefault(colName string, typ catalog.Type, in *protocol.ColumnDefault) (*catalog.Default, error) {
	if in == nil {
		return nil, nil
	}
	if in.Func != "" && in.Value != nil {
		return nil, wireErrf("column %q: default sets both func and value", colName)
	}
	if in.Func != "" {
		return &catalog.Default{Func: catalog.DefaultFunc(in.Func)}, nil
	}
	if in.Value == nil {
		return nil, wireErrf("column %q: default must set func or value", colName)
	}
	switch typ {
	case catalog.TypeText:
		value, ok := in.Value.(string)
		if !ok {
			return nil, wireErrf("column %q: default expects a string, got %T", colName, in.Value)
		}
		return &catalog.Default{Text: value}, nil
	case catalog.TypeInt64:
		value, ok := in.Value.(json.Number)
		if !ok {
			return nil, wireErrf("column %q: default expects a number, got %T", colName, in.Value)
		}
		integer, err := value.Int64()
		if err != nil {
			return nil, wireErrf("column %q: default expects an integer, got %v", colName, value)
		}
		return &catalog.Default{Int64: integer}, nil
	case catalog.TypeFloat64:
		value, ok := in.Value.(json.Number)
		if !ok {
			return nil, wireErrf("column %q: default expects a number, got %T", colName, in.Value)
		}
		floating, err := value.Float64()
		if err != nil {
			return nil, wireErrf("column %q: default expects a float, got %v", colName, value)
		}
		return &catalog.Default{Float64: floating}, nil
	case catalog.TypeBool:
		value, ok := in.Value.(bool)
		if !ok {
			return nil, wireErrf("column %q: default expects a boolean, got %T", colName, in.Value)
		}
		return &catalog.Default{Bool: value}, nil
	}
	return nil, wireErrf("column %q has unsupported type %q", colName, typ)
}
