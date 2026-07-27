package bind

import (
	"fmt"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func rowTypeEqual(a, b lir.RowType) bool {
	if len(a.Fields) != len(b.Fields) {
		return false
	}
	for i := range a.Fields {
		x, y := a.Fields[i], b.Fields[i]
		if x.Name != y.Name || !typeEqual(x.Type, y.Type) {
			return false
		}
	}
	return true
}

func typeEqual(a, b lir.Type) bool {
	if a.Kind != b.Kind || a.Nullable != b.Nullable {
		return false
	}
	switch a.Kind {
	case lir.KindRow:
		if a.Row == nil || b.Row == nil {
			return a.Row == nil && b.Row == nil
		}
		return rowTypeEqual(*a.Row, *b.Row)
	case lir.KindArray:
		if a.Elem == nil || b.Elem == nil {
			return a.Elem == nil && b.Elem == nil
		}
		return typeEqual(*a.Elem, *b.Elem)
	default:
		return true
	}
}

func recursiveOutputUnique(out lir.RowType) error {
	if duplicate, ok := duplicateColumn(out); ok {
		return reject.Inputf("planner: recursive anchor output has duplicate column %q — project it to a unique set of columns", duplicate)
	}
	return nil
}

func duplicateColumn(out lir.RowType) (string, bool) {
	seen := map[string]bool{}
	for _, field := range out.Fields {
		if seen[field.Name] {
			return field.Name, true
		}
		seen[field.Name] = true
	}
	return "", false
}

func reconcileRecursive(anchor, step lir.RowType) (lir.RowType, error) {
	if len(anchor.Fields) != len(step.Fields) {
		return lir.RowType{}, reject.Inputf("planner: recursive anchor produces %d columns but the step produces %d — anchor and step must produce the same columns", len(anchor.Fields), len(step.Fields))
	}
	out := make([]lir.Field, len(anchor.Fields))
	for i, anchorField := range anchor.Fields {
		stepField, ok := step.Lookup(anchorField.Name)
		if !ok {
			return lir.RowType{}, reject.Inputf("planner: recursive step is missing anchor column %q — anchor and step must produce the same columns", anchorField.Name)
		}
		typ, err := reconcileRecursiveType(anchorField.Type, stepField.Type, anchorField.Name)
		if err != nil {
			return lir.RowType{}, err
		}
		field := anchorField
		field.Type = typ
		out[i] = field
	}
	return lir.RowType{Fields: out}, nil
}

func reconcileRecursiveType(anchor, step lir.Type, path string) (lir.Type, error) {
	if anchor.Kind != step.Kind {
		return lir.Type{}, reject.Inputf("planner: recursive column %q is %s in the anchor but %s in the step — the kinds must match", path, anchor.Kind, step.Kind)
	}
	out := anchor
	out.Nullable = anchor.Nullable || step.Nullable

	switch anchor.Kind {
	case lir.KindRow:
		if anchor.Row == nil || step.Row == nil {
			if anchor.Row != nil || step.Row != nil {
				return lir.Type{}, reject.Inputf("planner: recursive column %q has incompatible row shapes", path)
			}
			return out, nil
		}
		row, err := reconcileNestedRow(*anchor.Row, *step.Row, path)
		if err != nil {
			return lir.Type{}, err
		}
		out.Row = &row
	case lir.KindArray:
		if anchor.Elem == nil || step.Elem == nil {
			if anchor.Elem != nil || step.Elem != nil {
				return lir.Type{}, reject.Inputf("planner: recursive column %q has incompatible array element shapes", path)
			}
			return out, nil
		}
		elem, err := reconcileRecursiveType(*anchor.Elem, *step.Elem, path+"[]")
		if err != nil {
			return lir.Type{}, err
		}
		out.Elem = &elem
	}
	return out, nil
}

func reconcileNestedRow(anchor, step lir.RowType, path string) (lir.RowType, error) {
	if len(anchor.Fields) != len(step.Fields) {
		return lir.RowType{}, reject.Inputf("planner: recursive column %q has incompatible row shapes", path)
	}
	out := make([]lir.Field, len(anchor.Fields))
	for i, anchorField := range anchor.Fields {
		stepField, ok := step.Lookup(anchorField.Name)
		if !ok {
			return lir.RowType{}, reject.Inputf("planner: recursive column %q has incompatible row shapes", path)
		}
		fieldPath := fmt.Sprintf("%s.%s", path, anchorField.Name)
		typ, err := reconcileRecursiveType(anchorField.Type, stepField.Type, fieldPath)
		if err != nil {
			return lir.RowType{}, err
		}
		field := anchorField
		field.Type = typ
		out[i] = field
	}
	return lir.RowType{Fields: out}, nil
}
