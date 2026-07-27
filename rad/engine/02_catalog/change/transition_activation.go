package change

import (
	"context"
	"fmt"
	"slices"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// ActivateWaitingTransition publishes the operation-specific foreground
// protocol only after every durable prerequisite is ready. Active
// prerequisites leave the transition waiting; a failed or cancelled
// prerequisite deterministically fails it without creating physical work.
func (m *Mutation) ActivateWaitingTransition(
	ctx context.Context,
	transitionID string,
) (model.SchemaTransition, error) {
	transition, ok, err := store.GetTransition(ctx, m.view, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: transition %q does not exist",
			transitionID,
		)
	}
	if transition.State != model.TransitionWaiting {
		return transition, nil
	}
	for _, prerequisiteID := range transition.Prerequisites {
		prerequisite, ok, err := store.GetTransition(ctx, m.view, prerequisiteID)
		if err != nil {
			return model.SchemaTransition{}, err
		}
		if !ok {
			return m.failWaitingTransition(
				ctx,
				transition,
				fmt.Sprintf("prerequisite transition %q is missing", prerequisiteID),
			)
		}
		switch prerequisite.State {
		case model.TransitionReady:
		case model.TransitionFailed, model.TransitionCancelled:
			return m.failWaitingTransition(
				ctx,
				transition,
				fmt.Sprintf(
					"prerequisite transition %q completed in state %q",
					prerequisiteID,
					prerequisite.State,
				),
			)
		default:
			return transition, nil
		}
	}

	switch transition.Kind {
	case model.TransitionIndexBuild:
		return m.activateWaitingIndexBuild(ctx, transition)
	case model.TransitionColumnReplacement:
		return m.activateWaitingColumnReplacement(ctx, transition)
	case model.TransitionConstraintValidation:
		return m.activateWaitingConstraintValidation(ctx, transition)
	default:
		return model.SchemaTransition{}, reject.Fail(
			reject.ReasonCatalogDrift,
			"catalog: transition %q of kind %q cannot wait for prerequisites",
			transition.ID,
			transition.Kind,
		)
	}
}

