package exec

import (
	"context"
	"errors"
	"fmt"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
)

// ErrRetentionPinned marks an expected reclamation wait on a durable owner.
var ErrRetentionPinned = errors.New("exec: reclamation is blocked by a durable retention pin")

// retain publishes one durable resource edge. It is an engine-internal
// lifecycle primitive for future prepared plans, retained snapshots, replicas,
// CDC readers, and schema jobs; it is deliberately not PIR or LIR.
func (e *Engine) retain(ctx context.Context, pin model.RetentionPin) error {
	txn, err := e.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return err
	}
	defer txn.Rollback()
	if err := store.SaveRetentionPin(ctx, txn, pin); err != nil {
		return err
	}
	if err := txn.Commit(ctx); err != nil {
		return err
	}
	e.kickSchemaJobs()
	return nil
}

// releaseRetention removes exactly one durable pin and wakes eligible schema
// work. Resource identities themselves remain permanently non-reusable.
func (e *Engine) releaseRetention(ctx context.Context, pinID string) error {
	txn, err := e.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return err
	}
	defer txn.Rollback()
	if err := store.DeleteRetentionPin(ctx, txn, pinID); err != nil {
		return err
	}
	if err := txn.Commit(ctx); err != nil {
		return err
	}
	e.kickSchemaJobs()
	return nil
}

// retentionHorizons returns a coherent resource-specific view of all live
// durable pins.
func (e *Engine) retentionHorizons(ctx context.Context) (model.RetentionHorizons, error) {
	return store.RetentionHorizons(ctx, e.store)
}

func retentionPinnedError(pin model.RetentionPin, reclamation model.Reclamation) error {
	return fmt.Errorf(
		"%w: reclamation %q is retained by %s %q through pin %q",
		ErrRetentionPinned, reclamation.ID, pin.OwnerKind, pin.OwnerID, pin.ID,
	)
}
