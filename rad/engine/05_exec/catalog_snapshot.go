package exec

import (
	"context"
	"fmt"

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

// CatalogSnapshot reads the catalog through an existing transaction, including
// catalog changes made by earlier programs in that transaction.
func (tx *Tx) CatalogSnapshot(ctx context.Context) (model.Revision, []model.Table, error) {
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
