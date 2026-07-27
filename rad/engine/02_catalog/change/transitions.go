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

func (c *Service) CancelSchemaTransition(ctx context.Context, transitionID string) (model.SchemaTransition, error) {
	var transition model.SchemaTransition
	err := c.mutate(ctx, func(change *Mutation) error {
		var err error
		transition, err = change.CancelSchemaTransition(ctx, transitionID)
		return err
	})
	return transition, err
}

func (c *Service) GetTransition(ctx context.Context, transitionID string) (model.SchemaTransition, bool, error) {
	txn, err := c.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return model.SchemaTransition{}, false, err
	}
	defer txn.Rollback()
	transition, ok, err := store.GetTransition(ctx, txn, transitionID)
	if err != nil || !ok {
		return transition, ok, err
	}
	transition.DeltaHighWater, err = store.DeltaHighWater(ctx, txn, transitionID)
	transition.RefreshWorkState(transition.DeltaHighWater)
	return transition, true, err
}

func (c *Service) ListTransitions(ctx context.Context) ([]model.SchemaTransition, error) {
	txn, err := c.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return nil, err
	}
	defer txn.Rollback()
	transitions, err := store.ListTransitions(ctx, txn)
	if err != nil {
		return nil, err
	}
	for i := range transitions {
		high, err := store.DeltaHighWater(ctx, txn, transitions[i].ID)
		if err != nil {
			return nil, err
		}
		transitions[i].DeltaHighWater = high
		transitions[i].RefreshWorkState(high)
	}
	return transitions, nil
}

func (m *Mutation) GetTransition(ctx context.Context, transitionID string) (model.SchemaTransition, error) {
	transition, ok, err := store.GetTransition(ctx, m.view, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Fail(reject.ReasonNotFound, "catalog: transition %q does not exist", transitionID)
	}
	transition.DeltaHighWater, err = store.DeltaHighWater(ctx, m.view, transitionID)
	transition.RefreshWorkState(transition.DeltaHighWater)
	return transition, err
}

// StartIndexBuild atomically publishes a planner-invisible index definition,
// its durable transition, and the new table write protocol.
func (m *Mutation) StartIndexBuild(ctx context.Context, tableID model.SchemaID, def model.IndexDef) (model.SchemaTransition, error) {
	return m.StartIndexBuildWithPrerequisites(ctx, tableID, def, nil)
}

func (m *Mutation) StartIndexBuildWithPrerequisites(
	ctx context.Context,
	tableID model.SchemaID,
	def model.IndexDef,
	prerequisites []string,
) (model.SchemaTransition, error) {
	return m.StartIndexBuildWithLimitsAndPrerequisites(
		ctx,
		tableID,
		def,
		prerequisites,
		model.DefaultDeltaSoftLimit,
		model.DefaultDeltaHardLimit,
	)
}

// StartIndexBuildWithLimits is the execution-layer form: both limits are
// published in transition diagnostics and the hard limit is copied into its
// immutable foreground delta sink, so writers do not read the worker's hot
// checkpoint record.
func (m *Mutation) StartIndexBuildWithLimits(
	ctx context.Context,
	tableID model.SchemaID,
	def model.IndexDef,
	softLimit, hardLimit uint64,
) (model.SchemaTransition, error) {
	return m.StartIndexBuildWithLimitsAndPrerequisites(
		ctx,
		tableID,
		def,
		nil,
		softLimit,
		hardLimit,
	)
}

