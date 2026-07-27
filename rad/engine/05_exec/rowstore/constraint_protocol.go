package rowstore

import (
	"context"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func applyConstraintChecks(
	ctx context.Context,
	view kv.KV,
	table model.Table,
	protocol model.WriteProtocol,
	row lir.Row,
	pk []byte,
) error {
	for _, obligation := range protocol.ConstraintChecks {
		constraint := obligation.Constraint
		switch constraint.Kind {
		case model.ConstraintNotNull:
			if len(constraint.ColumnIDs) != 1 {
				return reject.Fail(
					reject.ReasonCatalogDrift,
					"exec: not-null constraint %q has %d columns",
					constraint.Name,
					len(constraint.ColumnIDs),
				)
			}
			name, ok := physicalColumnName(table, constraint.ColumnIDs[0])
			if !ok {
				return reject.Fail(
					reject.ReasonCatalogDrift,
					"exec: constraint %q column %q is not active on table %q",
					constraint.Name,
					constraint.ColumnIDs[0],
					table.Name,
				)
			}
			if row[name].Null {
				return reject.Fail(
					reject.ReasonConstraintViolation,
					"exec: constraint %q rejects NULL in column %q",
					constraint.Name,
					name,
				)
			}
		default:
			return reject.Fail(
				reject.ReasonCatalogDrift,
				"exec: constraint %q has unsupported kind %q",
				constraint.Name,
				constraint.Kind,
			)
		}
		if err := store.DeleteTransitionViolation(ctx, view, obligation.TransitionID, pk); err != nil {
			return err
		}
	}
	return nil
}

func clearConstraintViolations(
	ctx context.Context,
	view kv.KV,
	protocol model.WriteProtocol,
	pk []byte,
) error {
	for _, obligation := range protocol.ConstraintChecks {
		if err := store.DeleteTransitionViolation(ctx, view, obligation.TransitionID, pk); err != nil {
			return err
		}
	}
	return nil
}
