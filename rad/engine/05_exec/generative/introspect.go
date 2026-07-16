package generative

import (
	"fmt"
	"slices"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
)

// Introspect converts a migrated catalog into a generator spec, or returns a
// non-empty reason it cannot — a schema whose shape the generator does not
// drive. The generator assumes every table has a single text "id" primary key,
// at most one single-column nullable text foreign key to another table's id,
// and single-column non-unique secondary indexes. A schema outside that shape
// is reported so the caller skips it rather than generating data and queries
// that would misfire against it.
func Introspect(tables []catalog.Table) (*Catalog, string) {
	nameByID := make(map[string]string, len(tables))
	for _, t := range tables {
		nameByID[t.ID] = t.Name
	}

	spec := &Catalog{}
	for _, t := range tables {
		if len(t.PrimaryKey) != 1 || t.PrimaryKey[0] != "id" {
			return nil, fmt.Sprintf("table %q: the generator requires a single primary-key column named %q", t.Name, "id")
		}

		tb := Table{Name: t.Name}
		var id *Column
		var rest []Column
		for _, c := range t.Columns {
			col := Column{Name: c.Name, Type: c.Type, Nullable: c.Nullable}
			if c.Name == "id" {
				if c.Type != catalog.TypeText {
					return nil, fmt.Sprintf("table %q: the generator requires a text %q column", t.Name, "id")
				}
				dup := col
				id = &dup
				continue
			}
			rest = append(rest, col)
		}
		if id == nil {
			return nil, fmt.Sprintf("table %q: no %q column", t.Name, "id")
		}
		// cols[0] is the id, matching the generator's own table layout.
		tb.Columns = append([]Column{*id}, rest...)

		switch len(t.ForeignKeys) {
		case 0:
		case 1:
			fk := t.ForeignKeys[0]
			if len(fk.Columns) != 1 {
				return nil, fmt.Sprintf("table %q: the generator drives only single-column foreign keys", t.Name)
			}
			col, ok := tb.col(fk.Columns[0])
			if !ok || col.Type != catalog.TypeText || !col.Nullable {
				return nil, fmt.Sprintf("table %q: the generator requires a nullable text foreign-key column", t.Name)
			}
			parent, ok := nameByID[fk.RefTableID]
			if !ok {
				return nil, fmt.Sprintf("table %q: foreign key references an unknown table", t.Name)
			}
			tb.FKCol = fk.Columns[0]
			tb.FKParent = parent
		default:
			return nil, fmt.Sprintf("table %q: the generator drives at most one foreign key", t.Name)
		}

		for _, idx := range t.Indexes {
			if slices.Equal(idx.Columns, t.PrimaryKey) {
				continue // the primary-key index, not a secondary one
			}
			if idx.Unique {
				return nil, fmt.Sprintf("table %q: the generator does not drive unique secondary indexes (generated data would violate them)", t.Name)
			}
			if len(idx.Columns) == 1 && tb.IndexOn == "" {
				tb.IndexOn = idx.Columns[0]
			}
		}

		spec.Tables = append(spec.Tables, tb)
	}
	return spec, ""
}

// TableDefs renders a spec as catalog table definitions in the spec's order —
// each foreign key references an earlier table — ready for CreateTable. It is
// the inverse of what a migrated schema yields, letting a synthetic spec build
// a database without a schema file.
func TableDefs(spec *Catalog) []catalog.TableDef {
	defs := make([]catalog.TableDef, 0, len(spec.Tables))
	for _, t := range spec.Tables {
		def := catalog.TableDef{Name: t.Name, PrimaryKey: []string{"id"}}
		for _, c := range t.Columns {
			def.Columns = append(def.Columns, catalog.ColumnDef{Name: c.Name, Type: c.Type, Nullable: c.Nullable})
		}
		if t.IndexOn != "" {
			def.Indexes = []catalog.IndexDef{{Name: t.Name + "_" + t.IndexOn + "_idx", Columns: []string{t.IndexOn}}}
		}
		if t.FKCol != "" {
			def.ForeignKeys = []catalog.ForeignKeyDef{{
				Name: t.Name + "_fk", Columns: []string{t.FKCol},
				RefTable: t.FKParent, RefColumns: []string{"id"},
			}}
		}
		defs = append(defs, def)
	}
	return defs
}
