package exec

import (
	"context"
	"fmt"
	"slices"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// CatalogSnapshot reads the committed revision and physical tables through
// one transaction snapshot and rejects any drift between them.
func (e *Engine) CatalogSnapshot(ctx context.Context) (model.Revision, []model.Table, error) {
	tx, err := e.Begin(ctx)
	if err != nil {
		return model.Revision{}, nil, err
	}
	defer tx.Rollback()
	return tx.CatalogSnapshot(ctx)
}

// SchemaMigrationSnapshot reads the revision, physical definitions, and
// durable transition records through one storage snapshot. Migration planning
// uses this instead of racing a catalog snapshot against a later transition
// listing when recovering an in-flight desired-schema apply.
func (e *Engine) SchemaMigrationSnapshot(ctx context.Context) (
	model.Revision,
	[]model.Table,
	[]model.SchemaTransition,
	error,
) {
	txn, err := e.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return model.Revision{}, nil, nil, err
	}
	defer txn.Rollback()
	revision, err := store.CurrentRevision(ctx, txn)
	if err != nil {
		return model.Revision{}, nil, nil, err
	}
	tables, err := store.New(txn).ListTables(ctx)
	if err != nil {
		return model.Revision{}, nil, nil, err
	}
	physical, err := model.BuildSchema(tables)
	if err != nil {
		return model.Revision{}, nil, nil, err
	}
	equal, err := revision.Schema.Equal(physical)
	if err != nil {
		return model.Revision{}, nil, nil, err
	}
	if !equal {
		return model.Revision{}, nil, nil, reject.Mark(reject.ReasonCatalogDrift,
			fmt.Errorf("catalog: stored schema revision %d differs from physical catalog", revision.Version))
	}
	transitions, err := store.ListTransitions(ctx, txn)
	if err != nil {
		return model.Revision{}, nil, nil, err
	}
	return revision, tables, transitions, nil
}

// CatalogSnapshot reads the catalog through an existing transaction, including
// catalog changes made by earlier programs in that transaction.
func (tx *Tx) CatalogSnapshot(ctx context.Context) (model.Revision, []model.Table, error) {
	if !tx.catalogDirty {
		return tx.catalog.revision, slices.Clone(tx.catalog.tables), nil
	}
	reader := store.New(tx.txn)
	revision, err := reader.Revision(ctx)
	if err != nil {
		return model.Revision{}, nil, err
	}
	tables, err := reader.ListTables(ctx)
	if err != nil {
		return model.Revision{}, nil, err
	}
	physical, err := model.BuildSchema(tables)
	if err != nil {
		return model.Revision{}, nil, err
	}
	equal, err := revision.Schema.Equal(physical)
	if err != nil {
		return model.Revision{}, nil, err
	}
	if !equal {
		return model.Revision{}, nil, reject.Mark(reject.ReasonCatalogDrift,
			fmt.Errorf("catalog: stored schema revision %d differs from physical catalog", revision.Version))
	}
	return revision, tables, nil
}

// catalogSnapshot is the coherent immutable catalog pinned before a data
// transaction begins. Bound execution carries these physical definitions and
// never needs to reread mutable name heads or current table blobs.
type catalogSnapshot struct {
	revision model.Revision
	tables   []model.Table
	byName   map[string]model.Table
}

func pinCatalog(ctx context.Context, database kv.TransactionalKV) (*catalogSnapshot, error) {
	txn, err := database.Begin(ctx, kv.Snapshot)
	if err != nil {
		return nil, err
	}
	defer txn.Rollback()
	revision, err := store.CurrentRevision(ctx, txn)
	if err != nil {
		return nil, err
	}
	tables, err := store.New(txn).ListTables(ctx)
	if err != nil {
		return nil, err
	}
	pinned := &catalogSnapshot{
		revision: revision,
		tables:   slices.Clone(tables),
		byName:   make(map[string]model.Table, len(tables)),
	}
	for _, table := range tables {
		pinned.byName[table.Name] = table
	}
	return pinned, nil
}

func (c *catalogSnapshot) GetTable(_ context.Context, name string) (model.Table, bool, error) {
	table, ok := c.byName[name]
	return table, ok, nil
}

// txCatalog switches to the transaction's mutable catalog only after that
// same transaction performs catalog work. This preserves PIR's rule that a
// later statement sees preceding DDL without making ordinary data execution
// depend on mutable catalog records.
type txCatalog struct{ tx *Tx }

func (c txCatalog) GetTable(ctx context.Context, name string) (model.Table, bool, error) {
	if c.tx.catalogDirty {
		return store.New(c.tx.txn).GetTable(ctx, name)
	}
	return c.tx.catalog.GetTable(ctx, name)
}

func (tx *Tx) table(ctx context.Context, name string) (model.Table, error) {
	table, ok, err := txCatalog{tx: tx}.GetTable(ctx, name)
	if err != nil {
		return model.Table{}, err
	}
	if !ok {
		return model.Table{}, reject.Inputf("exec: table %q does not exist", name)
	}
	return table, nil
}

func (tx *Tx) pinnedCatalogVersion() uint64 {
	if tx.catalog == nil {
		return 0
	}
	return tx.catalog.revision.Version
}

func (tx *Tx) markCatalogChanged() error {
	if tx.catalogDirty {
		return nil
	}
	if tx.catalog == nil {
		return fmt.Errorf("exec: transaction has no pinned catalog")
	}
	tx.catalogDirty = true
	return nil
}