func (m *Mutation) StartIndexBuildWithLimitsAndPrerequisites(
	ctx context.Context,
	tableID model.SchemaID,
	def model.IndexDef,
	prerequisites []string,
	softLimit, hardLimit uint64,
) (model.SchemaTransition, error) {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if err := m.ensureIndexNameAvailable(ctx, table, def.Name); err != nil {
		return model.SchemaTransition{}, err
	}
	affectedColumns, err := indexColumnSchemaIDs(table, model.Index{
		Name: def.Name, Columns: slices.Clone(def.Columns),
	})
	if err != nil {
		return model.SchemaTransition{}, err
	}
	prerequisites, waiting, err := m.validateTransitionAdmission(ctx, table, transitionCandidate{
		Kind: model.TransitionIndexBuild, TableID: table.ID, AffectedColumnIDs: affectedColumns,
		Prerequisites: prerequisites,
	})
	if err != nil {
		return model.SchemaTransition{}, err
	}
	request, err := newIndexBuildRequest(ctx, m.view, table, def)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	transitionID, err := store.NextPhysicalID(ctx, m.view, "tr")
	if err != nil {
		return model.SchemaTransition{}, err
	}
	revision, err := store.CurrentRevision(ctx, m.view)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	now := time.Now().UTC()
	index := model.Index{
		ID: request.PhysicalID, LogicalID: request.LogicalID,
		DefinitionGeneration: 1, State: model.IndexBuilding,
		Name: request.Name, Unique: request.Unique,
	}
	transition := model.SchemaTransition{
		ID: transitionID, Kind: model.TransitionIndexBuild, State: model.TransitionWaiting,
		ObjectID:   index.LogicalID,
		Generation: 1, SourceCatalogVersion: revision.Version,
		TableID: table.ID, TableSchemaID: table.SchemaID, Index: index,
		AffectedColumnIDs: affectedColumns,
		IndexRequest:      &request,
		Prerequisites:     prerequisites,
		GateTableIDs:      canonicalGateTableIDs([]string{table.ID}),
		DeltaSoftLimit:    softLimit, DeltaHardLimit: hardLimit,
		WorkState: model.TransitionWorkNormal,
		CreatedAt: now, UpdatedAt: now,
	}
	if !waiting {
		index, err = materializeIndexBuild(table, request, model.IndexBuilding)
		if err != nil {
			return model.SchemaTransition{}, err
		}
		protocol, err := store.ReadWriteProtocol(ctx, m.view, table)
		if err != nil {
			return model.SchemaTransition{}, err
		}
		protocol.Generation++
		protocol.DeltaSinks = append(protocol.DeltaSinks, model.IndexDeltaSink{
			TransitionID: transitionID, Index: index, Columns: slices.Clone(index.Columns),
			DeltaHardLimit: hardLimit,
		})
		table.WriteProtocolGeneration = protocol.Generation
		table.Indexes = append(table.Indexes, index)
		if err := store.SaveTable(ctx, m.view, table); err != nil {
			return model.SchemaTransition{}, err
		}
		if err := store.SaveWriteProtocol(ctx, m.view, protocol); err != nil {
			return model.SchemaTransition{}, err
		}
		transition.State = model.TransitionBuilding
		transition.Index = index
		if positioned, ok := m.view.(kv.PositionedTxn); ok {
			transition.BasePosition = model.DataPosition(positioned.BeginPosition())
		}
	}
	if err := store.SaveTransition(ctx, m.view, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

// BeginIndexValidation atomically closes the table's write protocol and moves
// a unique build into bounded claim validation. Writers admitted under the
// previous protocol conflict with this publication; later writers observe the
// gate and retry without affecting unrelated tables.
func (m *Mutation) BeginIndexValidation(ctx context.Context, transitionID string, ownerEpoch uint64) (model.SchemaTransition, error) {
	transition, ok, err := store.GetTransition(ctx, m.view, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Fail(reject.ReasonNotFound, "catalog: transition %q does not exist", transitionID)
	}
	if transition.OwnerEpoch != ownerEpoch {
		return model.SchemaTransition{}, fmt.Errorf("catalog: transition %q ownership changed: %w", transitionID, kv.ErrConflict)
	}
	if !transition.Index.Unique || transition.State != model.TransitionCatchingUp {
		return model.SchemaTransition{}, reject.Inputf("catalog: transition %q cannot begin unique validation from state %q", transitionID, transition.State)
	}
	if err := m.acquireSchemaFinalizationGates(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	table, ok, err := store.New(m.view).GetTableByID(ctx, transition.TableID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Inputf("catalog: transition %q table no longer exists", transitionID)
	}
	found := false
	for i := range table.Indexes {
		if table.Indexes[i].LogicalID == transition.Index.LogicalID {
			table.Indexes[i].State = model.IndexValidating
			table.Indexes[i].DefinitionGeneration++
			transition.Index = table.Indexes[i]
			found = true
			break
		}
	}
	if !found {
		return model.SchemaTransition{}, reject.Fail(
			reject.ReasonCatalogDrift,
			"catalog: transition %q index definition is missing",
			transitionID,
		)
	}
	transition.State = model.TransitionValidating
	transition.Generation++
	transition.UpdatedAt = time.Now().UTC()
	if err := store.SaveTable(ctx, m.view, table); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := store.SaveTransition(ctx, m.view, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

// FailIndexValidation releases the affected-table gate, removes the failed
// logical index definition, and schedules its partial physical state for
// bounded cleanup while retaining transition diagnostics.
func (m *Mutation) FailIndexValidation(
	ctx context.Context,
	transitionID string,
	ownerEpoch uint64,
	cause string,
) (model.SchemaTransition, error) {
	transition, ok, err := store.GetTransition(ctx, m.view, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Inputf("catalog: transition %q does not exist", transitionID)
	}
	if transition.OwnerEpoch != ownerEpoch {
		return model.SchemaTransition{}, fmt.Errorf("catalog: transition %q ownership changed: %w", transitionID, kv.ErrConflict)
	}
	if transition.State != model.TransitionValidating {
		return model.SchemaTransition{}, reject.Inputf("catalog: transition %q cannot fail validation from state %q", transitionID, transition.State)
	}
	table, ok, err := store.New(m.view).GetTableByID(ctx, transition.TableID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Inputf("catalog: transition %q table no longer exists", transitionID)
	}
	found := false
	for _, index := range table.Indexes {
		if index.LogicalID != transition.Index.LogicalID {
			continue
		}
		index.State = model.IndexFailed
		index.DefinitionGeneration++
		transition.Index = index
		found = true
		break
	}
	if !found {
		return model.SchemaTransition{}, reject.Inputf("catalog: transition %q index definition is missing", transitionID)
	}
	table.Indexes = slices.DeleteFunc(table.Indexes, func(index model.Index) bool {
		return index.LogicalID == transition.Index.LogicalID
	})
	protocol, err := store.ReadWriteProtocol(ctx, m.view, table)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	protocol.Generation++
	protocol.DeltaSinks = slices.DeleteFunc(protocol.DeltaSinks, func(sink model.IndexDeltaSink) bool {
		return sink.TransitionID == transitionID
	})
	if err := removeOwnedSchemaFinalizationGate(&protocol, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	table.WriteProtocolGeneration = protocol.Generation
	transition.State = model.TransitionFailed
	transition.WorkState = model.TransitionWorkNormal
	transition.LastError = cause
	transition.OwnerEpoch++
	transition.Generation++
	transition.UpdatedAt = time.Now().UTC()
	if err := store.SaveTable(ctx, m.view, table); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := store.SaveWriteProtocol(ctx, m.view, protocol); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := store.SaveTransition(ctx, m.view, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := m.retireFailedIndex(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

func (m *Mutation) PublishIndexReady(ctx context.Context, transitionID string, ownerEpoch uint64) (model.SchemaTransition, error) {
	transition, ok, err := store.GetTransition(ctx, m.view, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Inputf("catalog: transition %q does not exist", transitionID)
	}
	if transition.OwnerEpoch != ownerEpoch {
		return model.SchemaTransition{}, fmt.Errorf("catalog: transition %q ownership changed: %w", transitionID, kv.ErrConflict)
	}
	table, ok, err := store.New(m.view).GetTableByID(ctx, transition.TableID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Inputf("catalog: transition %q table no longer exists", transitionID)
	}
	indexPosition := -1
	for i := range table.Indexes {
		if table.Indexes[i].LogicalID != transition.Index.LogicalID {
			continue
		}
		indexPosition = i
		break
	}
	if indexPosition < 0 {
		return model.SchemaTransition{}, reject.Inputf("catalog: transition %q index definition is missing", transitionID)
	}
	if transition.State != model.TransitionValidating {
		return model.SchemaTransition{}, reject.Fail(
			reject.ReasonCatalogDrift,
			"catalog: transition %q cannot publish ready before validation",
			transitionID,
		)
	}
	table.Indexes[indexPosition].State = model.IndexReady
	table.Indexes[indexPosition].DefinitionGeneration++
	transition.Index = table.Indexes[indexPosition]
	protocol, err := store.ReadWriteProtocol(ctx, m.view, table)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	protocol.Generation++
	protocol.ReadyIndexes = append(protocol.ReadyIndexes, transition.Index)
	protocol.DeltaSinks = slices.DeleteFunc(protocol.DeltaSinks, func(sink model.IndexDeltaSink) bool {
		return sink.TransitionID == transitionID
	})
	if err := removeOwnedSchemaFinalizationGate(&protocol, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	table.WriteProtocolGeneration = protocol.Generation
	if err := store.SaveTable(ctx, m.view, table); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := store.SaveWriteProtocol(ctx, m.view, protocol); err != nil {
		return model.SchemaTransition{}, err
	}
	transition.State = model.TransitionReady
	transition.WorkState = model.TransitionWorkNormal
	transition.Generation++
	transition.UpdatedAt = time.Now().UTC()
	if err := store.SaveTransition(ctx, m.view, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := m.retireTransitionDeltas(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

func (m *Mutation) CancelSchemaTransition(ctx context.Context, transitionID string) (model.SchemaTransition, error) {
	transition, ok, err := store.GetTransition(ctx, m.view, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Fail(reject.ReasonNotFound, "catalog: transition %q does not exist", transitionID)
	}
	if transition.Kind == model.TransitionColumnReplacement {
		return m.CancelColumnReplacement(ctx, transitionID)
	}
	if transition.Kind == model.TransitionConstraintValidation {
		return m.CancelConstraintValidation(ctx, transitionID)
	}
	if transition.Kind != model.TransitionIndexBuild {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: transition %q of kind %q does not support cancellation",
			transitionID, transition.Kind,
		)
	}
	switch transition.State {
	case model.TransitionReady:
		return model.SchemaTransition{}, reject.Inputf("catalog: ready transition %q is not cancellable; delete its index", transitionID)
	case model.TransitionCancelled:
		return transition, nil
	case model.TransitionFailed:
		return model.SchemaTransition{}, reject.Inputf("catalog: failed transition %q requires cleanup, not cancellation", transitionID)
	case model.TransitionWaiting:
		transition.State = model.TransitionCancelled
		transition.Index.State = model.IndexCancelled
		transition.Generation++
		transition.OwnerEpoch++
		transition.UpdatedAt = time.Now().UTC()
		if err := store.SaveTransition(ctx, m.view, transition); err != nil {
			return model.SchemaTransition{}, err
		}
		m.changed = true
		return transition, nil
	}
	table, ok, err := store.New(m.view).GetTableByID(ctx, transition.TableID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok {
		return model.SchemaTransition{}, reject.Inputf("catalog: transition %q table no longer exists", transitionID)
	}
	protocol, err := store.ReadWriteProtocol(ctx, m.view, table)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	protocol.Generation++
	protocol.DeltaSinks = slices.DeleteFunc(protocol.DeltaSinks, func(sink model.IndexDeltaSink) bool {
		return sink.TransitionID == transitionID
	})
	if err := removeOwnedSchemaFinalizationGate(&protocol, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	table.WriteProtocolGeneration = protocol.Generation
	table.Indexes = slices.DeleteFunc(table.Indexes, func(index model.Index) bool {
		return index.LogicalID == transition.Index.LogicalID
	})
	if err := store.SaveTable(ctx, m.view, table); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := store.SaveWriteProtocol(ctx, m.view, protocol); err != nil {
		return model.SchemaTransition{}, err
	}
	transition.State = model.TransitionCancelled
	transition.Index.State = model.IndexCancelled
	transition.Generation++
	transition.OwnerEpoch++
	transition.UpdatedAt = time.Now().UTC()
	if err := store.SaveTransition(ctx, m.view, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := m.retireCancelledIndex(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}
