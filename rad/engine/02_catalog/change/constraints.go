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

// declareConstraint records the inert first phase of an atomic start. It does
// not change foreground writes; activation publishes enforcement before the
// surrounding transaction commits.
func (m *Mutation) declareConstraint(
	ctx context.Context,
	tableID model.SchemaID,
	def model.ConstraintDef,
) (model.Constraint, error) {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return model.Constraint{}, err
	}
	if def.Name == "" {
		return model.Constraint{}, reject.Inputf("catalog: constraint name is required")
	}
	for _, existing := range table.Constraints {
		if existing.Name == def.Name {
			return model.Constraint{}, reject.Inputf(
				"catalog: constraint %q already exists on table %q",
				def.Name,
				table.Name,
			)
		}
	}
	if def.Kind != model.ConstraintNotNull {
		return model.Constraint{}, reject.Inputf(
			"catalog: unsupported constraint kind %q",
			def.Kind,
		)
	}
	column, ok := columnBySchemaID(table, def.ColumnID)
	if !ok {
		return model.Constraint{}, reject.Inputf(
			"catalog: column schema ID %d does not exist in table %q",
			def.ColumnID,
			table.Name,
		)
	}
	if !column.Nullable {
		return model.Constraint{}, reject.Inputf(
			"catalog: column %q on table %q is already not nullable",
			column.Name,
			table.Name,
		)
	}
	id, err := store.NextPhysicalID(ctx, m.view, "ct")
	if err != nil {
		return model.Constraint{}, err
	}
	constraint := model.Constraint{
		ID:                   id,
		DefinitionGeneration: 1,
		Name:                 def.Name,
		Kind:                 def.Kind,
		State:                model.ConstraintDeclared,
		ColumnIDs:            []string{column.ID},
	}
	table.Constraints = append(table.Constraints, constraint)
	if err := store.SaveTable(ctx, m.view, table); err != nil {
		return model.Constraint{}, err
	}
	m.changed = true
	return constraint, nil
}

// StartConstraintValidation composes declaration and enforcement in one
// transaction. A successful return therefore means every later affected
// writer must obey the constraint while existing rows are validated.
func (m *Mutation) StartConstraintValidation(
	ctx context.Context,
	tableID model.SchemaID,
	def model.ConstraintDef,
) (model.SchemaTransition, error) {
	constraint, err := m.declareOrResetConstraint(ctx, tableID, def)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	return m.startDeclaredConstraintValidation(
		ctx,
		tableID,
		constraint.ID,
		def.Prerequisites,
	)
}

// declareOrResetConstraint makes a failed or cancelled validation retryable
// under the same deterministic desired-schema constraint name. Terminal
// transition records retain the previous attempt's diagnostics; the logical
// constraint identity is reset to declared and rebound to the column's current
// physical representation before a new transition is created.
func (m *Mutation) declareOrResetConstraint(
	ctx context.Context,
	tableID model.SchemaID,
	def model.ConstraintDef,
) (model.Constraint, error) {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return model.Constraint{}, err
	}
	index := slices.IndexFunc(table.Constraints, func(constraint model.Constraint) bool {
		return constraint.Name == def.Name
	})
	if index < 0 {
		return m.declareConstraint(ctx, tableID, def)
	}
	constraint := table.Constraints[index]
	if constraint.Kind != def.Kind {
		return model.Constraint{}, reject.Inputf(
			"catalog: constraint %q already exists on table %q with kind %q",
			def.Name,
			table.Name,
			constraint.Kind,
		)
	}
	switch constraint.State {
	case model.ConstraintFailed, model.ConstraintCancelled:
	default:
		return model.Constraint{}, reject.Inputf(
			"catalog: constraint %q already exists on table %q",
			def.Name,
			table.Name,
		)
	}
	column, ok := columnBySchemaID(table, def.ColumnID)
	if !ok {
		return model.Constraint{}, reject.Inputf(
			"catalog: column schema ID %d does not exist in table %q",
			def.ColumnID,
			table.Name,
		)
	}
	constraint.State = model.ConstraintDeclared
	constraint.DefinitionGeneration++
	constraint.ColumnIDs = []string{column.ID}
	table.Constraints[index] = constraint
	if err := store.SaveTable(ctx, m.view, table); err != nil {
		return model.Constraint{}, err
	}
	m.changed = true
	return constraint, nil
}

