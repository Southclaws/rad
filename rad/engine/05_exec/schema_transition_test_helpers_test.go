package exec

import (
	"context"

	"github.com/Southclaws/rad/rad/engine/02_catalog/change"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// These helpers let executor unit tests arrange durable transitions directly.
// Product callers start the same work exclusively through transactional PIR.

func (e *Engine) startIndexBuild(ctx context.Context, table string, def model.IndexDef) (model.SchemaTransition, error) {
	var transition model.SchemaTransition
	err := e.CatalogTxn(ctx, func(tx *Tx, mutation *change.Mutation) error {
		tbl, ok, err := store.New(tx.txn).GetTable(ctx, table)
		if err != nil {
			return err
		}
		if !ok {
			return reject.Inputf("catalog: table %q does not exist", table)
		}
		transition, err = mutation.StartIndexBuildWithLimits(
			ctx, tbl.SchemaID, def, e.schemaJobConfig.DeltaSoftLimit, e.schemaJobConfig.DeltaHardLimit,
		)
		return err
	})
	return transition, err
}

func (e *Engine) startIndexBuildBySchemaID(ctx context.Context, tableID model.SchemaID, def model.IndexDef) (model.SchemaTransition, error) {
	return e.startIndexBuildBySchemaIDWithPrerequisites(ctx, tableID, def, nil)
}

func (e *Engine) startIndexBuildBySchemaIDWithPrerequisites(
	ctx context.Context,
	tableID model.SchemaID,
	def model.IndexDef,
	prerequisites []string,
) (model.SchemaTransition, error) {
	var transition model.SchemaTransition
	err := e.CatalogTxn(ctx, func(_ *Tx, mutation *change.Mutation) error {
		var err error
		transition, err = mutation.StartIndexBuildWithLimitsAndPrerequisites(
			ctx,
			tableID,
			def,
			prerequisites,
			e.schemaJobConfig.DeltaSoftLimit,
			e.schemaJobConfig.DeltaHardLimit,
		)
		return err
	})
	return transition, err
}

func (e *Engine) startColumnReplacement(
	ctx context.Context,
	tableID model.SchemaID,
	columnID model.SchemaID,
	def model.ColumnReplacementDef,
) (model.SchemaTransition, error) {
	var transition model.SchemaTransition
	err := e.CatalogTxn(ctx, func(_ *Tx, mutation *change.Mutation) error {
		var err error
		transition, err = mutation.StartColumnReplacement(ctx, tableID, columnID, def)
		return err
	})
	return transition, err
}

func (e *Engine) startConstraintValidation(
	ctx context.Context,
	tableID model.SchemaID,
	def model.ConstraintDef,
) (model.SchemaTransition, error) {
	var transition model.SchemaTransition
	err := e.CatalogTxn(ctx, func(_ *Tx, mutation *change.Mutation) error {
		var err error
		transition, err = mutation.StartConstraintValidation(ctx, tableID, def)
		return err
	})
	return transition, err
}
