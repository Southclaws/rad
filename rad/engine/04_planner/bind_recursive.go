package planner

import (
	"maps"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func (b *binder) bindRecursiveRef(node lir.RecursiveRef) (*bound.RecursiveRef, error) {
	if node.Scope == "" {
		return nil, reject.Inputf("planner: recursive_ref of binding %q needs a scope label", node.Binding)
	}
	if b.labels[node.Scope] {
		return nil, reject.Inputf("planner: duplicate scope %q", node.Scope)
	}
	if node.Binding != b.recursing {
		return nil, reject.Inputf("planner: recursive_ref to %q is legal only inside that binding's step", node.Binding)
	}
	binding, ok := b.bindings[node.Binding]
	if !ok || !binding.Recursive {
		return nil, reject.Inputf("planner: recursive_ref names %q, which is not a recursive binding", node.Binding)
	}
	fields, canonical := b.freshOccurrence(binding.Out)
	ref := bound.NewRecursiveRef(node.Binding, node.Scope, fields, canonical)
	if err := b.exposeScope(node.Scope, ref); err != nil {
		return nil, err
	}
	return ref, nil
}

func (b *binder) bindRecursiveBinding(name string, recursive lir.Recursive) (*bound.Binding, error) {
	if err := validateRecursive(name, recursive); err != nil {
		return nil, err
	}

	anchor, err := b.bindRel(recursive.Anchor)
	b.scopes = b.scopes[:0]
	if err != nil {
		return nil, err
	}
	if err := recursiveOutputUnique(anchor.Output()); err != nil {
		return nil, err
	}

	binding := &bound.Binding{
		Name:         name,
		Root:         anchor,
		Out:          anchor.Output(),
		Recursive:    true,
		Accumulation: recursive.Accumulation,
	}
	b.bindings[name] = binding

	previous := b.recursing
	slotMark := b.nextSlot
	labelMark := maps.Clone(b.labels)
	var step bound.Relation
	for {
		b.nextSlot = slotMark
		b.labels = maps.Clone(labelMark)
		b.scopes = b.scopes[:0]

		b.recursing = name
		candidate, err := b.bindRel(recursive.Step)
		b.recursing = previous
		if err != nil {
			return nil, err
		}
		widened, err := reconcileRecursive(anchor.Output(), candidate.Output())
		if err != nil {
			return nil, err
		}
		step = candidate
		stable := rowTypeEqual(widened, binding.Out)
		binding.Out = widened
		if stable {
			break
		}
	}
	b.scopes = b.scopes[:0]

	binding.Step = step
	binding.PlanSensitive = bound.PlanSensitive(anchor) || bound.PlanSensitive(step)
	return binding, nil
}

func validateRecursive(name string, recursive lir.Recursive) error {
	if err := checkRecursiveAnchor(name, recursive.Anchor); err != nil {
		return err
	}
	count := 0
	if err := checkRecursiveStep(name, recursive.Step, false, &count); err != nil {
		return err
	}
	switch {
	case count == 0:
		return reject.Inputf("planner: recursive step contains no recursive_ref — the step must reference the binding to recurse")
	case count > 1:
		return reject.Inputf("planner: recursive step contains %d recursive_refs — linear recursion requires exactly one", count)
	default:
		return nil
	}
}

func checkRecursiveAnchor(name string, relation lir.Relation) error {
	return walkLIR(relation, func(relation lir.Relation) error {
		switch node := relation.(type) {
		case lir.RecursiveRef:
			return reject.Inputf("planner: recursive anchor contains a recursive_ref — the anchor is the base case and must not recurse")
		case lir.Ref:
			if node.Binding == name {
				return reject.Inputf("planner: recursive anchor references the binding through an ordinary ref — the base case cannot observe it")
			}
		case lir.Recursive:
			return reject.Inputf("planner: a recursive relation is only valid as a binding body")
		}
		return nil
	}, nil)
}

func checkRecursiveStep(name string, relation lir.Relation, forbidden bool, count *int) error {
	switch node := relation.(type) {
	case lir.RecursiveRef:
		if node.Binding != name {
			return reject.Inputf("planner: recursive_ref names a different binding %q — mutual recursion is not supported", node.Binding)
		}
		if forbidden {
			return reject.Inputf("planner: recursive_ref appears in a non-monotone position (under an aggregate, slice, the nullable side of a left join, or a crossing) — the step must be monotone in the frontier")
		}
		*count += 1
		return nil
	case lir.Ref:
		if node.Binding == name {
			return reject.Inputf("planner: the step observes the binding through an ordinary ref — use recursive_ref for the frontier; the completed value is only observable outside")
		}
		return nil
	case lir.Recursive:
		return reject.Inputf("planner: a recursive relation is only valid as a binding body")
	case lir.Join:
		if err := checkRecursiveStep(name, node.Left, forbidden, count); err != nil {
			return err
		}
		if err := checkRecursiveStep(name, node.Right, forbidden || node.Kind == lir.LeftJoin, count); err != nil {
			return err
		}
		return checkRecursiveStepExpr(name, node.On)
	case lir.Aggregate:
		if err := checkRecursiveStep(name, node.Input, true, count); err != nil {
			return err
		}
		for _, group := range node.Groups {
			if err := checkRecursiveStepExpr(name, group.Expr); err != nil {
				return err
			}
		}
		for _, term := range node.Terms {
			if err := checkRecursiveStepExpr(name, term.Arg); err != nil {
				return err
			}
		}
		return nil
	case lir.Order:
		if err := checkRecursiveStep(name, node.Input, forbidden, count); err != nil {
			return err
		}
		for _, term := range node.Terms {
			if err := checkRecursiveStepExpr(name, term.Expr); err != nil {
				return err
			}
		}
		return nil
	case lir.Slice:
		return checkRecursiveStep(name, node.Input, true, count)
	}

	children := lirRelationChildren(relation)
	for _, child := range children.relations {
		if err := checkRecursiveStep(name, child, forbidden, count); err != nil {
			return err
		}
	}
	for _, child := range children.expressions {
		if err := checkRecursiveStepExpr(name, child); err != nil {
			return err
		}
	}
	return nil
}

func checkRecursiveStepExpr(name string, expression lir.Expr) error {
	children := lirExpressionChildren(expression)
	for _, child := range children.expressions {
		if err := checkRecursiveStepExpr(name, child); err != nil {
			return err
		}
	}
	for _, child := range children.relations {
		var ignored int
		if err := checkRecursiveStep(name, child, true, &ignored); err != nil {
			return err
		}
	}
	return nil
}
