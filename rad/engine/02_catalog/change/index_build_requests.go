package change

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func newIndexBuildRequest(
	ctx context.Context,
	view kv.KV,
	table model.Table,
	def model.IndexDef,
) (model.IndexBuildRequest, error) {
	if len(def.Columns) == 0 {
		return model.IndexBuildRequest{}, reject.Inputf(
			"catalog: index %q has no columns",
			def.Name,
		)
	}
	columnSchemaIDs := make([]model.SchemaID, len(def.Columns))
	for i, name := range def.Columns {
		column, ok := table.Column(name)
		if !ok {
			return model.IndexBuildRequest{}, reject.Inputf(
				"catalog: index %q references unknown column %q",
				def.Name,
				name,
			)
		}
		columnSchemaIDs[i] = column.SchemaID
	}
	physicalID, err := store.NextPhysicalID(ctx, view, "i")
	if err != nil {
		return model.IndexBuildRequest{}, err
	}
	logicalID, err := store.NextPhysicalID(ctx, view, "ix")
	if err != nil {
		return model.IndexBuildRequest{}, err
	}
	return model.IndexBuildRequest{
		PhysicalID: physicalID, LogicalID: logicalID, Name: def.Name,
		ColumnSchemaIDs: columnSchemaIDs, Unique: def.Unique,
	}, nil
}

func materializeIndexBuild(
	table model.Table,
	request model.IndexBuildRequest,
	state model.IndexState,
) (model.Index, error) {
	columns := make([]string, len(request.ColumnSchemaIDs))
	columnIDs := make([]string, len(request.ColumnSchemaIDs))
	for i, schemaID := range request.ColumnSchemaIDs {
		column, ok := columnBySchemaID(table, schemaID)
		if !ok {
			return model.Index{}, reject.Inputf(
				"catalog: index %q logical column %d no longer exists on table %q",
				request.Name,
				schemaID,
				table.Name,
			)
		}
		columns[i] = column.Name
		columnIDs[i] = column.ID
	}
	return model.Index{
		ID: request.PhysicalID, LogicalID: request.LogicalID,
		DefinitionGeneration: 1, State: state,
		Name: request.Name, Columns: columns, ColumnIDs: columnIDs,
		Unique: request.Unique,
	}, nil
}

func reservedIndexName(transition model.SchemaTransition) string {
	if transition.IndexRequest != nil {
		return transition.IndexRequest.Name
	}
	return transition.Index.Name
}

func (m *Mutation) ensureIndexNameAvailable(
	ctx context.Context,
	table model.Table,
	name string,
) error {
	if _, exists := table.Index(name); exists {
		return reject.Inputf(
			"catalog: index %q already exists on table %q",
			name,
			table.Name,
		)
	}
	transitions, err := store.ListTransitions(ctx, m.view)
	if err != nil {
		return err
	}
	for _, transition := range transitions {
		if transition.TableID == table.ID &&
			transition.Kind == model.TransitionIndexBuild &&
			!transitionTerminal(transition.State) &&
			reservedIndexName(transition) == name {
			return reject.Inputf(
				"catalog: index name %q is reserved by active transition %q on table %q",
				name,
				transition.ID,
				table.Name,
			)
		}
	}
	return nil
}

func (m *Mutation) ensureIndexNameReservationOwned(
	ctx context.Context,
	transition model.SchemaTransition,
) error {
	name := reservedIndexName(transition)
	transitions, err := store.ListTransitions(ctx, m.view)
	if err != nil {
		return err
	}
	found := false
	for _, candidate := range transitions {
		if candidate.TableID != transition.TableID ||
			candidate.Kind != model.TransitionIndexBuild ||
			transitionTerminal(candidate.State) ||
			reservedIndexName(candidate) != name {
			continue
		}
		if candidate.ID != transition.ID {
			return reject.Fail(
				reject.ReasonCatalogDrift,
				"catalog: index name %q reserved by both transition %q and transition %q",
				name,
				transition.ID,
				candidate.ID,
			)
		}
		found = true
	}
	if !found {
		return reject.Fail(
			reject.ReasonCatalogDrift,
			"catalog: index transition %q no longer owns reservation for name %q",
			transition.ID,
			name,
		)
	}
	return nil
}
