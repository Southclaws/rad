package generative

import (
	"fmt"
	"math"
	"strings"

	"pgregory.net/rapid"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// GenerateData produces random rows for every table in spec, keyed by table
// name and generated in catalog order so a foreign key always sees its parent's
// rows. Each table gets up to 5 candidate rows; a candidate is dropped (not an
// error — the caller must be able to insert every row) when it would violate
// the primary key, a unique index (NULL-distinct: an index with a null
// component is not enforced), or a non-null foreign key with no parent to point
// at. The caller inserts in the same order for referential integrity.
func GenerateData(t *rapid.T, spec *Catalog) map[string][]lir.Row {
	out := map[string][]lir.Row{}
	for _, tbl := range spec.Tables {
		n := rapid.IntRange(0, 5).Draw(t, "rows")
		seenPK := map[string]bool{}
		seenUnique := make([]map[string]bool, len(tbl.Uniques))
		for i := range seenUnique {
			seenUnique[i] = map[string]bool{}
		}
		for i := 0; i < n; i++ {
			row, ok := genRow(t, tbl, out, i)
			if !ok {
				continue
			}
			pk := tupleKey(row, tbl.PrimaryKey)
			if seenPK[pk] {
				continue
			}
			ukeys := make([]string, len(tbl.Uniques))
			hasNull := make([]bool, len(tbl.Uniques))
			collide := false
			for ui, u := range tbl.Uniques {
				ukeys[ui], hasNull[ui] = uniqueKey(row, u)
				if !hasNull[ui] && seenUnique[ui][ukeys[ui]] {
					collide = true
					break
				}
			}
			if collide {
				continue
			}
			seenPK[pk] = true
			for ui := range tbl.Uniques {
				if !hasNull[ui] {
					seenUnique[ui][ukeys[ui]] = true
				}
			}
			out[tbl.Name] = append(out[tbl.Name], row)
		}
	}
	return out
}

// genRow builds one candidate row for tbl at index i, or ok=false when a
// non-null foreign key cannot be satisfied (its parent has no rows yet).
// Foreign keys are assigned first — copying a parent row's referenced values,
// or null when the key admits it; primary-key columns then take a value
// distinct per i; every other column draws a random typed value.
func genRow(t *rapid.T, tbl Table, out map[string][]lir.Row, i int) (lir.Row, bool) {
	row := lir.Row{}
	assigned := map[string]bool{}
	for _, fk := range tbl.FKs {
		parents := out[fk.Parent]
		nullable := tbl.allNullable(fk.Cols)
		switch {
		case len(parents) == 0:
			if !nullable {
				return nil, false
			}
			for _, c := range fk.Cols {
				row[c] = lir.Null(tbl.colType(c))
				assigned[c] = true
			}
		case nullable && rapid.IntRange(0, 2).Draw(t, "fk_null") == 0:
			for _, c := range fk.Cols {
				row[c] = lir.Null(tbl.colType(c))
				assigned[c] = true
			}
		default:
			pr := rapid.SampledFrom(parents).Draw(t, "fk_ref")
			for j, c := range fk.Cols {
				row[c] = pr[fk.ParentCols[j]]
				assigned[c] = true
			}
		}
	}
	pk := nameSet(tbl.PrimaryKey)
	for _, c := range tbl.Columns {
		if assigned[c.Name] {
			continue
		}
		if pk[c.Name] {
			row[c.Name] = pkValue(c, i)
		} else {
			row[c.Name] = genValue(t, c)
		}
	}
	return row, true
}

// pkValue gives a primary-key column a value distinct per row index, so rows
// stay unique on the key without leaning on the random value pool (which, for a
// small type like bool, would collide almost immediately).
func pkValue(c Column, i int) lir.Value {
	switch c.Type {
	case model.TypeText:
		return lir.Text(fmt.Sprintf("k%d", i))
	case model.TypeInt64:
		return lir.Int64(int64(i))
	case model.TypeFloat64:
		return lir.Float64(float64(i))
	default:
		return lir.Bool(i%2 == 0)
	}
}

// genValue draws a random value for a value column: a null a fraction of the
// time when the column is nullable, otherwise a value from a small fixed set
// per type (including type extremes for int64) so edge cases recur.
func genValue(t *rapid.T, c Column) lir.Value {
	if c.Nullable && rapid.IntRange(0, 9).Draw(t, "value_null") < 3 {
		return lir.Null(c.Type)
	}
	switch c.Type {
	case model.TypeText:
		return lir.Text(rapid.SampledFrom([]string{"a", "b", "c", ""}).Draw(t, "text"))
	case model.TypeInt64:
		return lir.Int64(rapid.SampledFrom([]int64{math.MinInt64, -2, -1, 0, 1, 2, 100, math.MaxInt64}).Draw(t, "int64"))
	case model.TypeFloat64:
		return lir.Float64(rapid.SampledFrom([]float64{-1.5, 0, 1.5, 2.5}).Draw(t, "float64"))
	default:
		return lir.Bool(rapid.Bool().Draw(t, "bool"))
	}
}

func nameSet(cols []string) map[string]bool {
	s := make(map[string]bool, len(cols))
	for _, c := range cols {
		s[c] = true
	}
	return s
}

// tupleKey renders the named columns of a row as a canonical string, for
// primary-key deduplication.
func tupleKey(row lir.Row, cols []string) string {
	var b strings.Builder
	for _, c := range cols {
		b.WriteString(row[c].String())
		b.WriteByte(0)
	}
	return b.String()
}

// uniqueKey renders the named columns and reports whether any is null — a null
// component exempts the row from a unique index (NULL-distinct semantics).
func uniqueKey(row lir.Row, cols []string) (string, bool) {
	var b strings.Builder
	anyNull := false
	for _, c := range cols {
		v := row[c]
		if v.Null {
			anyNull = true
		}
		b.WriteString(v.String())
		b.WriteByte(0)
	}
	return b.String(), anyNull
}
