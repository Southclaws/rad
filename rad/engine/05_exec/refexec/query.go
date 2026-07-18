package refexec

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	binder "github.com/Southclaws/rad/rad/engine/04_planner/bind"
)

// InterpretQuery binds an unbound query and interprets it — the whole oracle in
// one call. Binding is the deterministic name/slot resolution shared with
// production on purpose (the split that makes this an oracle is after binding);
// stored rows enter through the injected scan, so this still touches no storage
// itself. A bind error surfaces as-is, letting a caller tell an un-bindable
// query apart from a genuine runtime divergence.
func InterpretQuery(ctx context.Context, cat binder.Catalog, scan ScanFunc, q lir.Query) (lir.Datum, error) {
	bq, err := binder.Bind(ctx, cat, q)
	if err != nil {
		return lir.Datum{}, err
	}
	return Interpret(ctx, scan, bq)
}
