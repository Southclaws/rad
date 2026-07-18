package change

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func validateDefault(column model.ColumnDef) error {
	if column.Default == nil {
		return nil
	}
	switch column.Default.Func {
	case "":
		return nil
	case model.DefaultUUID:
		if column.Type != model.TypeText {
			return reject.Inputf("catalog: column %q: uuid() default requires a string column", column.Name)
		}
	case model.DefaultNowMS:
		if column.Type != model.TypeInt64 {
			return reject.Inputf("catalog: column %q: now_ms() default requires an int64 column", column.Name)
		}
	default:
		return reject.Inputf("catalog: column %q: unknown default function %q", column.Name, column.Default.Func)
	}
	return nil
}

func validateColumnDef(column model.ColumnDef) error {
	switch column.Type {
	case model.TypeText, model.TypeInt64, model.TypeFloat64, model.TypeBool:
		return validateDefault(column)
	default:
		return reject.Inputf("catalog: column %q has unsupported type %q", column.Name, column.Type)
	}
}

func buildColumn(ctx context.Context, view kv.KV, def model.ColumnDef) (model.Column, error) {
	id, err := store.NextPhysicalID(ctx, view, "c")
	if err != nil {
		return model.Column{}, err
	}
	return model.Column{
		ID: id, SchemaID: def.ID, Name: def.Name, Type: def.Type, Nullable: def.Nullable,
		Format: def.Format, Default: def.Default,
	}, nil
}

func buildIndex(ctx context.Context, view kv.KV, table model.Table, def model.IndexDef) (model.Index, error) {
	if len(def.Columns) == 0 {
		return model.Index{}, reject.Inputf("catalog: index %q has no columns", def.Name)
	}
	for _, column := range def.Columns {
		if _, ok := table.Column(column); !ok {
			return model.Index{}, reject.Inputf("catalog: index %q references unknown column %q", def.Name, column)
		}
	}
	id, err := store.NextPhysicalID(ctx, view, "i")
	if err != nil {
		return model.Index{}, err
	}
	return model.Index{ID: id, Name: def.Name, Columns: def.Columns, Unique: def.Unique}, nil
}
