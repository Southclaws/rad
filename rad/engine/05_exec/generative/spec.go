// Package generative synthesises typed, schema-aware LIR: a random catalog,
// random data for it, and bind-valid queries that are correct by construction.
// It is storage-free and engine-free — it produces LIR values and plain row
// data, leaving execution and comparison to the caller. The generator tracks
// each relation's output schema as it builds and only emits legal children
// (typed literals, unique output names, an order where one is required, join
// sides that can't see each other), so a bind failure downstream is a generator
// bug rather than an expected error path.
package generative

import (
	"fmt"
	"math/rand"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
)

// Column is one column in a synthesised table: a name, a scalar type, and
// whether it admits nulls.
type Column struct {
	Name     string
	Type     catalog.Type
	Nullable bool
}

// Table is one synthesised table. Columns[0] is always the text primary key
// "id". IndexOn, when non-empty, names a column carrying a non-unique secondary
// index. FKCol, when non-empty, names a nullable text foreign-key column (also
// present in Columns) referencing FKParent's id.
type Table struct {
	Name     string
	Columns  []Column
	IndexOn  string
	FKCol    string
	FKParent string
}

// Catalog is a synthesised schema: an ordered set of tables. Order is
// significant — foreign keys only ever reference earlier tables, and data must
// be generated and inserted in this order.
type Catalog struct {
	Tables []Table
}

// scalarTypes are the column types the generator draws from.
var scalarTypes = []catalog.Type{catalog.TypeText, catalog.TypeInt64, catalog.TypeFloat64, catalog.TypeBool}

// col returns the named column of t, if present.
func (t Table) col(name string) (Column, bool) {
	for _, c := range t.Columns {
		if c.Name == name {
			return c, true
		}
	}
	return Column{}, false
}

// SynthCatalog draws a random schema: 2..4 tables, each with a text "id" PK and
// 1..3 typed value columns. A table may carry a nullable foreign key to an
// earlier table (so joins and grouping can find matches) and a non-unique
// secondary index on one value column (so the planner has a range-scan access
// path to exercise path-independence).
func SynthCatalog(rng *rand.Rand) *Catalog {
	n := 2 + rng.Intn(3) // 2..4 tables
	cat := &Catalog{}
	for i := 0; i < n; i++ {
		tbl := Table{Name: fmt.Sprintf("t%d", i)}
		tbl.Columns = append(tbl.Columns, Column{Name: "id", Type: catalog.TypeText})
		extra := 1 + rng.Intn(3) // 1..3 value columns
		for j := 0; j < extra; j++ {
			tbl.Columns = append(tbl.Columns, Column{
				Name:     fmt.Sprintf("c%d", j),
				Type:     scalarTypes[rng.Intn(len(scalarTypes))],
				Nullable: rng.Intn(2) == 0,
			})
		}
		// A nullable FK to an earlier table lets joins/grouping find matches.
		if i > 0 && rng.Intn(2) == 0 {
			parent := cat.Tables[rng.Intn(i)]
			tbl.FKCol = "fk"
			tbl.FKParent = parent.Name
			tbl.Columns = append(tbl.Columns, Column{Name: "fk", Type: catalog.TypeText, Nullable: true})
		}
		// A non-unique secondary index gives the planner a range-scan option
		// (exercises path-independence) without risking insert rejection.
		if extra > 0 && rng.Intn(2) == 0 {
			tbl.IndexOn = tbl.Columns[1+rng.Intn(extra)].Name
		}
		cat.Tables = append(cat.Tables, tbl)
	}
	return cat
}