func (m *Mutation) activateWaitingIndexBuild(
	ctx context.Context,
	transition model.SchemaTransition,
) (model.SchemaTransition, error) {
	request := transition.IndexRequest
	if request == nil {
		return model.SchemaTransition{}, reject.Fail(
			reject.ReasonCatalogDrift,
			"catalog: waiting index transition %q has no logical request",
			transition.ID,
		)
	}
	table, ok, err := store.New(m.view).GetTableByID(ctx, transition.TableID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return m.failWaitingTransition(ctx, transition, "target table was deleted")
	}
	if err := m.ensureIndexNameReservationOwned(ctx, transition); err != nil {
		return m.failWaitingTransition(ctx, transition, err.Error())
	}
	if existing, exists := table.Index(request.Name); exists {
		return m.failWaitingTransition(
			ctx,
			transition,
			fmt.Sprintf(
				"index name %q was claimed by physical index %q",
				request.Name,
				existing.ID,
			),
		)
	}
	index, err := materializeIndexBuild(table, *request, model.IndexBuilding)
	if err != nil {
		return m.failWaitingTransition(ctx, transition, err.Error())
	}
	protocol, err := store.ReadWriteProtocol(ctx, m.view, table)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	protocol.Generation++
	protocol.DeltaSinks = append(protocol.DeltaSinks, model.IndexDeltaSink{
		TransitionID: transition.ID, Index: index,
		Columns: slices.Clone(index.Columns), DeltaHardLimit: transition.DeltaHardLimit,
	})
	table.WriteProtocolGeneration = protocol.Generation
	table.Indexes = append(table.Indexes, index)
	if err := store.SaveTable(ctx, m.view, table); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := store.SaveWriteProtocol(ctx, m.view, protocol); err != nil {
		return model.SchemaTransition{}, err
	}
	revision, err := store.CurrentRevision(ctx, m.view)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	transition.SourceCatalogVersion = revision.Version
	if positioned, ok := m.view.(kv.PositionedTxn); ok {
		transition.BasePosition = model.DataPosition(positioned.BeginPosition())
	}
	transition.Index = index
	transition.State = model.TransitionBuilding
	transition.Generation++
	transition.UpdatedAt = time.Now().UTC()
	if err := store.SaveTransition(ctx, m.view, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

func (m *Mutation) activateWaitingColumnReplacement(
	ctx context.Context,
	transition model.SchemaTransition,
) (model.SchemaTransition, error) {
	request := transition.ReplacementRequest
	if request == nil {
		return model.SchemaTransition{}, reject.Fail(
			reject.ReasonCatalogDrift,
			"catalog: waiting replacement transition %q has no logical request",
			transition.ID,
		)
	}
	table, ok, err := store.New(m.view).GetTableByID(ctx, transition.TableID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return m.failWaitingTransition(ctx, transition, "target table was deleted")
	}
	source, ok := columnBySchemaID(table, request.ColumnSchemaID)
	if !ok {
		return m.failWaitingTransition(
			ctx,
			transition,
			fmt.Sprintf("logical column %d was deleted", request.ColumnSchemaID),
		)
	}
	if err := validateReplacementDependencies(table, source, request.Nullable); err != nil {
		return m.failWaitingTransition(ctx, transition, err.Error())
	}
	if source.Type == request.Type &&
		source.Nullable == request.Nullable &&
		source.Format == request.Format &&
		equalDefault(source.InsertDefault, request.Default) {
		transition.State = model.TransitionReady
		transition.Generation++
		transition.UpdatedAt = time.Now().UTC()
		if err := store.SaveTransition(ctx, m.view, transition); err != nil {
			return model.SchemaTransition{}, err
		}
		m.changed = true
		return transition, nil
	}
	target, err := buildColumn(ctx, m.view, model.ColumnDef{
		ID:       source.SchemaID,
		Name:     source.Name,
		Type:     request.Type,
		Nullable: request.Nullable,
		Format:   request.Format,
		Default:  request.Default,
	})
	if err != nil {
		return model.SchemaTransition{}, err
	}
	replacement := model.ColumnReplacement{
		Source: source, Target: target, Conversion: request.Conversion,
	}
	protocol, err := store.ReadWriteProtocol(ctx, m.view, table)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	protocol.Generation++
	protocol.ColumnReplacements = append(
		protocol.ColumnReplacements,
		model.ColumnReplacementWrite{
			TransitionID: transition.ID,
			Replacement:  replacement,
		},
	)
	table.WriteProtocolGeneration = protocol.Generation
	if err := store.SaveTable(ctx, m.view, table); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := store.SaveWriteProtocol(ctx, m.view, protocol); err != nil {
		return model.SchemaTransition{}, err
	}
	revision, err := store.CurrentRevision(ctx, m.view)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	transition.SourceCatalogVersion = revision.Version
	if positioned, ok := m.view.(kv.PositionedTxn); ok {
		transition.BasePosition = model.DataPosition(positioned.BeginPosition())
	}
	transition.ColumnReplacement = &replacement
	transition.State = model.TransitionBuilding
	transition.Generation++
	transition.UpdatedAt = time.Now().UTC()
	if err := store.SaveTransition(ctx, m.view, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

func (m *Mutation) activateWaitingConstraintValidation(
	ctx context.Context,
	transition model.SchemaTransition,
) (model.SchemaTransition, error) {
	request := transition.ConstraintRequest
	if request == nil {
		return model.SchemaTransition{}, reject.Fail(
			reject.ReasonCatalogDrift,
			"catalog: waiting constraint transition %q has no logical request",
			transition.ID,
		)
	}
	table, ok, err := store.New(m.view).GetTableByID(ctx, transition.TableID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return m.failWaitingTransition(ctx, transition, "target table was deleted")
	}
	constraintIndex := slices.IndexFunc(table.Constraints, func(constraint model.Constraint) bool {
		return constraint.ID == request.ConstraintID
	})
	if constraintIndex < 0 {
		return m.failWaitingTransition(ctx, transition, "declared constraint was deleted")
	}
	column, ok := columnBySchemaID(table, request.ColumnSchemaID)
	if !ok {
		return m.failWaitingTransition(
			ctx,
			transition,
			fmt.Sprintf("logical column %d was deleted", request.ColumnSchemaID),
		)
	}
	constraint := table.Constraints[constraintIndex]
	constraint.ColumnIDs = []string{column.ID}
	constraint.DefinitionGeneration++
	if !column.Nullable {
		constraint.State = model.ConstraintValid
		table.Constraints[constraintIndex] = constraint
		transition.Constraint = &constraint
		transition.State = model.TransitionReady
		transition.Generation++
		transition.UpdatedAt = time.Now().UTC()
		if err := store.SaveTable(ctx, m.view, table); err != nil {
			return model.SchemaTransition{}, err
		}
		if err := store.SaveTransition(ctx, m.view, transition); err != nil {
			return model.SchemaTransition{}, err
		}
		if err := m.retireConstraintValidation(ctx, transition); err != nil {
			return model.SchemaTransition{}, err
		}
		m.changed = true
		return transition, nil
	}

	constraint.State = model.ConstraintEnforcingNewWrites
	table.Constraints[constraintIndex] = constraint
	protocol, err := store.ReadWriteProtocol(ctx, m.view, table)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	protocol.Generation++
	protocol.ConstraintChecks = append(protocol.ConstraintChecks, model.ConstraintCheck{
		TransitionID: transition.ID,
		Constraint:   constraint,
	})
	table.WriteProtocolGeneration = protocol.Generation
	if err := store.SaveTable(ctx, m.view, table); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := store.SaveWriteProtocol(ctx, m.view, protocol); err != nil {
		return model.SchemaTransition{}, err
	}
	revision, err := store.CurrentRevision(ctx, m.view)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	transition.SourceCatalogVersion = revision.Version
	if positioned, ok := m.view.(kv.PositionedTxn); ok {
		transition.BasePosition = model.DataPosition(positioned.BeginPosition())
	}
	transition.Constraint = &constraint
	transition.State = model.TransitionBuilding
	transition.Generation++
	transition.UpdatedAt = time.Now().UTC()
	if err := store.SaveTransition(ctx, m.view, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

func (m *Mutation) failWaitingTransition(
	ctx context.Context,
	transition model.SchemaTransition,
	cause string,
) (model.SchemaTransition, error) {
	transition.State = model.TransitionFailed
	if transition.Kind == model.TransitionIndexBuild {
		transition.Index.State = model.IndexFailed
	}
	transition.Generation++
	transition.OwnerEpoch++
	transition.LastError = cause
	transition.UpdatedAt = time.Now().UTC()
	if transition.Kind == model.TransitionConstraintValidation &&
		transition.ConstraintRequest != nil {
		table, ok, err := store.New(m.view).GetTableByID(ctx, transition.TableID)
		if err != nil {
			return model.SchemaTransition{}, err
		}
		if ok {
			index := slices.IndexFunc(table.Constraints, func(constraint model.Constraint) bool {
				return constraint.ID == transition.ConstraintRequest.ConstraintID
			})
			if index >= 0 {
				constraint := table.Constraints[index]
				constraint.State = model.ConstraintFailed
				constraint.DefinitionGeneration++
				table.Constraints[index] = constraint
				transition.Constraint = &constraint
				if err := store.SaveTable(ctx, m.view, table); err != nil {
					return model.SchemaTransition{}, err
				}
			}
		}
		if err := m.retireConstraintValidation(ctx, transition); err != nil {
			return model.SchemaTransition{}, err
		}
	}
	if err := store.SaveTransition(ctx, m.view, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}
