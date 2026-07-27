package change

import (
	"context"
	"slices"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
)

func (m *Mutation) nextCatalogVersion(ctx context.Context) (uint64, error) {
	revision, err := store.CurrentRevision(ctx, m.view)
	if err != nil {
		return 0, err
	}
	return revision.Version + 1, nil
}

func (m *Mutation) retireTable(ctx context.Context, table model.Table) error {
	version, err := m.nextCatalogVersion(ctx)
	if err != nil {
		return err
	}
	indexes := make([]string, 0, len(table.Indexes))
	for _, index := range table.Indexes {
		indexes = append(indexes, index.ID)
	}
	slices.Sort(indexes)
	return store.QueueReclamation(ctx, m.view, model.Reclamation{
		ID: store.TableReclamationID(table.ID), Kind: model.ReclamationTable,
		RetiredCatalogVersion: version, TableID: table.ID, TableSchemaID: table.SchemaID,
		IndexIDs: indexes,
	})
}

func (m *Mutation) retireColumn(ctx context.Context, table model.Table, column model.Column) error {
	version, err := m.nextCatalogVersion(ctx)
	if err != nil {
		return err
	}
	return store.QueueReclamation(ctx, m.view, model.Reclamation{
		ID: store.ColumnReclamationID(table.ID, column.ID), Kind: model.ReclamationColumn,
		RetiredCatalogVersion: version, TableID: table.ID, TableSchemaID: table.SchemaID,
		ColumnID: column.ID,
	})
}

func (m *Mutation) retireIndex(ctx context.Context, table model.Table, index model.Index) error {
	version, err := m.nextCatalogVersion(ctx)
	if err != nil {
		return err
	}
	return store.QueueReclamation(ctx, m.view, model.Reclamation{
		ID: store.IndexReclamationID(table.ID, index.ID), Kind: model.ReclamationIndex,
		RetiredCatalogVersion: version, TableID: table.ID, TableSchemaID: table.SchemaID,
		IndexID: index.ID,
	})
}

func (m *Mutation) retireTransitionDeltas(ctx context.Context, transition model.SchemaTransition) error {
	version, err := m.nextCatalogVersion(ctx)
	if err != nil {
		return err
	}
	return store.QueueReclamation(ctx, m.view, model.Reclamation{
		ID: store.TransitionDeltaReclamationID(transition.ID), Kind: model.ReclamationTransitionDeltas,
		RetiredCatalogVersion: version, TableID: transition.TableID,
		TableSchemaID: transition.TableSchemaID, IndexID: transition.Index.ID,
		TransitionID: transition.ID,
	})
}

func (m *Mutation) retireCancelledIndex(ctx context.Context, transition model.SchemaTransition) error {
	version, err := m.nextCatalogVersion(ctx)
	if err != nil {
		return err
	}
	return store.QueueReclamation(ctx, m.view, model.Reclamation{
		ID: store.CancelledIndexReclamationID(transition.ID), Kind: model.ReclamationCancelledIndex,
		RetiredCatalogVersion: version, TableID: transition.TableID,
		TableSchemaID: transition.TableSchemaID, IndexID: transition.Index.ID,
		TransitionID: transition.ID,
	})
}

func (m *Mutation) retireFailedIndex(ctx context.Context, transition model.SchemaTransition) error {
	version, err := m.nextCatalogVersion(ctx)
	if err != nil {
		return err
	}
	return store.QueueReclamation(ctx, m.view, model.Reclamation{
		ID: store.FailedIndexReclamationID(transition.ID), Kind: model.ReclamationFailedIndex,
		RetiredCatalogVersion: version, TableID: transition.TableID,
		TableSchemaID: transition.TableSchemaID, IndexID: transition.Index.ID,
		TransitionID: transition.ID,
	})
}

func (m *Mutation) retireReplacedColumn(ctx context.Context, transition model.SchemaTransition) error {
	return m.retireReplacementColumn(
		ctx,
		transition,
		store.ReplacedColumnReclamationID(transition.ID),
		model.ReclamationReplacedColumn,
		transition.ColumnReplacement.Source.ID,
	)
}

func (m *Mutation) retireCancelledReplacement(ctx context.Context, transition model.SchemaTransition) error {
	return m.retireReplacementColumn(
		ctx,
		transition,
		store.CancelledReplacementReclamationID(transition.ID),
		model.ReclamationCancelledReplacement,
		transition.ColumnReplacement.Target.ID,
	)
}

func (m *Mutation) retireFailedReplacement(ctx context.Context, transition model.SchemaTransition) error {
	return m.retireReplacementColumn(
		ctx,
		transition,
		store.FailedReplacementReclamationID(transition.ID),
		model.ReclamationFailedReplacement,
		transition.ColumnReplacement.Target.ID,
	)
}

func (m *Mutation) retireReplacementColumn(
	ctx context.Context,
	transition model.SchemaTransition,
	id string,
	kind model.ReclamationKind,
	columnID string,
) error {
	version, err := m.nextCatalogVersion(ctx)
	if err != nil {
		return err
	}
	return store.QueueReclamation(ctx, m.view, model.Reclamation{
		ID: id, Kind: kind, RetiredCatalogVersion: version,
		TableID: transition.TableID, TableSchemaID: transition.TableSchemaID,
		ColumnID: columnID, TransitionID: transition.ID,
	})
}

func (m *Mutation) retireConstraintValidation(ctx context.Context, transition model.SchemaTransition) error {
	version, err := m.nextCatalogVersion(ctx)
	if err != nil {
		return err
	}
	return store.QueueReclamation(ctx, m.view, model.Reclamation{
		ID:   store.ConstraintValidationReclamationID(transition.ID),
		Kind: model.ReclamationConstraintValidation, RetiredCatalogVersion: version,
		TableID: transition.TableID, TableSchemaID: transition.TableSchemaID,
		TransitionID: transition.ID,
	})
}
