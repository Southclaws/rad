package change

import (
	"context"
	"fmt"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
)

func (s *Service) Mode(ctx context.Context) (model.Mode, error) {
	return store.ReadMode(ctx, s.store)
}

func (s *Service) InitMode(ctx context.Context, requested model.Mode) (model.Mode, error) {
	var settled model.Mode
	err := s.transact(ctx, func(view kv.Txn) error {
		stored, ok, err := store.ReadStoredMode(ctx, view)
		if err != nil {
			return err
		}
		if ok {
			if requested != "" && requested != stored {
				return fmt.Errorf(
					"catalog: this database is %s-managed and its mode is set once at creation; it cannot be changed to %s",
					stored, requested,
				)
			}
			settled = stored
			return nil
		}
		settled = requested
		if settled == "" {
			settled = model.ModeDirect
		}
		return store.SetMode(ctx, view, settled)
	})
	return settled, err
}
