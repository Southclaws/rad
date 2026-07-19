package eval

import lir "github.com/Southclaws/rad/rad/engine/03_lir"

// CanonicalRowSet gives distinct and recursive accumulation the same full-row
// identity. Field order and scalar types are significant, and NULL equals NULL
// rather than following three-valued predicate equality.
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

func (s *CanonicalRowSet) key(row Env) string { return CanonicalRowKey(s.fields, row) }

// CanonicalRowKey renders a row's canonical full-row identity over the given
// fields' slots, in order: the identity distinct, recursive accumulation, and
// the bag set operations share. The key is value-based, so rows from two
// positionally compatible relations — different slots, same column shape —
// produce comparable keys, which is what lets intersect and except probe one
// side's multiset with the other side's rows.
func CanonicalRowKey(fields []lir.Field, row Env) string {
	var key []byte
	for _, f := range fields {
		d, ok := row[f.Slot]
		if !ok || d.Kind == lir.DatumNull {
			key = append(key, "|N"...)
			continue
		}
		key = d.Scalar.AppendIdentity(key)
	}
	return string(key)
}
