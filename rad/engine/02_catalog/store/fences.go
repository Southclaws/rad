package store

import (
	"context"
	"fmt"
	"strconv"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

const (
	tableExistenceFencePrefix = "/rad/catalog/fence/table_existence/"
	columnValueFencePrefix    = "/rad/catalog/fence/column_value/"
	indexAccessFencePrefix    = "/rad/catalog/fence/index_access/"
)

// AdmitCatalogDependencies joins a bound plan's exact semantic dependencies
// to the Slate transaction's read set before execution. A later incompatible
// catalog publication advances one of these keys and therefore conflicts at
// commit; compatible publications leave them untouched.
func AdmitCatalogDependencies(ctx context.Context, view kv.KV, dependencies model.CatalogDependencies) error {
	for _, dependency := range dependencies.TableExistence {
		if err := readGenerationFence(
			ctx, view, TableExistenceFenceKey(dependency.TableID), dependency.Generation,
			fmt.Sprintf("existence fence for table %q", dependency.TableName),
			fmt.Sprintf("catalog: table %q existence", dependency.TableName),
		); err != nil {
			return err
		}
	}
	for _, dependency := range dependencies.ColumnValues {
		if err := readGenerationFence(
			ctx, view, ColumnValueFenceKey(dependency.TableID, dependency.ColumnID), dependency.Generation,
			fmt.Sprintf("value fence for column %q.%q", dependency.TableName, dependency.ColumnName),
			fmt.Sprintf("catalog: column %q.%q value definition", dependency.TableName, dependency.ColumnName),
		); err != nil {
			return err
		}
	}
	for _, dependency := range dependencies.IndexAccess {
		if err := readGenerationFence(
			ctx, view, IndexAccessFenceKey(dependency.TableID, dependency.IndexID), dependency.Generation,
			fmt.Sprintf("access fence for index %q on table %q", dependency.IndexName, dependency.TableName),
			fmt.Sprintf("catalog: index %q on table %q access", dependency.IndexName, dependency.TableName),
		); err != nil {
			return err
		}
	}
	for _, dependency := range dependencies.WriteProtocols {
		if err := readGenerationFence(
			ctx, view, WriteProtocolKey(dependency.TableID), dependency.Generation,
			fmt.Sprintf("write protocol fence for table %q", dependency.TableName),
			fmt.Sprintf("catalog: table %q write protocol", dependency.TableName),
		); err != nil {
			return err
		}
	}
	return nil
}

func readGenerationFence(
	ctx context.Context,
	view kv.KV,
	key []byte,
	expected uint64,
	label string,
	changed string,
) error {
	raw, ok, err := view.Get(ctx, key)
	if err != nil {
		return err
	}
	actual := uint64(0)
	if ok {
		actual, err = strconv.ParseUint(string(raw), 10, 64)
		if err != nil {
			return fmt.Errorf("catalog: corrupt %s: %w", label, err)
		}
	}
	if actual != expected {
		return fmt.Errorf("%s changed from generation %d to %d: %w", changed, expected, actual, kv.ErrConflict)
	}
	return nil
}

func TableExistenceFenceKey(tableID string) []byte {
	return []byte(tableExistenceFencePrefix + tableID)
}

// ReadTableExistenceFence admits work bound to an immutable table definition.
// The read joins the data transaction's serializable read set, so a logical
// delete racing commit either happens wholly before the work or conflicts it.
func ReadTableExistenceFence(ctx context.Context, view kv.KV, table model.Table) error {
	return readGenerationFence(
		ctx, view, TableExistenceFenceKey(table.ID), table.ExistenceGeneration,
		fmt.Sprintf("existence fence for table %q", table.Name),
		fmt.Sprintf("catalog: table %q existence", table.Name),
	)
}

func AdvanceTableExistenceFence(ctx context.Context, view kv.KV, table model.Table) error {
	if err := ReadTableExistenceFence(ctx, view, table); err != nil {
		return err
	}
	next := table.ExistenceGeneration + 1
	return view.Put(ctx, TableExistenceFenceKey(table.ID), []byte(strconv.FormatUint(next, 10)))
}

func ColumnValueFenceKey(tableID, columnID string) []byte {
	return []byte(columnValueFencePrefix + tableID + "/" + columnID)
}

func ReadColumnValueFence(ctx context.Context, view kv.KV, table model.Table, column model.Column) error {
	return readGenerationFence(
		ctx, view, ColumnValueFenceKey(table.ID, column.ID), column.ValueGeneration,
		fmt.Sprintf("value fence for column %q.%q", table.Name, column.Name),
		fmt.Sprintf("catalog: column %q.%q value definition", table.Name, column.Name),
	)
}

func AdvanceColumnValueFence(ctx context.Context, view kv.KV, table model.Table, column model.Column) error {
	if err := ReadColumnValueFence(ctx, view, table, column); err != nil {
		return err
	}
	next := column.ValueGeneration + 1
	return view.Put(ctx, ColumnValueFenceKey(table.ID, column.ID), []byte(strconv.FormatUint(next, 10)))
}

func IndexAccessFenceKey(tableID, indexID string) []byte {
	return []byte(indexAccessFencePrefix + tableID + "/" + indexID)
}

// ReadIndexAccessFence prevents a plan bound before logical index retirement
// from beginning physical access after reclamation may have removed the
// index's current keys. A transaction whose Slate snapshot predates retirement
// continues to see the old fence and old index versions coherently.
func ReadIndexAccessFence(ctx context.Context, view kv.KV, table model.Table, index model.Index) error {
	return readGenerationFence(
		ctx, view, IndexAccessFenceKey(table.ID, index.ID), index.AccessGeneration,
		fmt.Sprintf("access fence for index %q on table %q", index.Name, table.Name),
		fmt.Sprintf("catalog: index %q on table %q access", index.Name, table.Name),
	)
}

func AdvanceIndexAccessFence(ctx context.Context, view kv.KV, table model.Table, index model.Index) error {
	if err := ReadIndexAccessFence(ctx, view, table, index); err != nil {
		return err
	}
	next := index.AccessGeneration + 1
	return view.Put(ctx, IndexAccessFenceKey(table.ID, index.ID), []byte(strconv.FormatUint(next, 10)))
}
