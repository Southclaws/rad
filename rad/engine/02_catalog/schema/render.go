package schema

import (
	"fmt"

	yaml "github.com/goccy/go-yaml"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

type renderedSchema struct {
	Tables []renderedTable `yaml:"tables"`
}

type renderedTable struct {
	ID          model.SchemaID   `yaml:"id"`
	Name        string           `yaml:"name"`
	Columns     []renderedColumn `yaml:"columns"`
	PrimaryKey  []string         `yaml:"primary_key"`
	Indexes     []fileIndex      `yaml:"indexes,omitempty"`
	ForeignKeys []fileForeignKey `yaml:"foreign_keys,omitempty"`
}

type renderedColumn struct {
	ID       model.SchemaID `yaml:"id"`
	Name     string         `yaml:"name"`
	Type     string         `yaml:"type"`
	Nullable bool           `yaml:"nullable,omitempty"`
	Format   string         `yaml:"format,omitempty"`
	Default  any            `yaml:"default,omitempty"`
}

// Render returns the canonical schema as a pure, parseable rad.schema.yaml
// document. It deliberately emits the full forms for indexes and foreign
// keys so a schema reconstructed from a Direct-mode catalog round-trips
// without inventing names.
func Render(canonical model.Schema) ([]byte, error) {
	out := renderedSchema{Tables: make([]renderedTable, 0, len(canonical.Tables))}
	for _, table := range canonical.Tables {
		rendered := renderedTable{
			ID: table.ID, Name: table.Name, PrimaryKey: table.PrimaryKey,
		}
		for _, column := range table.Columns {
			typeName, err := schemaType(column.Type)
			if err != nil {
				return nil, err
			}
			value, err := schemaDefault(column)
			if err != nil {
				return nil, err
			}
			rendered.Columns = append(rendered.Columns, renderedColumn{
				ID: column.ID, Name: column.Name, Type: typeName, Nullable: column.Nullable,
				Format: column.Format, Default: value,
			})
		}
		for _, index := range table.Indexes {
			rendered.Indexes = append(rendered.Indexes, fileIndex{
				Name: index.Name, Columns: index.Columns, Unique: index.Unique,
			})
		}
		for _, foreignKey := range table.ForeignKeys {
			rendered.ForeignKeys = append(rendered.ForeignKeys, fileForeignKey{
				Name: foreignKey.Name, Columns: foreignKey.Columns,
				RefTable: foreignKey.RefTable, RefColumns: foreignKey.RefColumns,
			})
		}
		out.Tables = append(out.Tables, rendered)
	}
	return yaml.Marshal(out)
}

func schemaType(typ model.Type) (string, error) {
	switch typ {
	case model.TypeText:
		return "string", nil
	case model.TypeInt64, model.TypeFloat64, model.TypeBool:
		return string(typ), nil
	default:
		return "", fmt.Errorf("schema: cannot render column type %q", typ)
	}
}

func schemaDefault(column model.ColumnDef) (any, error) {
	if column.Default == nil {
		return nil, nil
	}
	switch column.Default.Func {
	case model.DefaultUUID:
		return "uuid()", nil
	case model.DefaultNowMS:
		return "now_ms()", nil
	case "":
	default:
		return nil, fmt.Errorf("schema: cannot render default function %q", column.Default.Func)
	}
	switch column.Type {
	case model.TypeText:
		return column.Default.Text, nil
	case model.TypeInt64:
		return column.Default.Int64, nil
	case model.TypeFloat64:
		return column.Default.Float64, nil
	case model.TypeBool:
		return column.Default.Bool, nil
	default:
		return nil, fmt.Errorf("schema: cannot render default for type %q", column.Type)
	}
}
