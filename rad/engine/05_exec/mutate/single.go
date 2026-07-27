package mutate

import (
	"context"
	"maps"
	"slices"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/rowstore"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func CreateOne(ctx context.Context, view kv.KV, table model.Table, row lir.Row) (lir.Row, error) {
	rows, err := Create(ctx, view, table, []lir.Row{row})
	if err != nil {
		return nil, err
	}
	return rows[0], nil
}

func UpdateOne(ctx context.Context, view kv.KV, table model.Table, key, set lir.Row) (lir.Row, bool, error) {
	current, pk, ok, err := rowstore.Get(ctx, view, table, key)
	if err != nil || !ok {
		return nil, false, err
	}
	for name := range set {
		if _, ok := table.Column(name); !ok {
			return nil, false, reject.Inputf("exec: table %q has no column %q", table.Name, name)
		}
		if slices.Contains(table.PrimaryKey, name) {
			return nil, false, reject.Inputf("exec: cannot update primary key column %q", name)
		}
	}
	merged := maps.Clone(current)
	maps.Copy(merged, set)
	stored, err := normalize(table, merged)
	if err != nil {
		return nil, false, err
	}
	if err := checkForeignKeysFor(ctx, view, table, stored, set); err != nil {
		return nil, false, err
	}
	if err := checkUniqueIndexesFor(ctx, view, table, stored, pk, set); err != nil {
		return nil, false, err
	}
	if err := rowstore.Replace(ctx, view, table, current, stored, pk); err != nil {
		return nil, false, err
	}
	return stored, true, nil
}

func DeleteOne(ctx context.Context, view kv.KV, table model.Table, key lir.Row) (bool, error) {
	current, pk, ok, err := rowstore.Get(ctx, view, table, key)
	if err != nil || !ok {
		return false, err
	}
	if err := checkNoReferences(ctx, view, table, current); err != nil {
		return false, err
	}
	if err := rowstore.Delete(ctx, view, table, current, pk); err != nil {
		return false, err
	}
	return true, nil
}
