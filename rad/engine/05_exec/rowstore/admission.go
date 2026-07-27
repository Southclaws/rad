package rowstore

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
)

// admitTable reads only the compatibility fences for the pinned physical
// definitions execution actually decodes. Current scans decode complete rows,
// so every pinned column is a dependency until projection-aware row decoding
// can narrow this set.
func admitTable(ctx context.Context, view kv.KV, table model.Table) error {
	if err := store.ReadTableExistenceFence(ctx, view, table); err != nil {
		return err
	}
	for _, column := range table.Columns {
		if err := store.ReadColumnValueFence(ctx, view, table, column); err != nil {
			return err
		}
	}
	return nil
}
