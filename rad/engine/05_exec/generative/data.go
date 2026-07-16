package generative

import (
	"fmt"
	"math"
	"math/rand"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// GenerateData produces random rows for every table in spec, keyed by table
// name. Each table gets 0..5 rows (sometimes empty). The "id" column is a
// deterministic "<table>_<i>" text key; a foreign-key column is either null or
// a reference drawn from an already-generated parent row's id; every other
// column draws a random typed value. Rows are generated in spec.Tables order so
// a foreign key always sees its parent's ids, and the caller must insert in the
// same order for referential integrity.
func GenerateData(rng *rand.Rand, spec *Catalog) map[string][]lir.Row {
	out := map[string][]lir.Row{}
	ids := map[string][]string{} // table -> generated ids, for FK refs
	for _, tbl := range spec.Tables {
		rows := rng.Intn(6) // 0..5 rows, sometimes empty
		for i := 0; i < rows; i++ {
			row := lir.Row{}
			for _, c := range tbl.Columns {
				switch {
				case c.Name == "id":
					row["id"] = lir.Text(fmt.Sprintf("%s_%d", tbl.Name, i))
				case c.Name == tbl.FKCol:
					parents := ids[tbl.FKParent]
					if len(parents) == 0 || rng.Intn(3) == 0 {
						row[c.Name] = lir.Null(catalog.TypeText)
					} else {
						row[c.Name] = lir.Text(parents[rng.Intn(len(parents))])
					}
				default:
					row[c.Name] = genValue(rng, c)
				}
			}
			out[tbl.Name] = append(out[tbl.Name], row)
			ids[tbl.Name] = append(ids[tbl.Name], fmt.Sprintf("%s_%d", tbl.Name, i))
		}
	}
	return out
}

// genValue draws a random value for a value column: a null a fraction of the
// time when the column is nullable, otherwise a value from a small fixed set
// per type (including type extremes for int64) so edge cases recur.
func genValue(rng *rand.Rand, c Column) lir.Value {
	if c.Nullable && rng.Intn(10) < 3 {
		return lir.Null(c.Type)
	}
	switch c.Type {
	case catalog.TypeText:
		return lir.Text([]string{"a", "b", "c", ""}[rng.Intn(4)])
	case catalog.TypeInt64:
		return lir.Int64([]int64{math.MinInt64, -2, -1, 0, 1, 2, 100, math.MaxInt64}[rng.Intn(8)])
	case catalog.TypeFloat64:
		return lir.Float64([]float64{-1.5, 0, 1.5, 2.5}[rng.Intn(4)])
	default:
		return lir.Bool(rng.Intn(2) == 0)
	}
}
