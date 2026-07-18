package generative

import (
	"fmt"
	"slices"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

// Introspect converts a migrated catalog into a generator spec, covering any
// Rad schema: primary keys of any arity and type, secondary unique and
// non-unique indexes, and foreign keys over arbitrary columns. Tables come in
// catalog (creation) order, so foreign keys reference earlier tables and data
// can be generated in order.
func Introspect(tables []model.Table) *Catalog {
	nameByID := make(map[string]string, len(tables))
	for _, t := range tables {
		nameByID[t.ID] = t.Name
	}

	cat := &Catalog{}
	for _, t := range tables {
		tb := Table{Name: t.Name, PrimaryKey: t.PrimaryKey}
		for _, c := range t.Columns {
			tb.Columns = append(tb.Columns, Column{Name: c.Name, Type: c.Type, Nullable: c.Nullable})
		}
		for _, idx := range t.Indexes {
			if slices.Equal(idx.Columns, t.PrimaryKey) {
				continue // the primary-key index, not a secondary one
			}
			if idx.Unique {
				tb.Uniques = append(tb.Uniques, idx.Columns)
			} else {
				tb.Indexes = append(tb.Indexes, idx.Columns)
			}
		}
		for _, fk := range t.ForeignKeys {
			tb.FKs = append(tb.FKs, FK{
				Cols:       fk.Columns,
				Parent:     nameByID[fk.RefTableID],
				ParentCols: fk.RefColumns,
			})
		}
		cat.Tables = append(cat.Tables, tb)
	}
	return cat
}

// TableDefs renders a spec as catalog table definitions in the spec's order —
// each foreign key references an earlier table — ready for CreateTable. It is
// the inverse of what a migrated schema yields, letting a synthetic spec build
// a database without a schema file.
func TableDefs(spec *Catalog) []model.TableDef {
	defs := make([]model.TableDef, 0, len(spec.Tables))
	for _, t := range spec.Tables {
		def := model.TableDef{Name: t.Name, PrimaryKey: t.PrimaryKey}
		for _, c := range t.Columns {
			def.Columns = append(def.Columns, model.ColumnDef{Name: c.Name, Type: c.Type, Nullable: c.Nullable})
		}
		for i, u := range t.Uniques {
			def.Indexes = append(def.Indexes, model.IndexDef{Name: fmt.Sprintf("%s_u%d", t.Name, i), Columns: u, Unique: true})
		}
		for i, idx := range t.Indexes {
			def.Indexes = append(def.Indexes, model.IndexDef{Name: fmt.Sprintf("%s_i%d", t.Name, i), Columns: idx})
		}
		for i, fk := range t.FKs {
			def.ForeignKeys = append(def.ForeignKeys, model.ForeignKeyDef{
				Name: fmt.Sprintf("%s_fk%d", t.Name, i), Columns: fk.Cols,
				RefTable: fk.Parent, RefColumns: fk.ParentCols,
			})
		}
		defs = append(defs, def)
	}
	return defs
}
