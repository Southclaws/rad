package change

import (
	"context"
	"fmt"
	"slices"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

type transitionCandidate struct {
	Kind              model.TransitionKind
	TableID           string
	AffectedColumnIDs []model.SchemaID
	Prerequisites     []string
}

// validateTransitionAdmission canonicalizes the durable DAG edges and applies
// the resource compatibility matrix. A non-ready prerequisite makes the new
// transition wait. A conflicting active transition is accepted only when it
// is an explicit prerequisite, so activation has one deterministic order.
func (m *Mutation) validateTransitionAdmission(
	ctx context.Context,
	table model.Table,
	candidate transitionCandidate,
) (prerequisites []string, waiting bool, err error) {
	prerequisites = slices.Clone(candidate.Prerequisites)
	slices.Sort(prerequisites)
	prerequisites = slices.Compact(prerequisites)
	candidate.AffectedColumnIDs = canonicalSchemaIDs(candidate.AffectedColumnIDs)

	for _, id := range prerequisites {
		transition, ok, err := store.GetTransition(ctx, m.view, id)
		if err != nil {
			return nil, false, err
		}
		if !ok {
			return nil, false, reject.Inputf(
				"catalog: prerequisite transition %q does not exist",
				id,
			)
		}
		switch transition.State {
		case model.TransitionReady:
		case model.TransitionFailed, model.TransitionCancelled:
			return nil, false, reject.Inputf(
				"catalog: prerequisite transition %q is terminal in state %q",
				id,
				transition.State,
			)
		default:
			waiting = true
		}
	}

	transitions, err := store.ListTransitions(ctx, m.view)
	if err != nil {
		return nil, false, err
	}
	for _, active := range transitions {
		if active.TableID != candidate.TableID || transitionTerminal(active.State) {
			continue
		}
		activeColumns, err := affectedColumnSchemaIDs(table, active)
		if err != nil {
			return nil, false, err
		}
		if candidate.Kind == model.TransitionColumnReplacement &&
			active.Kind == model.TransitionIndexBuild &&
			schemaIDsOverlap(candidate.AffectedColumnIDs, activeColumns) {
			return nil, false, reject.Inputf(
				"catalog: column replacement cannot follow active index transition %q on table %q because the ready index would still depend on the old physical column; cancel or finish and delete the index first",
				active.ID,
				table.Name,
			)
		}
		if transitionKindsCompatible(
			candidate.Kind,
			candidate.AffectedColumnIDs,
			active.Kind,
			activeColumns,
		) {
			continue
		}
		if slices.Contains(prerequisites, active.ID) {
			waiting = true
			continue
		}
		return nil, false, reject.Inputf(
			"catalog: %s transition conflicts with active %s transition %q on table %q; add it as a prerequisite to wait",
			candidate.Kind,
			active.Kind,
			active.ID,
			table.Name,
		)
	}
	return prerequisites, waiting, nil
}

func transitionTerminal(state model.TransitionState) bool {
	switch state {
	case model.TransitionReady, model.TransitionFailed, model.TransitionCancelled:
		return true
	default:
		return false
	}
}

func transitionKindsCompatible(
	aKind model.TransitionKind,
	aColumns []model.SchemaID,
	bKind model.TransitionKind,
	bColumns []model.SchemaID,
) bool {
	if !schemaIDsOverlap(aColumns, bColumns) {
		return true
	}
	if aKind == model.TransitionIndexBuild && bKind == model.TransitionIndexBuild {
		return true
	}
	return (aKind == model.TransitionIndexBuild && bKind == model.TransitionConstraintValidation) ||
		(aKind == model.TransitionConstraintValidation && bKind == model.TransitionIndexBuild)
}

func schemaIDsOverlap(a, b []model.SchemaID) bool {
	for _, left := range a {
		if slices.Contains(b, left) {
			return true
		}
	}
	return false
}

func canonicalSchemaIDs(ids []model.SchemaID) []model.SchemaID {
	out := slices.Clone(ids)
	slices.Sort(out)
	return slices.Compact(out)
}

func affectedColumnSchemaIDs(
	table model.Table,
	transition model.SchemaTransition,
) ([]model.SchemaID, error) {
	if len(transition.AffectedColumnIDs) > 0 {
		return canonicalSchemaIDs(transition.AffectedColumnIDs), nil
	}
	if transition.IndexRequest != nil {
		return canonicalSchemaIDs(transition.IndexRequest.ColumnSchemaIDs), nil
	}
	if transition.ReplacementRequest != nil {
		return []model.SchemaID{transition.ReplacementRequest.ColumnSchemaID}, nil
	}
	if transition.ConstraintRequest != nil {
		return []model.SchemaID{transition.ConstraintRequest.ColumnSchemaID}, nil
	}
	if transition.ColumnReplacement != nil {
		return []model.SchemaID{transition.ColumnReplacement.Source.SchemaID}, nil
	}
	if transition.Kind == model.TransitionIndexBuild {
		return indexColumnSchemaIDs(table, transition.Index)
	}
	if transition.Constraint != nil {
		return physicalColumnSchemaIDs(table, transition.Constraint.ColumnIDs)
	}
	return nil, reject.Fail(
		reject.ReasonCatalogDrift,
		"catalog: active transition %q has no affected-column identity",
		transition.ID,
	)
}

func indexColumnSchemaIDs(table model.Table, index model.Index) ([]model.SchemaID, error) {
	if len(index.ColumnIDs) > 0 {
		return physicalColumnSchemaIDs(table, index.ColumnIDs)
	}
	ids := make([]model.SchemaID, 0, len(index.Columns))
	for _, name := range index.Columns {
		column, ok := table.Column(name)
		if !ok {
			return nil, reject.Fail(
				reject.ReasonCatalogDrift,
				"catalog: index %q references missing column %q on table %q",
				index.Name,
				name,
				table.Name,
			)
		}
		ids = append(ids, column.SchemaID)
	}
	return canonicalSchemaIDs(ids), nil
}

func physicalColumnSchemaIDs(table model.Table, physicalIDs []string) ([]model.SchemaID, error) {
	ids := make([]model.SchemaID, 0, len(physicalIDs))
	for _, physicalID := range physicalIDs {
		found := false
		for _, column := range table.Columns {
			if column.ID != physicalID {
				continue
			}
			ids = append(ids, column.SchemaID)
			found = true
			break
		}
		if !found {
			return nil, fmt.Errorf(
				"catalog: table %q has no physical column %q",
				table.Name,
				physicalID,
			)
		}
	}
	return canonicalSchemaIDs(ids), nil
}
