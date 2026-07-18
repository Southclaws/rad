package exec

import (
	"context"
	"fmt"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// CatalogSnapshot reads the committed revision and physical tables through
// one transaction snapshot and rejects any drift between them.
func (e *Engine) CatalogSnapshot(ctx context.Context) (catalog.Revision, []catalog.Table, error) {
	tx, err := e.Begin(ctx)
	if err != nil {
		return catalog.Revision{}, nil, err
	}
	defer tx.Rollback()
	reader := catalog.NewReader(tx.txn)
	revision, err := reader.Revision(ctx)
	if err != nil {
		return catalog.Revision{}, nil, err
	}
	tables, err := reader.ListTables(ctx)
	if err != nil {
		return catalog.Revision{}, nil, err
	}
	physical, err := catalog.BuildSchema(tables)
	if err != nil {
		return catalog.Revision{}, nil, err
	}
	equal, err := revision.Schema.Equal(physical)
	if err != nil {
		return catalog.Revision{}, nil, err
	}
	if !equal {
		return catalog.Revision{}, nil, reject.Mark(reject.ReasonCatalogDrift,
			fmt.Errorf("catalog: stored schema revision %d differs from physical catalog", revision.Version))
	}
	return revision, tables, nil
}