func (m *Mutation) startDeclaredConstraintValidation(
	ctx context.Context,
	tableID model.SchemaID,
	constraintID string,
	prerequisites []string,
) (model.SchemaTransition, error) {
	table, err := m.TableBySchemaID(ctx, tableID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	constraintIndex := slices.IndexFunc(table.Constraints, func(constraint model.Constraint) bool {
		return constraint.ID == constraintID
	})
	if constraintIndex < 0 {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: constraint %q does not exist on table %q",
			constraintID,
			table.Name,
		)
	}
	constraint := table.Constraints[constraintIndex]
	if constraint.State != model.ConstraintDeclared {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: constraint %q cannot begin validation from state %q",
			constraint.Name,
			constraint.State,
		)
	}
	column, ok := physicalColumnByID(table, constraint.ColumnIDs[0])
	if !ok {
		return model.SchemaTransition{}, reject.Fail(
			reject.ReasonCatalogDrift,
			"catalog: constraint %q references missing physical column %q",
			constraint.Name,
			constraint.ColumnIDs[0],
		)
	}
	prerequisites, waiting, err := m.validateTransitionAdmission(ctx, table, transitionCandidate{
		Kind: model.TransitionConstraintValidation, TableID: table.ID,
		AffectedColumnIDs: []model.SchemaID{column.SchemaID},
		Prerequisites:     prerequisites,
	})
	if err != nil {
		return model.SchemaTransition{}, err
	}
	transitionID, err := store.NextPhysicalID(ctx, m.view, "tr")
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !waiting {
		constraint.State = model.ConstraintEnforcingNewWrites
		constraint.DefinitionGeneration++
		table.Constraints[constraintIndex] = constraint
		protocol, err := store.ReadWriteProtocol(ctx, m.view, table)
		if err != nil {
			return model.SchemaTransition{}, err
		}
		protocol.Generation++
		protocol.ConstraintChecks = append(protocol.ConstraintChecks, model.ConstraintCheck{
			TransitionID: transitionID,
			Constraint:   constraint,
		})
		table.WriteProtocolGeneration = protocol.Generation
		if err := store.SaveTable(ctx, m.view, table); err != nil {
			return model.SchemaTransition{}, err
		}
		if err := store.SaveWriteProtocol(ctx, m.view, protocol); err != nil {
			return model.SchemaTransition{}, err
		}
	}
	revision, err := store.CurrentRevision(ctx, m.view)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	position := model.DataPosition("")
	if !waiting {
		if positioned, ok := m.view.(kv.PositionedTxn); ok {
			position = model.DataPosition(positioned.BeginPosition())
		}
	}
	now := time.Now().UTC()
	state := model.TransitionBuilding
	if waiting {
		state = model.TransitionWaiting
	}
	transition := model.SchemaTransition{
		ID:                   transitionID,
		Kind:                 model.TransitionConstraintValidation,
		ObjectID:             constraint.ID,
		State:                state,
		Generation:           1,
		SourceCatalogVersion: revision.Version,
		BasePosition:         position,
		TableID:              table.ID,
		TableSchemaID:        table.SchemaID,
		AffectedColumnIDs:    []model.SchemaID{column.SchemaID},
		Constraint:           &constraint,
		ConstraintRequest: &model.ConstraintValidationRequest{
			ConstraintID: constraint.ID, ColumnSchemaID: column.SchemaID,
		},
		Prerequisites: prerequisites,
		GateTableIDs:  canonicalGateTableIDs([]string{table.ID}),
		WorkState:     model.TransitionWorkNormal,
		CreatedAt:     now,
		UpdatedAt:     now,
	}
	if err := store.SaveTransition(ctx, m.view, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

func (m *Mutation) BeginConstraintHistoricalValidation(
	ctx context.Context,
	transitionID string,
	ownerEpoch uint64,
) (model.SchemaTransition, error) {
	transition, table, protocol, constraintIndex, err := m.constraintContext(
		ctx,
		transitionID,
		ownerEpoch,
	)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	constraint := table.Constraints[constraintIndex]
	if transition.State != model.TransitionBuilding ||
		constraint.State != model.ConstraintEnforcingNewWrites {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: constraint %q cannot scan from transition state %q and constraint state %q",
			constraint.Name,
			transition.State,
			constraint.State,
		)
	}
	if !constraintCheckExists(protocol, transitionID) {
		return model.SchemaTransition{}, reject.Fail(
			reject.ReasonCatalogDrift,
			"catalog: constraint transition %q has no foreground write obligation",
			transitionID,
		)
	}
	constraint.State = model.ConstraintValidatingExisting
	constraint.DefinitionGeneration++
	table.Constraints[constraintIndex] = constraint
	transition.Constraint = &constraint
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

func (m *Mutation) BeginConstraintFinalization(
	ctx context.Context,
	transitionID string,
	ownerEpoch uint64,
) (model.SchemaTransition, error) {
	transition, table, protocol, constraintIndex, err := m.constraintContext(
		ctx,
		transitionID,
		ownerEpoch,
	)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	constraint := table.Constraints[constraintIndex]
	if transition.State != model.TransitionBuilding ||
		constraint.State != model.ConstraintValidatingExisting {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: constraint %q cannot finalize from transition state %q and constraint state %q",
			constraint.Name,
			transition.State,
			constraint.State,
		)
	}
	if !constraintCheckExists(protocol, transitionID) {
		return model.SchemaTransition{}, reject.Fail(
			reject.ReasonCatalogDrift,
			"catalog: constraint transition %q has no foreground write obligation",
			transitionID,
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

func (m *Mutation) PublishConstraint(
	ctx context.Context,
	transitionID string,
	ownerEpoch uint64,
) (model.SchemaTransition, error) {
	transition, _, _, _, err := m.constraintContext(
		ctx,
		transitionID,
		ownerEpoch,
	)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if transition.State != model.TransitionValidating {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: constraint transition %q cannot publish from state %q",
			transitionID,
			transition.State,
		)
	}
	if _, _, violation, err := store.FirstTransitionViolation(ctx, m.view, transitionID); err != nil {
		return model.SchemaTransition{}, err
	} else if violation {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: constraint transition %q still has violations",
			transitionID,
		)
	}
	if err := m.releaseSchemaFinalizationGates(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	transition, table, protocol, constraintIndex, err := m.constraintContext(
		ctx,
		transitionID,
		ownerEpoch,
	)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	constraint := table.Constraints[constraintIndex]
	if constraint.Kind != model.ConstraintNotNull || len(constraint.ColumnIDs) != 1 {
		return model.SchemaTransition{}, reject.Fail(
			reject.ReasonCatalogDrift,
			"catalog: constraint %q has invalid not-null definition",
			constraint.Name,
		)
	}
	columnIndex := slices.IndexFunc(table.Columns, func(column model.Column) bool {
		return column.ID == constraint.ColumnIDs[0]
	})
	if columnIndex < 0 {
		return model.SchemaTransition{}, reject.Fail(
			reject.ReasonCatalogDrift,
			"catalog: constraint %q physical column %q is missing",
			constraint.Name,
			constraint.ColumnIDs[0],
		)
	}
	if err := store.AdvanceColumnValueFence(ctx, m.view, table, table.Columns[columnIndex]); err != nil {
		return model.SchemaTransition{}, err
	}
	table.Columns[columnIndex].Nullable = false
	table.Columns[columnIndex].ValueGeneration++
	constraint.State = model.ConstraintValid
	constraint.DefinitionGeneration++
	table.Constraints[constraintIndex] = constraint
	protocol.Generation++
	protocol.ConstraintChecks = slices.DeleteFunc(protocol.ConstraintChecks, func(check model.ConstraintCheck) bool {
		return check.TransitionID == transitionID
	})
	table.WriteProtocolGeneration = protocol.Generation
	transition.Constraint = &constraint
	transition.State = model.TransitionReady
	transition.Generation++
	transition.WorkState = model.TransitionWorkNormal
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
	if err := m.retireConstraintValidation(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

// FailConstraintValidation explicitly removes foreground enforcement. The
// failure remains in constraint and transition metadata; it never silently
// leaves an unvalidated rule active.
func (m *Mutation) FailConstraintValidation(
	ctx context.Context,
	transitionID string,
	ownerEpoch uint64,
	cause string,
) (model.SchemaTransition, error) {
	transition, _, _, _, err := m.constraintContext(
		ctx,
		transitionID,
		ownerEpoch,
	)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if err := m.releaseSchemaFinalizationGates(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	transition, table, protocol, constraintIndex, err := m.constraintContext(
		ctx,
		transitionID,
		ownerEpoch,
	)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	constraint := table.Constraints[constraintIndex]
	constraint.State = model.ConstraintFailed
	constraint.DefinitionGeneration++
	table.Constraints[constraintIndex] = constraint
	protocol.Generation++
	protocol.ConstraintChecks = slices.DeleteFunc(protocol.ConstraintChecks, func(check model.ConstraintCheck) bool {
		return check.TransitionID == transitionID
	})
	table.WriteProtocolGeneration = protocol.Generation
	transition.Constraint = &constraint
	transition.State = model.TransitionFailed
	transition.Generation++
	transition.OwnerEpoch++
	transition.WorkState = model.TransitionWorkNormal
	transition.LastError = cause
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
	if err := m.retireConstraintValidation(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

func (m *Mutation) CancelConstraintValidation(
	ctx context.Context,
	transitionID string,
) (model.SchemaTransition, error) {
	transition, ok, err := store.GetTransition(ctx, m.view, transitionID)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	if !ok || transition.Kind != model.TransitionConstraintValidation {
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: constraint validation transition %q does not exist",
			transitionID,
		)
	}
	switch transition.State {
	case model.TransitionCancelled:
		return transition, nil
	case model.TransitionReady:
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: valid constraint transition %q is not cancellable",
			transitionID,
		)
	case model.TransitionFailed:
		return model.SchemaTransition{}, reject.Inputf(
			"catalog: failed constraint transition %q is already cleaning up",
			transitionID,
		)
	}
	if transition.State == model.TransitionWaiting {
		table, ok, err := store.New(m.view).GetTableByID(ctx, transition.TableID)
		if err != nil {
			return model.SchemaTransition{}, err
		}
		if !ok {
			return model.SchemaTransition{}, reject.Inputf(
				"catalog: constraint transition %q table no longer exists",
				transitionID,
			)
		}
		constraintIndex := slices.IndexFunc(table.Constraints, func(constraint model.Constraint) bool {
			return transition.Constraint != nil && constraint.ID == transition.Constraint.ID
		})
		if constraintIndex < 0 {
			return model.SchemaTransition{}, reject.Fail(
				reject.ReasonCatalogDrift,
				"catalog: waiting constraint transition %q definition is missing",
				transitionID,
			)
		}
		constraint := table.Constraints[constraintIndex]
		constraint.State = model.ConstraintCancelled
		constraint.DefinitionGeneration++
		table.Constraints[constraintIndex] = constraint
		transition.Constraint = &constraint
		transition.State = model.TransitionCancelled
		transition.Generation++
		transition.OwnerEpoch++
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
	if _, _, _, _, err := m.constraintContext(
		ctx,
		transitionID,
		transition.OwnerEpoch,
	); err != nil {
		return model.SchemaTransition{}, err
	}
	if err := m.releaseSchemaFinalizationGates(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	transition, table, protocol, constraintIndex, err := m.constraintContext(
		ctx,
		transitionID,
		transition.OwnerEpoch,
	)
	if err != nil {
		return model.SchemaTransition{}, err
	}
	constraint := table.Constraints[constraintIndex]
	constraint.State = model.ConstraintCancelled
	constraint.DefinitionGeneration++
	table.Constraints[constraintIndex] = constraint
	protocol.Generation++
	protocol.ConstraintChecks = slices.DeleteFunc(protocol.ConstraintChecks, func(check model.ConstraintCheck) bool {
		return check.TransitionID == transitionID
	})
	table.WriteProtocolGeneration = protocol.Generation
	transition.Constraint = &constraint
	transition.State = model.TransitionCancelled
	transition.Generation++
	transition.OwnerEpoch++
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
	if err := m.retireConstraintValidation(ctx, transition); err != nil {
		return model.SchemaTransition{}, err
	}
	m.changed = true
	return transition, nil
}

func (m *Mutation) constraintContext(
	ctx context.Context,
	transitionID string,
	ownerEpoch uint64,
) (model.SchemaTransition, model.Table, model.WriteProtocol, int, error) {
	transition, ok, err := store.GetTransition(ctx, m.view, transitionID)
	if err != nil {
		return model.SchemaTransition{}, model.Table{}, model.WriteProtocol{}, -1, err
	}
	if !ok || transition.Kind != model.TransitionConstraintValidation || transition.Constraint == nil {
		return model.SchemaTransition{}, model.Table{}, model.WriteProtocol{}, -1,
			reject.Inputf("catalog: constraint validation transition %q does not exist", transitionID)
	}
	if transition.OwnerEpoch != ownerEpoch {
		return model.SchemaTransition{}, model.Table{}, model.WriteProtocol{}, -1,
			fmt.Errorf("catalog: transition %q ownership changed: %w", transitionID, kv.ErrConflict)
	}
	table, ok, err := store.New(m.view).GetTableByID(ctx, transition.TableID)
	if err != nil {
		return model.SchemaTransition{}, model.Table{}, model.WriteProtocol{}, -1, err
	}
	if !ok {
		return model.SchemaTransition{}, model.Table{}, model.WriteProtocol{}, -1,
			reject.Inputf("catalog: constraint transition %q table no longer exists", transitionID)
	}
	constraintIndex := slices.IndexFunc(table.Constraints, func(constraint model.Constraint) bool {
		return constraint.ID == transition.Constraint.ID
	})
	if constraintIndex < 0 {
		return model.SchemaTransition{}, model.Table{}, model.WriteProtocol{}, -1,
			reject.Fail(
				reject.ReasonCatalogDrift,
				"catalog: constraint transition %q definition %q is missing",
				transitionID,
				transition.Constraint.ID,
			)
	}
	protocol, err := store.ReadWriteProtocol(ctx, m.view, table)
	return transition, table, protocol, constraintIndex, err
}

func constraintCheckExists(protocol model.WriteProtocol, transitionID string) bool {
	return slices.ContainsFunc(protocol.ConstraintChecks, func(check model.ConstraintCheck) bool {
		return check.TransitionID == transitionID
	})
}
