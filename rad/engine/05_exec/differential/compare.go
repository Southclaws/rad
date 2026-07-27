package differential

import (
	"encoding/json"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// multiset renders an array-datum result as a count per canonical-JSON row, so
// results compare order-insensitively. A non-array datum contributes a single
// entry.
func multiset(d lir.Datum) map[string]int {
	m := map[string]int{}
	if d.Kind != lir.DatumArray {
		b, _ := json.Marshal(jsonish(d))
		m[string(b)]++
		return m
	}
	for _, e := range d.Elems {
		b, _ := json.Marshal(jsonish(e))
		m[string(b)]++
	}
	return m
}

// sameMultiset reports whether two multisets have identical keys and counts.
func sameMultiset(a, b map[string]int) bool {
	if len(a) != len(b) {
		return false
	}
	for k, v := range a {
		if b[k] != v {
			return false
		}
	}
	return true
}

// seqJSON renders a datum as canonical JSON, order-sensitively (array order
// preserved), for exact row-sequence comparison.
func seqJSON(d lir.Datum) string {
	b, _ := json.Marshal(jsonish(d))
	return string(b)
}

// jsonish flattens a Datum into plain Go values (nil, scalars, map, slice) for
// readable, comparable JSON.
func jsonish(d lir.Datum) any {
	switch d.Kind {
	case lir.DatumNull:
		return nil
	case lir.DatumScalar:
		v := d.Scalar
		switch v.Type {
		case model.TypeText:
			return v.Text
		case model.TypeInt64:
			return v.Int64
		case model.TypeFloat64:
			return v.Float64
		case model.TypeBool:
			return v.Bool
		}
		return v
	case lir.DatumObject:
		m := map[string]any{}
		for _, f := range d.Fields {
			m[f.Name] = jsonish(f.Datum)
		}
		return m
	default:
		out := make([]any, len(d.Elems))
		for i, e := range d.Elems {
			out[i] = jsonish(e)
		}
		return out
	}
}
