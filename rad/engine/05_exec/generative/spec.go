// Package generative synthesises typed, schema-aware LIR: a random catalog,
// random data for it, and bind-valid queries that are correct by construction.
// It is storage-free and engine-free — it produces LIR values and plain row
// data, leaving execution and comparison to the caller. The generator tracks
// each relation's output schema as it builds and only emits legal children
// (typed literals, unique output names, an order where one is required, join
// sides that can't see each other), so a bind failure downstream is a generator
// bug rather than an expected error path.
//
// A spec describes any Rad schema, synthetic or introspected: primary keys of
// any arity and type, secondary unique and non-unique indexes, and foreign keys
// over arbitrary columns. Data generation and query generation both work off
// that general shape.
package generative

import (
	"fmt"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"pgregory.net/rapid"
)

// Column is one column: a name, a scalar type, and whether it admits nulls.
type Column struct {
	Name     string
	Type     catalog.Type
	Nullable bool
}

// FK is a foreign key: local columns referencing a parent table's columns
// (positionally). The referenced columns form a key of the parent, so a value
// drawn from any parent row satisfies the constraint.
type FK struct {
	Cols       []string
	Parent     string
	ParentCols []string
}

// Table is one table. PrimaryKey names its key columns (arity ≥ 1, any types).
// Uniques and Indexes are secondary column-sets (unique and non-unique); FKs
// are its foreign keys. A table's foreign keys reference only tables earlier in
// the catalog (or itself), so data can be generated in catalog order.
type Table struct {
	Name       string
	Columns    []Column
	PrimaryKey []string
	Uniques    [][]string
	Indexes    [][]string
	FKs        []FK
}

// Catalog is a schema: an ordered set of tables. Order is significant — foreign
// keys reference earlier tables, and data is generated and inserted in order.
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

// colType returns the scalar type of the named column (text if absent, which
// never happens for a name the caller took from the table).
func (t Table) colType(name string) catalog.Type {
	c, _ := t.col(name)
	return c.Type
}

// allNullable reports whether every named column admits nulls — the condition
// under which a foreign key can be left null.
func (t Table) allNullable(cols []string) bool {
	for _, name := range cols {
		if c, ok := t.col(name); !ok || !c.Nullable {
			return false
		}
	}
	return true
}

// SynthConfig bounds the shape of a synthesised catalog: how many tables, and
// how many typed value columns each carries beyond its "id" key. Widen it to
// stress the engine with larger schemas; DefaultSynthConfig is the modest shape
// used by the standard runs.
type SynthConfig struct {
	MinTables    int
	MaxTables    int
	MinValueCols int
	MaxValueCols int
}

// DefaultSynthConfig is the standard synthetic shape: 2..4 tables, each with
// 1..3 value columns.
func DefaultSynthConfig() SynthConfig {
	return SynthConfig{MinTables: 2, MaxTables: 8, MinValueCols: 1, MaxValueCols: 10}
}

// SynthCatalog draws a random schema with the default bounds. Each table has a
// text "id" PK and some typed value columns; a table may carry a nullable
// foreign key to an earlier table (so joins and grouping can find matches) and
// a non-unique secondary index on one value column (so the planner has a
// range-scan access path to exercise path-independence). Awkward shapes —
// composite or non-text keys, unique secondaries, multi-column foreign keys —
// are covered by the schema-directed fixtures rather than synthesised here.
func SynthCatalog(t *rapid.T) *Catalog {
	return SynthCatalogWith(t, DefaultSynthConfig())
}

// SynthCatalogWith draws a random schema within cfg's bounds.
func SynthCatalogWith(t *rapid.T, cfg SynthConfig) *Catalog {
	n := rapid.IntRange(cfg.MinTables, cfg.MaxTables).Draw(t, "tables")
	cat := &Catalog{}
	for i := 0; i < n; i++ {
		tbl := Table{Name: fmt.Sprintf("t%d", i), PrimaryKey: []string{"id"}}
		tbl.Columns = append(tbl.Columns, Column{Name: "id", Type: catalog.TypeText})
		extra := rapid.IntRange(cfg.MinValueCols, cfg.MaxValueCols).Draw(t, "value_cols")
		for j := 0; j < extra; j++ {
			tbl.Columns = append(tbl.Columns, Column{
				Name:     fmt.Sprintf("c%d", j),
				Type:     rapid.SampledFrom(scalarTypes).Draw(t, "col_type"),
				Nullable: rapid.Bool().Draw(t, "col_nullable"),
			})
		}
		if i > 0 && rapid.Bool().Draw(t, "has_fk") {
			parent := rapid.SampledFrom(cat.Tables).Draw(t, "fk_parent")
			tbl.Columns = append(tbl.Columns, Column{Name: "fk", Type: catalog.TypeText, Nullable: true})
			tbl.FKs = append(tbl.FKs, FK{Cols: []string{"fk"}, Parent: parent.Name, ParentCols: []string{"id"}})
		}
		if extra > 0 && rapid.Bool().Draw(t, "has_index") {
			col := tbl.Columns[1+rapid.IntRange(0, extra-1).Draw(t, "index_col")].Name
			tbl.Indexes = append(tbl.Indexes, []string{col})
		}
		cat.Tables = append(cat.Tables, tbl)
	}
	return cat
}
