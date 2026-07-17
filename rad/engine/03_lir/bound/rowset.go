package bound

import (
	"fmt"
	"strings"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// CanonicalRowSet tracks membership by canonical full-row identity over a fixed
// set of output fields: the complete row, type- and order-significant, with
// NULL equal to NULL — deliberately unlike three-valued predicate equality
// (where NULL ≠ NULL). It is the one definition of row identity shared by the
// `distinct` operator and recursive admit-new accumulation, so those two can
// never disagree. Like EvalPred/EvalDatum, it is a value-semantics primitive
// rather than query logic, pinned by the distinct and recursion tests.
type CanonicalRowSet struct {
	fields []lir.Field
	seen   map[string]bool
}

// NewCanonicalRowSet builds a set that identifies rows by the given fields'
// slots, in order.
func NewCanonicalRowSet(fields []lir.Field) *CanonicalRowSet {
	return &CanonicalRowSet{fields: fields, seen: map[string]bool{}}
}

// Add records row and reports whether it was newly seen (true) or a duplicate
// of an already-admitted row (false).
func (s *CanonicalRowSet) Add(row Env) bool {
	k := s.key(row)
	if s.seen[k] {
		return false
	}
	s.seen[k] = true
	return true
}

// Contains reports whether an identical row has already been added.
func (s *CanonicalRowSet) Contains(row Env) bool { return s.seen[s.key(row)] }

// key encodes a row's fields for identity: each datum as a type-tagged,
// length-framed token with a distinct marker for NULL, so NULL == NULL and no
// two distinct values or kinds collide. Values are scalars in the current value
// domain.
func (s *CanonicalRowSet) key(row Env) string {
	var b strings.Builder
	for _, f := range s.fields {
		d, ok := row[f.Slot]
		if !ok || d.Kind == lir.DatumNull {
			b.WriteString("|N")
			continue
		}
		v := d.Scalar
		fmt.Fprintf(&b, "|%s:", v.Type)
		switch v.Type {
		case catalog.TypeText:
			fmt.Fprintf(&b, "%d:%s", len(v.Text), v.Text)
		case catalog.TypeInt64:
			fmt.Fprintf(&b, "%d", v.Int64)
		case catalog.TypeFloat64:
			fmt.Fprintf(&b, "%v", v.Float64)
		case catalog.TypeBool:
			fmt.Fprintf(&b, "%t", v.Bool)
		}
	}
	return b.String()
}
