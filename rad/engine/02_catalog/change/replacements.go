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

// StartColumnReplacement publishes dual-write conversion before any backfill
// begins. The source remains the only bindable logical representation until a
// later atomic ready publication replaces it with Target.
func (m *Mutation) StartColumnReplacement(
	ctx context.Context,
	tableID model.SchemaID,
	columnID model.SchemaID,
	def model.ColumnReplacementDef,
) (model.SchemaTransition, error) {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	source, ok := columnBySchemaID(table, columnID)
	if !ok {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: column schema ID %d does not exist in table %q",
			columnID,
			table.Name,
		)
	}
	if err := validateReplacementDependencies(table, source, def.Nullable); err != nil {
		return model.SchemaTransition{}, err
	}
	tables, err := store.New(m.view).ListTables(ctx)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	for _, candidate := range tables {
		for _, foreignKey := range candidate.ForeignKeys {
			if foreignKey.RefTableID == table.ID && slices.Contains(foreignKey.RefColumns, source.Name) {
				return model.SchemaTransition{}, reject.Inputf(
					"catalog: cannot replace column %q while foreign key %q on table %q references it",
					source.Name,
					foreignKey.Name,
					candidate.Name,
				)
			}
		}
	}
	if err := validateColumnDef(model.ColumnDef{
		ID:       source.SchemaID,
		Name:     source.Name,
		Type:     def.Type,
		Nullable: def.Nullable,
		Format:   def.Format,
		Default:  def.Default,
	}); err != nil {
		return model.SchemaTransition{}, err
	}
	if def.Conversion == "" {
		def.Conversion = model.ColumnConversionStrictBuiltin
	}
	if def.Conversion != model.ColumnConversionStrictBuiltin {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: unsupported column conversion %q",
			def.Conversion,
		)
	}
	prerequisites, waiting, err := m.validateTransitionAdmission(ctx, table, transitionCandidate{
		Kind: model.TransitionColumnReplacement, TableID: table.ID,
		AffectedColumnIDs: []model.SchemaID{source.SchemaID},
		Prerequisites:     def.Prerequisites,
	})
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !waiting &&
		source.Type == def.Type &&
		source.Nullable == def.Nullable &&
		source.Format == def.Format &&
		equalDefault(source.InsertDefault, def.Default) {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: replacement for column %q has identical value semantics",
			source.Name,
		)
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
	transition := model.SchemaTransition{
		ID:                   transitionID,
		Kind:                 model.TransitionColumnReplacement,
		ObjectID:             fmt.Sprintf("column:%d", source.SchemaID),
		State:                model.TransitionWaiting,
		Generation:           1,
		SourceCatalogVersion: revision.Version,
		TableID:              table.ID,
		TableSchemaID:        table.SchemaID,
		AffectedColumnIDs:    []model.SchemaID{source.SchemaID},
		ReplacementRequest: &model.ColumnReplacementRequest{
			ColumnSchemaID: source.SchemaID,
			Type:           def.Type,
			Nullable:       def.Nullable,
			Format:         def.Format,
			Default:        cloneDefault(def.Default),
			Conversion:     def.Conversion,
		},
		Prerequisites: prerequisites,
		GateTableIDs:  canonicalGateTableIDs([]string{table.ID}),
		WorkState:     model.TransitionWorkNormal,
		CreatedAt:     now,
		UpdatedAt:     now,
	}
	if !waiting {
		target, err := buildColumn(ctx, m.view, model.ColumnDef{
			ID:       source.SchemaID,
			Name:     source.Name,
			Type:     def.Type,
			Nullable: def.Nullable,
			Format:   def.Format,
			Default:  def.Default,
		})
		if err != nil {
			return model.SchemaTransition{}, err
		}
		replacement := model.ColumnReplacement{
			Source: source, Target: target, Conversion: def.Conversion,
		}
		protocol, err := store.ReadWriteProtocol(ctx, m.view, table)
		if err != nil {
			return model.SchemaTransition{}, err
		}
		protocol.Generation++
		protocol.ColumnReplacements = append(protocol.ColumnReplacements, model.ColumnReplacementWrite{
			TransitionID: transitionID,
			Replacement:  replacement,
		})
		table.WriteProtocolGeneration = protocol.Generation
		if err := store.SaveTable(ctx, m.view, table); err != nil {
			return model.SchemaTransition{}, err
		}
		if err := store.SaveWriteProtocol(ctx, m.view, protocol); err != nil {
			return model.SchemaTransition{}, err
		}
		transition.State = model.TransitionBuilding
		transition.ColumnReplacement = &replacement
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

func (m *Mutation) BeginColumnReplacementValidation(
	ctx context.Context,
	transitionID string,
	ownerEpoch uint64,
) (model.SchemaTransition, error) {
	transition, _, _, err := m.replacementContext(ctx, transitionID, ownerEpoch)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if transition.State != model.TransitionBuilding {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: replacement transition %q cannot validate from state %q",
			transitionID,
			transition.State,
		)
	}
	if err := m.acquireSchemaFinalizationGates(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	transition.State = model.TransitionValidating
	transition.Generation++
	transition.UpdatedAt = time.Now().UTC()
	if err := store.SaveTransition(ctx, m.view, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

func (m *Mutation) PublishColumnReplacement(
	ctx context.Context,
	transitionID string,
	ownerEpoch uint64,
) (model.SchemaTransition, error) {
	transition, table, protocol, err := m.replacementContext(ctx, transitionID, ownerEpoch)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if transition.State != model.TransitionValidating {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: replacement transition %q cannot publish from state %q",
			transitionID,
			transition.State,
		)
	}
	if _, _, violation, err := store.FirstTransitionViolation(ctx, m.view, transitionID); err != nil {
		return model.SchemaTransition{}, err
	} else if violation {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: replacement transition %q still has conversion violations",
			transitionID,
		)
	}
	if err := m.releaseSchemaFinalizationGates(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	transition, table, protocol, err = m.replacementContext(ctx, transitionID, ownerEpoch)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	replacement := *transition.ColumnReplacement
	found := false
	for i := range table.Columns {
		if table.Columns[i].ID != replacement.Source.ID {
			continue
		}
		if err := store.AdvanceColumnValueFence(ctx, m.view, table, table.Columns[i]); err != nil {
			return model.SchemaTransition{}, err
		}
		replacement.Target.Name = table.Columns[i].Name
		replacement.Target.SchemaID = table.Columns[i].SchemaID
		table.Columns[i] = replacement.Target
		for constraintIndex := range table.Constraints {
			for columnIndex := range table.Constraints[constraintIndex].ColumnIDs {
				if table.Constraints[constraintIndex].ColumnIDs[columnIndex] == replacement.Source.ID {
					table.Constraints[constraintIndex].ColumnIDs[columnIndex] = replacement.Target.ID
					table.Constraints[constraintIndex].DefinitionGeneration++
				}
			}
		}
		transition.ColumnReplacement = &replacement
		found = true
		break
	}
	if !found {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: replacement transition %q source column is no longer active",
			transitionID,
		)
	}
	protocol.Generation++
	protocol.ColumnReplacements = slices.DeleteFunc(protocol.ColumnReplacements, func(write model.ColumnReplacementWrite) bool {
		return write.TransitionID == transitionID
	})
	table.WriteProtocolGeneration = protocol.Generation
	if err := store.SaveTable(ctx, m.view, table); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := store.SaveWriteProtocol(ctx, m.view, protocol); err != nil {
		return model.SchemaTransition{}, err
	}
	transition.State = model.TransitionReady
	transition.Generation++
	transition.WorkState = model.TransitionWorkNormal
	transition.UpdatedAt = time.Now().UTC()
	if err := store.SaveTransition(ctx, m.view, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := m.retireReplacedColumn(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

func (m *Mutation) FailColumnReplacement(
	ctx context.Context,
	transitionID string,
	ownerEpoch uint64,
	cause string,
) (model.SchemaTransition, error) {
	transition, table, protocol, err := m.replacementContext(ctx, transitionID, ownerEpoch)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if err := m.releaseSchemaFinalizationGates(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	transition, table, protocol, err = m.replacementContext(ctx, transitionID, ownerEpoch)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	protocol.Generation++
	protocol.ColumnReplacements = slices.DeleteFunc(protocol.ColumnReplacements, func(write model.ColumnReplacementWrite) bool {
		return write.TransitionID == transitionID
	})
	table.WriteProtocolGeneration = protocol.Generation
	if err := store.SaveTable(ctx, m.view, table); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := store.SaveWriteProtocol(ctx, m.view, protocol); err != nil {
		return model.SchemaTransition{}, err
	}
	transition.State = model.TransitionFailed
	transition.Generation++
	transition.OwnerEpoch++
	transition.WorkState = model.TransitionWorkNormal
	transition.LastError = cause
	transition.UpdatedAt = time.Now().UTC()
	if err := store.SaveTransition(ctx, m.view, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := m.retireFailedReplacement(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

func (m *Mutation) CancelColumnReplacement(
	ctx context.Context,
	transitionID string,
) (model.SchemaTransition, error) {
	transition, ok, err := store.GetTransition(ctx, m.view, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok || transition.Kind != model.TransitionColumnReplacement {
		return model.SchemaTransition{}, reject.Inputf("catalog: column replacement transition %q does not exist", transitionID)
	}
	if transition.State == model.TransitionCancelled {
		return transition, nil
	}
	if transition.State == model.TransitionReady {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: ready replacement transition %q is not cancellable",
			transitionID,
		)
	}
	if transition.State == model.TransitionFailed {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: failed replacement transition %q is already cleaning up",
			transitionID,
		)
	}
	if transition.State == model.TransitionWaiting {
		transition.State = model.TransitionCancelled
		transition.Generation++
		transition.OwnerEpoch++
		transition.UpdatedAt = time.Now().UTC()
		if err := store.SaveTransition(ctx, m.view, transition); err != nil {
			return model.SchemaTransition{}, err
		}
		m.changed = true
		return transition, nil
	}
	transition, table, protocol, err := m.replacementContext(ctx, transitionID, transition.OwnerEpoch)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if err := m.releaseSchemaFinalizationGates(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	transition, table, protocol, err = m.replacementContext(
		ctx,
		transitionID,
		transition.OwnerEpoch,
	)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	protocol.Generation++
	protocol.ColumnReplacements = slices.DeleteFunc(protocol.ColumnReplacements, func(write model.ColumnReplacementWrite) bool {
		return write.TransitionID == transitionID
	})
	table.WriteProtocolGeneration = protocol.Generation
	if err := store.SaveTable(ctx, m.view, table); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := store.SaveWriteProtocol(ctx, m.view, protocol); err != nil {
		return model.SchemaTransition{}, err
	}
	transition.State = model.TransitionCancelled
	transition.Generation++
	transition.OwnerEpoch++
	transition.UpdatedAt = time.Now().UTC()
	if err := store.SaveTransition(ctx, m.view, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := m.retireCancelledReplacement(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

func (m *Mutation) replacementContext(
	ctx context.Context,
	transitionID string,
	ownerEpoch uint64,
) (model.SchemaTransition, model.Table, model.WriteProtocol, error) {
	transition, ok, err := store.GetTransition(ctx, m.view, transitionID)
	if err != nil {
		return model.SchemaTransition{}, model.Table{}, model.WriteProtocol{}, err
	}
	if !ok || transition.Kind != model.TransitionColumnReplacement || transition.ColumnReplacement == nil {
		return model.SchemaTransition{}, model.Table{}, model.WriteProtocol{},
			reject.Inputf("catalog: column replacement transition %q does not exist", transitionID)
	}
	if transition.OwnerEpoch != ownerEpoch {
		return model.SchemaTransition{}, model.Table{}, model.WriteProtocol{},
			fmt.Errorf("catalog: transition %q ownership changed: %w", transitionID, kv.ErrConflict)
	}
	table, ok, err := store.New(m.view).GetTableByID(ctx, transition.TableID)
	if err != nil {
		return model.SchemaTransition{}, model.Table{}, model.WriteProtocol{}, err
	}
	if !ok {
		return model.SchemaTransition{}, model.Table{}, model.WriteProtocol{},
			reject.Inputf("catalog: replacement transition %q table no longer exists", transitionID)
	}
	protocol, err := store.ReadWriteProtocol(ctx, m.view, table)
	return transition, table, protocol, err
}

func validateReplacementDependencies(
	table model.Table,
	source model.Column,
	targetNullable bool,
) error {
	if slices.Contains(table.PrimaryKey, source.Name) {
		return reject.Inputf(
			"catalog: cannot replace primary-key column %q until key-rewrite transitions are supported",
			source.Name,
		)
	}
	for _, index := range table.Indexes {
		if slices.Contains(index.ColumnIDs, source.ID) || slices.Contains(index.Columns, source.Name) {
			return reject.Inputf(
				"catalog: cannot replace column %q while index %q uses its physical representation",
				source.Name,
				index.Name,
			)
		}
	}
	for _, foreignKey := range table.ForeignKeys {
		if slices.Contains(foreignKey.Columns, source.Name) || slices.Contains(foreignKey.RefColumns, source.Name) {
			return reject.Inputf(
				"catalog: cannot replace column %q while foreign key %q uses it",
				source.Name,
				foreignKey.Name,
			)
		}
	}
	if targetNullable {
		for _, constraint := range table.Constraints {
			if constraint.Kind == model.ConstraintNotNull &&
				constraint.State == model.ConstraintValid &&
				slices.Contains(constraint.ColumnIDs, source.ID) {
				return reject.Inputf(
					"catalog: cannot make column %q nullable while valid constraint %q requires non-null values",
					source.Name,
					constraint.Name,
				)
			}
		}
	}
	return nil
}

func columnBySchemaID(table model.Table, schemaID model.SchemaID) (model.Column, bool) {
	for _, column := range table.Columns {
		if column.SchemaID == schemaID {
			return column, true
		}
	}
	return model.Column{}, false
}

func physicalColumnByID(table model.Table, physicalID string) (model.Column, bool) {
	for _, column := range table.Columns {
		if column.ID == physicalID {
			return column, true
		}
	}
	return model.Column{}, false
}

func equalDefault(a, b *model.Default) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
}
