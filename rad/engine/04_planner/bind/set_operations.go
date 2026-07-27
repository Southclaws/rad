package bind

import (
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// Binding for the set-operation family: concatenate, intersect, except. All
// three share the positional-compatibility contract — inputs bind in order,
// must agree on arity and per-position column name and scalar kind, and are
// independent of one another (no input may reference a sibling's scopes,
// since each executes on its own and only remapped output rows exist above
// the node). The output row type is fresh: new slots under the node's own
// scope, and the inputs' scopes close here, exactly as a projection's do.

// bindConcatenate binds the n-ary bag concatenation. Output nullability
// widens per position: nullable when any input's column is.
func (b *binder) bindConcatenate(n lir.Concatenate) (*bound.Concatenate, error) {
	if len(n.Inputs) < 2 {
		return nil, reject.Inputf("planner: concatenate needs at least two inputs, got %d", len(n.Inputs))
	}
	mark := len(b.scopes)
	ins, err := b.bindSetOperationInputs("concatenate", n.Scope, n.Inputs)
	if err != nil {
		return nil, err
	}
	if err := compatibleSetOperationInputs("concatenate", ins); err != nil {
		return nil, err
	}

	first := ins[0].Output().Fields
	slots := b.freshSlots(len(first))
	out := make([]lir.Field, len(first))
	for k, f := range first {
		nullable := f.Type.Nullable
		for _, in := range ins[1:] {
			nullable = nullable || in.Output().Fields[k].Type.Nullable
		}
		out[k] = lir.Field{Name: f.Name, Slot: slots[k], Type: lir.Type{Kind: f.Type.Kind, Nullable: nullable}}
	}

	b.scopes = b.scopes[:mark]
	c := bound.NewConcatenate(ins, n.Scope, out)
	if err := b.exposeScope(n.Scope, c); err != nil {
		return nil, err
	}
	return c, nil
}

// bindIntersect binds the bag intersection. Every output row is drawn from
// the left input, so the output columns carry the left side's types.
func (b *binder) bindIntersect(n lir.Intersect) (*bound.Intersect, error) {
	ins, out, err := b.bindBinarySetOperation("intersect", n.Scope, n.Quantifier, n.Left, n.Right)
	if err != nil {
		return nil, err
	}
	i := bound.NewIntersect(ins[0], ins[1], n.Quantifier, n.Scope, out)
	if err := b.exposeScope(n.Scope, i); err != nil {
		return nil, err
	}
	return i, nil
}

// bindExcept binds the bag difference. Every output row is drawn from the
// left input, so the output columns carry the left side's types.
func (b *binder) bindExcept(n lir.Except) (*bound.Except, error) {
	ins, out, err := b.bindBinarySetOperation("except", n.Scope, n.Quantifier, n.Left, n.Right)
	if err != nil {
		return nil, err
	}
	e := bound.NewExcept(ins[0], ins[1], n.Quantifier, n.Scope, out)
	if err := b.exposeScope(n.Scope, e); err != nil {
		return nil, err
	}
	return e, nil
}

// bindBinarySetOperation is the shared core of intersect and except: bind
// and verify both sides, then build the fresh output row type from the left
// side's fields.
func (b *binder) bindBinarySetOperation(op, scope string, quantifier lir.SetQuantifier, left, right lir.Relation) ([]bound.Relation, []lir.Field, error) {
	switch quantifier {
	case lir.QuantifierAll, lir.QuantifierDistinct:
	default:
		return nil, nil, reject.Inputf("planner: unknown %s quantifier %q", op, quantifier)
	}
	mark := len(b.scopes)
	ins, err := b.bindSetOperationInputs(op, scope, []lir.Relation{left, right})
	if err != nil {
		return nil, nil, err
	}
	if err := compatibleSetOperationInputs(op, ins); err != nil {
		return nil, nil, err
	}

	first := ins[0].Output().Fields
	slots := b.freshSlots(len(first))
	out := make([]lir.Field, len(first))
	for k, f := range first {
		out[k] = lir.Field{Name: f.Name, Slot: slots[k], Type: f.Type}
	}
	b.scopes = b.scopes[:mark]
	return ins, out, nil
}

// bindSetOperationInputs binds each input in order and rejects a reference
// into a sibling input's scopes — the same independence law as a join's
// sides, rejected the same way.
func (b *binder) bindSetOperationInputs(op, scope string, inputs []lir.Relation) ([]bound.Relation, error) {
	if scope == "" {
		return nil, reject.Inputf("planner: %s needs a scope label", op)
	}
	ins := make([]bound.Relation, len(inputs))
	var earlier bound.SlotSet
	for i, input := range inputs {
		in, err := b.bindRel(input)
		if err != nil {
			return nil, err
		}
		for _, slot := range in.FreeSlots().Slots() {
			if earlier.Contains(slot) {
				desc := setOperationSlotDesc(ins[:i], slot)
				return nil, reject.Inputf("planner: %s input %d references %s from another input; %s inputs are independent relations — correlate through an enclosing scope instead", op, i+1, desc, op)
			}
		}
		earlier = earlier.Union(in.Produced())
		ins[i] = in
	}
	return ins, nil
}

// compatibleSetOperationInputs enforces the positional-compatibility
// contract: unique column names, equal arity, and per position the same
// name and scalar kind.
func compatibleSetOperationInputs(op string, ins []bound.Relation) error {
	first := ins[0].Output().Fields
	if dup, ok := duplicateColumn(ins[0].Output()); ok {
		return reject.Inputf("planner: %s output has duplicate column %q — project each input to a unique set of columns", op, dup)
	}
	for i, in := range ins[1:] {
		fields := in.Output().Fields
		if len(fields) != len(first) {
			return reject.Inputf("planner: %s inputs must have the same number of columns: input 1 has %d, input %d has %d", op, len(first), i+2, len(fields))
		}
	}
	for k, f := range first {
		if !f.Type.Kind.Scalar() {
			return reject.Inputf("planner: %s column %d (%q) has non-scalar type %s — %s combines scalar columns", op, k+1, f.Name, f.Type.Kind, op)
		}
		for i, in := range ins[1:] {
			g := in.Output().Fields[k]
			if g.Name != f.Name {
				return reject.Inputf("planner: %s column %d is named %q in input 1 but %q in input %d — %s matches columns positionally", op, k+1, f.Name, g.Name, i+2, op)
			}
			if g.Type.Kind != f.Type.Kind {
				return reject.Inputf("planner: %s column %d (%q) is %s in input 1 but %s in input %d", op, k+1, f.Name, f.Type.Kind, g.Type.Kind, i+2)
			}
		}
	}
	return nil
}

// setOperationSlotDesc names a slot produced by any of the given relations,
// for the sibling-reference rejection message.
func setOperationSlotDesc(ins []bound.Relation, slot lir.SlotID) string {
	for _, in := range ins {
		if desc := slotDesc(in, slot); desc != "" {
			return desc
		}
	}
	return "a column"
}
