package exec

import (
	"context"
	"fmt"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/change"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
	"github.com/Southclaws/rad/rad/engine/05_exec/rowstore"
)

func TestOnlineSchemaTransitionsMatchSingleTransactionOracle(t *testing.T) {
	online, onlineCtx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
	oracle, oracleCtx := setupWithOptions(t, WithSchemaJobScheduling(false), withAutomaticReclamation(false))
	for i := range 37 {
		row := userRow(i, fmt.Sprintf("user-%02d", i), int64((i*17)%101))
		if err := online.Insert(onlineCtx, "users", row); err != nil {
			t.Fatal(err)
		}
		if err := oracle.Insert(oracleCtx, "users", row); err != nil {
			t.Fatal(err)
		}
	}

	onlineTable, onlineAge := replacementColumn(t, onlineCtx, online, "users", "age")
	onlineReplacement, err := online.startColumnReplacement(
		onlineCtx,
		onlineTable.SchemaID,
		onlineAge.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := online.runColumnReplacement(onlineCtx, onlineReplacement.ID, 3); err != nil {
		t.Fatal(err)
	}

	oracleTable, oracleAge := replacementColumn(t, oracleCtx, oracle, "users", "age")
	if err := replaceColumnSynchronously(
		oracleCtx,
		oracle,
		oracleTable.SchemaID,
		oracleAge.SchemaID,
		model.ColumnReplacementDef{Type: model.TypeText, Nullable: true},
	); err != nil {
		t.Fatal(err)
	}
	assertLogicalTablesEqual(t, onlineCtx, online, oracleCtx, oracle, "users", 37)

	onlineTable, onlineAge = replacementColumn(t, onlineCtx, online, "users", "age")
	onlineConstraint, err := online.startConstraintValidation(
		onlineCtx,
		onlineTable.SchemaID,
		model.ConstraintDef{
			Name: "users_age_required", Kind: model.ConstraintNotNull, ColumnID: onlineAge.SchemaID,
		},
	)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := online.runConstraintValidation(onlineCtx, onlineConstraint.ID, 4); err != nil {
		t.Fatal(err)
	}

	oracleTable, oracleAge = replacementColumn(t, oracleCtx, oracle, "users", "age")
	if err := validateConstraintSynchronously(
		oracleCtx,
		oracle,
		oracleTable.SchemaID,
		model.ConstraintDef{
			Name: "users_age_required", Kind: model.ConstraintNotNull, ColumnID: oracleAge.SchemaID,
		},
	); err != nil {
		t.Fatal(err)
	}
	assertLogicalTablesEqual(t, onlineCtx, online, oracleCtx, oracle, "users", 37)
	onlineSchema, err := online.Catalog().Schema(onlineCtx)
	if err != nil {
		t.Fatal(err)
	}
	oracleSchema, err := oracle.Catalog().Schema(oracleCtx)
	if err != nil {
		t.Fatal(err)
	}
	equal, err := onlineSchema.Equal(oracleSchema)
	if err != nil || !equal {
		t.Fatalf("online schema differs from synchronous oracle: equal=%v err=%v\nonline=%+v\noracle=%+v", equal, err, onlineSchema, oracleSchema)
	}
}

func replaceColumnSynchronously(
	ctx context.Context,
	engine *Engine,
	tableID model.SchemaID,
	columnID model.SchemaID,
	def model.ColumnReplacementDef,
) error {
	return engine.CatalogTxn(ctx, func(tx *Tx, mutation *change.Mutation) error {
		transition, err := mutation.StartColumnReplacement(ctx, tableID, columnID, def)
		if err != nil {
			return err
		}
		table, ok, err := store.New(tx.txn).GetTableByID(ctx, transition.TableID)
		if err != nil || !ok {
			return fmt.Errorf("oracle: replacement table missing: %w", err)
		}
		rows, _, err := rowstore.ScanRawTableBatch(ctx, tx.txn, table, nil, 1_000_000)
		if err != nil {
			return err
		}
		for _, row := range rows {
			source, err := codec.ReadColumnValue(row.Raw, transition.ColumnReplacement.Source)
			if err != nil {
				return err
			}
			target, err := codec.ConvertColumnValue(
				source,
				transition.ColumnReplacement.Target,
				transition.ColumnReplacement.Conversion,
			)
			if err != nil {
				return err
			}
			raw, err := codec.SetColumnValue(row.Raw, transition.ColumnReplacement.Target, target)
			if err != nil {
				return err
			}
			if err := tx.txn.Put(ctx, row.Key, raw); err != nil {
				return err
			}
		}
		transition, err = mutation.BeginColumnReplacementValidation(ctx, transition.ID, transition.OwnerEpoch)
		if err != nil {
			return err
		}
		_, err = mutation.PublishColumnReplacement(ctx, transition.ID, transition.OwnerEpoch)
		return err
	})
}

func validateConstraintSynchronously(
	ctx context.Context,
	engine *Engine,
	tableID model.SchemaID,
	def model.ConstraintDef,
) error {
	return engine.CatalogTxn(ctx, func(tx *Tx, mutation *change.Mutation) error {
		transition, err := mutation.StartConstraintValidation(ctx, tableID, def)
		if err != nil {
			return err
		}
		transition, err = mutation.BeginConstraintHistoricalValidation(
			ctx,
			transition.ID,
			transition.OwnerEpoch,
		)
		if err != nil {
			return err
		}
		table, ok, err := store.New(tx.txn).GetTableByID(ctx, transition.TableID)
		if err != nil || !ok {
			return fmt.Errorf("oracle: constraint table missing: %w", err)
		}
		column, ok := physicalColumn(table, transition.Constraint.ColumnIDs[0])
		if !ok {
			return fmt.Errorf("oracle: constraint column is missing")
		}
		rows, _, err := rowstore.ScanRawTableBatch(ctx, tx.txn, table, nil, 1_000_000)
		if err != nil {
			return err
		}
		for _, row := range rows {
			value, err := codec.ReadColumnValue(row.Raw, column)
			if err != nil {
				return err
			}
			if value.Null {
				return fmt.Errorf("oracle: row %x violates constraint %q", row.PK, transition.Constraint.Name)
			}
		}
		transition, err = mutation.BeginConstraintFinalization(ctx, transition.ID, transition.OwnerEpoch)
		if err != nil {
			return err
		}
		_, err = mutation.PublishConstraint(ctx, transition.ID, transition.OwnerEpoch)
		return err
	})
}

func assertLogicalTablesEqual(
	t *testing.T,
	leftCtx context.Context,
	left *Engine,
	rightCtx context.Context,
	right *Engine,
	table string,
	rows int,
) {
	t.Helper()
	for i := range rows {
		key := lir.Row{"id": lir.Int64(int64(i))}
		leftRow, leftOK, leftErr := left.GetByPrimaryKey(leftCtx, table, key)
		rightRow, rightOK, rightErr := right.GetByPrimaryKey(rightCtx, table, key)
		if leftErr != nil || rightErr != nil || leftOK != rightOK || fmt.Sprint(leftRow) != fmt.Sprint(rightRow) {
			t.Fatalf(
				"row %d differs: left=%v ok=%v err=%v right=%v ok=%v err=%v",
				i,
				leftRow,
				leftOK,
				leftErr,
				rightRow,
				rightOK,
				rightErr,
			)
		}
	}
}
