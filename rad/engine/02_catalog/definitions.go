package catalog

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func validateDefault(column ColumnDef) error {
	if column.Default == nil {
		return nil
	}
	switch column.Default.Func {
	case "":
		return nil
	case DefaultUUID:
		if column.Type != TypeText {
			return reject.Inputf("catalog: column %q: uuid() default requires a string column", column.Name)
		}
	case DefaultNowMS:
		if column.Type != TypeInt64 {
			return reject.Inputf("catalog: column %q: now_ms() default requires an int64 column", column.Name)
		}
	default:
		return reject.Inputf("catalog: column %q: unknown default function %q", column.Name, column.Default.Func)
	}
	return nil
}

func validateColumnDef(column ColumnDef) error {
	switch column.Type {
	case TypeText, TypeInt64, TypeFloat64, TypeBool:
		return validateDefault(column)
	default:
		return reject.Inputf("catalog: column %q has unsupported type %q", column.Name, column.Type)
	}
}

func buildColumn(ctx context.Context, view kv.KV, def ColumnDef) (Column, error) {
	id, err := nextID(ctx, view, "c")
	if err != nil {
		return Column{}, err
	}
	return Column{
		ID: id, SchemaID: def.ID, Name: def.Name, Type: def.Type, Nullable: def.Nullable,
		Format: def.Format, Default: def.Default,
	}, nil
}

func buildIndex(ctx context.Context, view kv.KV, table Table, def IndexDef) (Index, error) {
	if len(def.Columns) == 0 {
		return Index{}, reject.Inputf("catalog: index %q has no columns", def.Name)
	}
	for _, column := range def.Columns {
		if _, ok := table.Column(column); !ok {
			return Index{}, reject.Inputf("catalog: index %q references unknown column %q", def.Name, column)
		}
	}
	id, err := nextID(ctx, view, "i")
	if err != nil {
		return Index{}, err
	}
	return Index{ID: id, Name: def.Name, Columns: def.Columns, Unique: def.Unique}, nil
}
