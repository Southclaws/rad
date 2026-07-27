package analysis

import (
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

func underlyingScan(rel bound.Relation) *bound.Scan {
	for {
		switch node := rel.(type) {
		case *bound.Scan:
			return node
		case *bound.Filter:
			rel = node.In
		case *bound.Order:
			rel = node.In
		case *bound.Slice:
			rel = node.In
		default:
			return nil
		}
	}
}

// UnderlyingScan returns the scan beneath a filter/order/slice chain.
func UnderlyingScan(rel bound.Relation) *bound.Scan { return underlyingScan(rel) }

func conjuncts(expr bound.Expr) []bound.Expr {
	if binary, ok := expr.(bound.Binary); ok && binary.Op == lir.OpAnd {
		return append(conjuncts(binary.L), conjuncts(binary.R)...)
	}
	return []bound.Expr{expr}
}
