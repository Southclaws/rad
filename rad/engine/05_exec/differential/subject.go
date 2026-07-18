package differential

import (
	"context"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// Subject is the system a differential runs against: something that executes a
// query by its chosen plan and by a forced full scan, and exposes the catalog
// the reference interpreter binds against. Both *frontend.DB and the raw
// *exec.Engine satisfy it, so a runner can point the same differential at
// either level of the stack.
type Subject interface {
	Execute(ctx context.Context, q lir.Query) (lir.Datum, error)
	ExecuteForced(ctx context.Context, q lir.Query) (lir.Datum, error)
	Catalog() *catalog.Catalog
}

// ScanFunc yields a table's rows to the reference interpreter. A runner passes
// the rows it loaded, so the oracle reads data independently of the engine's
// own read path — a storage round-trip bug then surfaces as a divergence rather
// than hiding in both sides.
type ScanFunc func(ctx context.Context, table model.Table) ([]lir.Row, error)
