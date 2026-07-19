package refexec

import (
	"math"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
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
	fields  []lir.Field
	buckets map[uint64][]oracleRow
}

// oracleRow is a fully decoded row: one cell per identity field, in field order.
type oracleRow []oracleCell

// oracleCell is one decoded datum. A NULL carries no value; a non-NULL carries
// its type and the one matching field, floats as raw bits so identity is by
// representation.
type oracleCell struct {
	null  bool
	typ   model.Type
	text  string
	int64 int64
	float uint64
	boolv bool
}

func newOracleRowSet(fields []lir.Field) *oracleRowSet {
	return &oracleRowSet{fields: fields, buckets: map[uint64][]oracleRow{}}
}

// canon decodes a row's identity fields into an oracleRow. A field whose slot
// is absent decodes as NULL, matching how a missing datum reads elsewhere.
func (s *oracleRowSet) canon(env lireval.Env) oracleRow { return decodeOracleRow(s.fields, env) }

// decodeOracleRow decodes a row's identity over the given fields, in order.
func decodeOracleRow(fields []lir.Field, env lireval.Env) oracleRow {
	row := make(oracleRow, len(fields))
	for i, f := range fields {
		d, ok := env[f.Slot]
		if !ok || d.Kind == lir.DatumNull {
			row[i] = oracleCell{null: true}
			continue
		}
		v := d.Scalar
		c := oracleCell{typ: v.Type}
		switch v.Type {
		case model.TypeText:
			c.text = v.Text
		case model.TypeInt64:
			c.int64 = v.Int64
		case model.TypeFloat64:
			c.float = math.Float64bits(v.Float64)
		case model.TypeBool:
			c.boolv = v.Bool
		}
		row[i] = c
	}
	return row
}

// oracleRowMultiset counts row occurrences under the oracle's own identity,
// for the bag set operations. Like oracleRowSet it shares no code with the
// production primitives; probing accepts an already-decoded oracleRow, so
// one side's rows (decoded over its own fields) test against the other
// side's occurrences — identity is cell values in position, never slots.
type oracleRowMultiset struct {
	buckets map[uint64][]*oracleRowCount
}

type oracleRowCount struct {
	row   oracleRow
	count int
}

func newOracleRowMultiset() *oracleRowMultiset {
	return &oracleRowMultiset{buckets: map[uint64][]*oracleRowCount{}}
}

func (m *oracleRowMultiset) find(row oracleRow) *oracleRowCount {
	for _, entry := range m.buckets[row.hash()] {
		if entry.row.equal(row) {
			return entry
		}
	}
	return nil
}

// add records one occurrence of row.
func (m *oracleRowMultiset) add(row oracleRow) {
	if entry := m.find(row); entry != nil {
		entry.count++
		return
	}
	h := row.hash()
	m.buckets[h] = append(m.buckets[h], &oracleRowCount{row: row, count: 1})
}

// take consumes one remaining occurrence of row, reporting whether it did.
func (m *oracleRowMultiset) take(row oracleRow) bool {
	entry := m.find(row)
	if entry == nil || entry.count == 0 {
		return false
	}
	entry.count--
	return true
}

// contains reports whether at least one occurrence of row remains.
func (m *oracleRowMultiset) contains(row oracleRow) bool {
	entry := m.find(row)
	return entry != nil && entry.count > 0
}

// add records env's identity and reports whether it was newly seen. The hash
// only buckets candidates for comparison; equal is the authority, so a weak or
// even wrong hash could at worst split equal rows across buckets and make the
// oracle over-count — a loud differential mismatch, never a silent false match.
func (s *oracleRowSet) add(env lireval.Env) bool {
	cand := s.canon(env)
	h := cand.hash()
	for _, seen := range s.buckets[h] {
		if seen.equal(cand) {
			return false
		}
	}
	s.buckets[h] = append(s.buckets[h], cand)
	return true
}

// hash is an independent FNV-1a over the decoded cells, consistent with equal:
// rows equal cell-for-cell hash the same. It exists only to bucket membership
// comparisons, not to define identity.
func (r oracleRow) hash() uint64 {
	const (
		offset = 1469598103934665603
		prime  = 1099511628211
	)
	h := uint64(offset)
	mix := func(b byte) { h ^= uint64(b); h *= prime }
	mixU64 := func(v uint64) {
		for i := range 8 {
			mix(byte(v >> (8 * i)))
		}
	}
	for _, c := range r {
		if c.null {
			mix(0)
			continue
		}
		switch c.typ {
		case model.TypeText:
			mix(1)
			mixU64(uint64(len(c.text)))
			for i := 0; i < len(c.text); i++ {
				mix(c.text[i])
			}
		case model.TypeInt64:
			mix(2)
			mixU64(uint64(c.int64))
		case model.TypeFloat64:
			mix(3)
			mixU64(c.float)
		case model.TypeBool:
			mix(4)
			if c.boolv {
				mix(1)
			} else {
				mix(0)
			}
		}
	}
	return h
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
		case model.TypeText:
			if a.text != b.text {
				return false
			}
		case model.TypeInt64:
			if a.int64 != b.int64 {
				return false
			}
		case model.TypeFloat64:
			if a.float != b.float {
				return false
			}
		case model.TypeBool:
			if a.boolv != b.boolv {
				return false
			}
		}
	}
	return true
}
