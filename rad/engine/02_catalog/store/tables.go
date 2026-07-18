// Package store persists and retrieves catalog models from a KV view.
package store

import (
	"context"
	"encoding/json"
	"fmt"
	"slices"
	"strconv"
	"strings"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/reject"
)

const (
	nextIDKey       = "/rad/catalog/meta/next_id"
	tablePrefix     = "/rad/catalog/table/"
	tableNamePrefix = "/rad/catalog/table_name/"
)

type Reader struct {
	view kv.KV
}

func New(view kv.KV) Reader { return Reader{view: view} }

func TableKey(id string) []byte       { return []byte(tablePrefix + id) }
func TableNameKey(name string) []byte { return []byte(tableNamePrefix + name) }

func (r Reader) GetTable(ctx context.Context, name string) (model.Table, bool, error) {
	id, ok, err := r.view.Get(ctx, TableNameKey(name))
	if err != nil || !ok {
		return model.Table{}, false, err
	}
	return r.GetTableByID(ctx, string(id))
}

func (r Reader) GetTableByID(ctx context.Context, id string) (model.Table, bool, error) {
	raw, ok, err := r.view.Get(ctx, TableKey(id))
	if err != nil || !ok {
		return model.Table{}, false, err
	}
	var table model.Table
	if err := json.Unmarshal(raw, &table); err != nil {
		return model.Table{}, false, err
	}
	return table, true, nil
}

func (r Reader) GetTableBySchemaID(ctx context.Context, id model.SchemaID) (model.Table, bool, error) {
	if id == 0 || id > model.MaxSchemaID {
		return model.Table{}, false, reject.Inputf("catalog: invalid table schema ID %d", id)
	}
	tables, err := r.ListTables(ctx)
	if err != nil {
		return model.Table{}, false, err
	}
	var found model.Table
	have := false
	for _, table := range tables {
		if table.SchemaID != id {
			continue
		}
		if have {
			return model.Table{}, false, reject.Fail(reject.ReasonCatalogDrift,
				"catalog: tables %q and %q share schema ID %d", found.Name, table.Name, id)
		}
		found, have = table, true
	}
	return found, have, nil
}

func (r Reader) ListTables(ctx context.Context) ([]model.Table, error) {
	prefix := []byte(tablePrefix)
	iterator, err := r.view.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		return nil, err
	}
	defer iterator.Close()

	var tables []model.Table
	for iterator.Next() {
		var table model.Table
		if err := json.Unmarshal(iterator.Value(), &table); err != nil {
			return nil, fmt.Errorf("catalog: corrupt table entry %q: %w", iterator.Key(), err)
		}
		tables = append(tables, table)
	}
	if err := iterator.Err(); err != nil {
		return nil, err
	}
	slices.SortFunc(tables, func(a, b model.Table) int { return strings.Compare(a.Name, b.Name) })
	return tables, nil
}

func (r Reader) Schema(ctx context.Context) (model.Schema, error) {
	tables, err := r.ListTables(ctx)
	if err != nil {
		return model.Schema{}, err
	}
	return model.BuildSchema(tables)
}

func SaveTable(ctx context.Context, view kv.KV, table model.Table) error {
	raw, err := json.Marshal(table)
	if err != nil {
		return err
	}
	return view.Put(ctx, TableKey(table.ID), raw)
}

func NextPhysicalID(ctx context.Context, view kv.KV, kind string) (string, error) {
	var next uint64
	raw, ok, err := view.Get(ctx, []byte(nextIDKey))
	if err != nil {
		return "", err
	}
	if ok {
		next, err = strconv.ParseUint(string(raw), 10, 64)
		if err != nil {
			return "", fmt.Errorf("catalog: corrupt next_id %q: %w", raw, err)
		}
	}
	next++
	if err := view.Put(ctx, []byte(nextIDKey), []byte(strconv.FormatUint(next, 10))); err != nil {
		return "", err
	}
	return fmt.Sprintf("%s%d", kind, next), nil
}
