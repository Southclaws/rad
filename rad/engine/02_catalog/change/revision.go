package change

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
)

type Mutation struct {
	view    kv.KV
	changed bool
}

func Apply(ctx context.Context, view kv.Txn, fn func(mutation *Mutation) error) (model.Revision, error) {
	mutation := &Mutation{view: view}
	if err := fn(mutation); err != nil {
		return model.Revision{}, err
	}
	if !mutation.changed {
		return store.CurrentRevision(ctx, view)
	}
	return store.BumpRevision(ctx, view)
}

func (s *Service) Revision(ctx context.Context) (model.Revision, error) {
	return store.CurrentRevision(ctx, s.store)
}

func (s *Service) Revisions(ctx context.Context) ([]model.Revision, error) {
	return store.Revisions(ctx, s.store)
}
