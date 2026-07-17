package refexec

import (
	"math"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

// oracleRowSet is refexec's own definition of canonical full-row identity,
// written to be deliberately dull and to share no code with the production
// bound.CanonicalRowSet. It decodes each admitted row into typed cells and
// tests a candidate by comparing cell against cell — NULL equal to NULL, kinds
// and column order significant, floats by bit pattern. Independence is the
// whole point: an oracle that called the production primitive could only ever
// agree with it, so a bug in canonical identity — composite keys, typed-NULL
// equality, missing slots — would sit in both and the differential would stay
// green. A linear membership scan is fine; refexec runs over small test data.
type oracleRowSet struct {
	fields []lir.Field
	rows   []oracleRow
}

// oracleRow is a fully decoded row: one cell per identity field, in field order.
type oracleRow []oracleCell

// oracleCell is one decoded datum. A NULL carries no value; a non-NULL carries
// its type and the one matching field, floats as raw bits so identity is by
// representation.
type oracleCell struct {
	null  bool
	typ   catalog.Type
	text  string
	int64 int64
	float uint64
	boolv bool
}

func newOracleRowSet(fields []lir.Field) *oracleRowSet {
	return &oracleRowSet{fields: fields}
}

// canon decodes a row's identity fields into an oracleRow. A field whose slot
// is absent decodes as NULL, matching how a missing datum reads elsewhere.
func (s *oracleRowSet) canon(env bound.Env) oracleRow {
	row := make(oracleRow, len(s.fields))
	for i, f := range s.fields {
		d, ok := env[f.Slot]
		if !ok || d.Kind == lir.DatumNull {
			row[i] = oracleCell{null: true}
			continue
		}
		v := d.Scalar
		c := oracleCell{typ: v.Type}
		switch v.Type {
		case catalog.TypeText:
			c.text = v.Text
		case catalog.TypeInt64:
			c.int64 = v.Int64
		case catalog.TypeFloat64:
			c.float = math.Float64bits(v.Float64)
		case catalog.TypeBool:
			c.boolv = v.Bool
		}
		row[i] = c
	}
	return row
}

// add records env's identity and reports whether it was newly seen.
func (s *oracleRowSet) add(env bound.Env) bool {
	cand := s.canon(env)
	for _, seen := range s.rows {
		if seen.equal(cand) {
			return false
		}
	}
	s.rows = append(s.rows, cand)
	return true
}

// equal is element-wise typed equality between two decoded rows.
func (r oracleRow) equal(o oracleRow) bool {
	if len(r) != len(o) {
		return false
	}
	for i := range r {
		a, b := r[i], o[i]
		if a.null || b.null {
			if a.null != b.null {
				return false
			}
			continue
		}
		if a.typ != b.typ {
			return false
		}
		switch a.typ {
		case catalog.TypeText:
			if a.text != b.text {
				return false
			}
		case catalog.TypeInt64:
			if a.int64 != b.int64 {
				return false
			}
		case catalog.TypeFloat64:
			if a.float != b.float {
				return false
			}
		case catalog.TypeBool:
			if a.boolv != b.boolv {
				return false
			}
		}
	}
	return true
}
