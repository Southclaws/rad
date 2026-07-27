package change

import (
	"context"
	"fmt"
	"slices"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func canonicalGateTableIDs(ids []string) []string {
	out := slices.Clone(ids)
	slices.Sort(out)
	out = slices.Compact(out)
	return out
}

// acquireSchemaFinalizationGates publishes one exclusive gate per affected
// table in stable physical ID order inside the caller's Slate transaction.
// The transaction makes the full affected-table set atomic.
func (m *Mutation) acquireSchemaFinalizationGates(
	ctx context.Context,
	transition model.SchemaTransition,
) error {
	for _, tableID := range canonicalGateTableIDs(transition.GateTableIDs) {
		table, ok, err := store.New(m.view).GetTableByID(ctx, tableID)
		if err != nil {
			return err
		}
		if !ok {
			return reject.Inputf(
				"catalog: finalization table %q for transition %q does not exist",
				tableID,
				transition.ID,
			)
		}
		protocol, err := store.ReadWriteProtocol(ctx, m.view, table)
		if err != nil {
			return err
		}
		expected := schemaFinalizationGate(transition)
		if gate := protocol.FinalizationGate; gate != nil {
			if *gate == expected {
				continue
			}
			if gate.TransitionID == transition.ID {
				return reject.Fail(
					reject.ReasonCatalogDrift,
					"catalog: table %q gate for transition %q has object %q and kind %q, want object %q and kind %q",
					table.Name,
					transition.ID,
					gate.ObjectID,
					gate.Kind,
					expected.ObjectID,
					expected.Kind,
				)
			}
			return fmt.Errorf(
				"catalog: table %q is already gated by transition %q: %w",
				table.Name,
				gate.TransitionID,
				kv.ErrConflict,
			)
		}
		protocol.Generation++
		protocol.FinalizationGate = &expected
		table.WriteProtocolGeneration = protocol.Generation
		if err := store.SaveTable(ctx, m.view, table); err != nil {
			return err
		}
		if err := store.SaveWriteProtocol(ctx, m.view, protocol); err != nil {
			return err
		}
	}
	return nil
}

func (m *Mutation) releaseSchemaFinalizationGates(
	ctx context.Context,
	transition model.SchemaTransition,
) error {
	for _, tableID := range canonicalGateTableIDs(transition.GateTableIDs) {
		table, ok, err := store.New(m.view).GetTableByID(ctx, tableID)
		if err != nil {
			return err
		}
		if !ok {
			return reject.Inputf(
				"catalog: finalization table %q for transition %q does not exist",
				tableID,
				transition.ID,
			)
		}
		protocol, err := store.ReadWriteProtocol(ctx, m.view, table)
		if err != nil {
			return err
		}
		removed, err := removeSchemaFinalizationGate(&protocol, transition)
		if err != nil {
			return err
		}
		if !removed {
			if requiresSchemaFinalizationGate(transition) {
				return reject.Fail(
					reject.ReasonCatalogDrift,
					"catalog: validating transition %q has no finalization gate on table %q",
					transition.ID,
					table.Name,
				)
			}
			continue
		}
		protocol.Generation++
		table.WriteProtocolGeneration = protocol.Generation
		if err := store.SaveTable(ctx, m.view, table); err != nil {
			return err
		}
		if err := store.SaveWriteProtocol(ctx, m.view, protocol); err != nil {
			return err
		}
	}
	return nil
}

func schemaFinalizationGate(transition model.SchemaTransition) model.SchemaFinalizationGate {
	return model.SchemaFinalizationGate{
		TransitionID: transition.ID,
		ObjectID:     transition.ObjectID,
		Kind:         transition.Kind,
	}

}

func requiresSchemaFinalizationGate(transition model.SchemaTransition) bool {
	if transition.State != model.TransitionValidating {
		return false
	}
	return transition.Kind != model.TransitionIndexBuild || transition.Index.Unique
}

func removeSchemaFinalizationGate(
	protocol *model.WriteProtocol,
	transition model.SchemaTransition,
) (bool, error) {
	if protocol.FinalizationGate == nil || protocol.FinalizationGate.TransitionID != transition.ID {
		return false, nil
	}
	expected := schemaFinalizationGate(transition)
	if *protocol.FinalizationGate != expected {
		return false, reject.Fail(
			reject.ReasonCatalogDrift,
			"catalog: gate for transition %q has object %q and kind %q, want object %q and kind %q",
			transition.ID,
			protocol.FinalizationGate.ObjectID,
			protocol.FinalizationGate.Kind,
			expected.ObjectID,
			expected.Kind,
		)
	}
	protocol.FinalizationGate = nil
	return true, nil
}

func removeOwnedSchemaFinalizationGate(
	protocol *model.WriteProtocol,
	transition model.SchemaTransition,
) error {
	removed, err := removeSchemaFinalizationGate(protocol, transition)
	if err != nil {
		return err
	}
	if requiresSchemaFinalizationGate(transition) && !removed {
		return reject.Fail(
			reject.ReasonCatalogDrift,
			"catalog: validating transition %q has no finalization gate",
			transition.ID,
		)
	}
	return nil
}
