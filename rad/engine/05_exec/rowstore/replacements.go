package rowstore

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func applyColumnReplacementWrites(
	ctx context.Context,
	view kv.KV,
	table model.Table,
	protocol model.WriteProtocol,
	row lir.Row,
	pk []byte,
	raw []byte,
) ([]byte, error) {
	var err error
	for _, obligation := range protocol.ColumnReplacements {
		sourceName, ok := physicalColumnName(table, obligation.Replacement.Source.ID)
		if !ok {
			return nil, reject.Fail(
				reject.ReasonCatalogDrift,
				"exec: replacement transition %q source column %q is not active on table %q",
				obligation.TransitionID,
				obligation.Replacement.Source.ID,
				table.Name,
			)
		}
		converted, conversionErr := codec.ConvertColumnValue(
			row[sourceName],
			obligation.Replacement.Target,
			obligation.Replacement.Conversion,
		)
		if conversionErr != nil {
			return nil, reject.Fail(
				reject.ReasonConstraintViolation,
				"exec: replacement transition %q cannot convert column %q: %v",
				obligation.TransitionID,
				sourceName,
				conversionErr,
			)
		}
		raw, err = codec.SetColumnValue(raw, obligation.Replacement.Target, converted)
		if err != nil {
			return nil, err
		}
		if err := store.DeleteTransitionViolation(ctx, view, obligation.TransitionID, pk); err != nil {
			return nil, err
		}
	}
	return raw, nil
}

func clearColumnReplacementViolations(
	ctx context.Context,
	view kv.KV,
	protocol model.WriteProtocol,
	pk []byte,
) error {
	for _, obligation := range protocol.ColumnReplacements {
		if err := store.DeleteTransitionViolation(ctx, view, obligation.TransitionID, pk); err != nil {
			return err
		}
	}
	return nil
}

func physicalColumnName(table model.Table, physicalID string) (string, bool) {
	for _, column := range table.Columns {
		if column.ID == physicalID {
			return column.Name, true
		}
	}
	return "", false
}
