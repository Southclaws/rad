package inspect

import "github.com/Southclaws/rad/rad/engine/03_lir/bound"

// PlanSensitive reports whether valid physical plans can commit different
// values for rel. It is conservative: every slice, first, and array is treated
// as sensitive even when another law could prove a unique result.
func PlanSensitive(rel bound.Relation) bool {
	sensitive := false
	WalkRelation(rel, func(rel bound.Relation) {
		if _, ok := rel.(*bound.Slice); ok {
			sensitive = true
		}
	}, func(expr bound.Expr) {
		switch expr.(type) {
		case bound.First, bound.Array:
			sensitive = true
		}
	})
	return sensitive
}
