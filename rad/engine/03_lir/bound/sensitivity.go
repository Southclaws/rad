package bound

// PlanSensitive reports whether valid physical plans can commit different
// values for rel. It is conservative: every slice, first, and array is treated
// as sensitive even when another law could prove a unique result.
func PlanSensitive(rel Relation) bool {
	sensitive := false
	WalkRelation(rel, func(rel Relation) {
		if _, ok := rel.(*Slice); ok {
			sensitive = true
		}
	}, func(expr Expr) {
		switch expr.(type) {
		case First, Array:
			sensitive = true
		}
	})
	return sensitive
}
