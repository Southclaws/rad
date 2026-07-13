// Package migrate computes the schema-change steps that reconcile a live
// catalog with a desired schema (a parsed schema.rad file).
//
// The differ is pure: it takes the current tables and the desired schema and
// returns an ordered list of steps. Applying them (catalog mutations plus
// index backfills) is the frontend's job, keeping this layer free of
// execution concerns.
//
// Renames are recognized only through @renamed_from hints — without a hint,
// a renamed table or column diffs as a delete plus a create, exactly as SQL
// migration tools behave.
//
// Unsupported transformations (column type or nullability changes, foreign
// key changes on existing tables) produce errors instead of destructive
// guesses.
package migrate

import (
	"fmt"

	"github.com/Southclaws/rad/rad/engine/reject"
	"slices"
	"strings"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
)

// Step is one schema-change operation of a migration plan. Steps are ordered so that
// applying them sequentially is always valid: renames first, then new
// tables (dependency-ordered), created columns, index changes, and deletes
// last (table deletes referencing-table-first, mirroring creation order).
type Step interface {
	step()
	String() string
}

type RenameTable struct{ From, To string }
type RenameColumn struct{ Table, From, To string }
type CreateTable struct{ Def catalog.TableDef }
type CreateColumn struct {
	Table string
	Def   catalog.ColumnDef
}
type CreateIndex struct {
	Table string
	Def   catalog.IndexDef
}
type DeleteIndex struct{ Table, Index string }
type DeleteColumn struct{ Table, Column string }
type DeleteTable struct{ Table string }

func (RenameTable) step()  {}
func (RenameColumn) step() {}
func (CreateTable) step()  {}
func (CreateColumn) step()    {}
func (CreateIndex) step()     {}
func (DeleteIndex) step()    {}
func (DeleteColumn) step()   {}
func (DeleteTable) step()    {}

func (s RenameTable) String() string { return fmt.Sprintf("rename table %s -> %s", s.From, s.To) }
func (s RenameColumn) String() string {
	return fmt.Sprintf("rename column %s.%s -> %s", s.Table, s.From, s.To)
}
func (s CreateTable) String() string { return fmt.Sprintf("create table %s", s.Def.Name) }
func (s CreateColumn) String() string   { return fmt.Sprintf("create column %s.%s", s.Table, s.Def.Name) }
func (s CreateIndex) String() string    { return fmt.Sprintf("create index %s on %s", s.Def.Name, s.Table) }
func (s DeleteIndex) String() string   { return fmt.Sprintf("delete index %s on %s", s.Index, s.Table) }
func (s DeleteColumn) String() string  { return fmt.Sprintf("delete column %s.%s", s.Table, s.Column) }
func (s DeleteTable) String() string   { return fmt.Sprintf("delete table %s", s.Table) }

// Diff computes the ordered steps that take current to desired.
func Diff(current []catalog.Table, desired *schema.Schema) ([]Step, error) {
	var renames, adds, indexDeletes, indexCreates, columnDeletes, tableDeletes []Step

	curByName := map[string]catalog.Table{}
	for _, t := range current {
		curByName[t.Name] = t
	}
	desiredNames := map[string]bool{}
	for _, d := range desired.Tables {
		desiredNames[d.Def.Name] = true
	}

	// Match desired tables to current ones, honoring rename hints.
	matched := map[string]schema.Table{} // current name -> desired
	var creates []schema.Table
	for _, d := range desired.Tables {
		switch {
		case hasTable(curByName, d.Def.Name):
			matched[d.Def.Name] = d
		case d.RenamedFrom != "" && hasTable(curByName, d.RenamedFrom) && !desiredNames[d.RenamedFrom]:
			renames = append(renames, RenameTable{From: d.RenamedFrom, To: d.Def.Name})
			matched[d.RenamedFrom] = d
		default:
			creates = append(creates, d)
		}
	}

	// Current tables with no desired counterpart get deleted. Deletes are
	// ordered referencing-table-first: the catalog refuses to delete a table
	// another table still points at through a foreign key, so children must
	// go before their parents.
	deleted := map[string]bool{}
	var deletedTables []catalog.Table
	for _, t := range current {
		if _, ok := matched[t.Name]; !ok {
			deleted[t.Name] = true
			deletedTables = append(deletedTables, t)
		}
	}
	for _, t := range orderDeletes(deletedTables) {
		tableDeletes = append(tableDeletes, DeleteTable{Table: t.Name})
	}

	// No surviving or new table may reference a deleted one.
	for _, d := range desired.Tables {
		for _, fk := range d.Def.ForeignKeys {
			if deleted[fk.RefTable] {
				return nil, reject.Inputf("migrate: table %q references deleted table %q", d.Def.Name, fk.RefTable)
			}
		}
	}

	ordered, err := orderCreates(creates)
	if err != nil {
		return nil, err
	}
	var tableCreates []Step
	for _, d := range ordered {
		tableCreates = append(tableCreates, CreateTable{Def: d.Def})
	}

	// Diff matched tables.
	for curName, d := range matched {
		cur := curByName[curName]
		steps, err := diffTable(cur, d)
		if err != nil {
			return nil, err
		}
		renames = append(renames, steps.renames...)
		adds = append(adds, steps.adds...)
		indexDeletes = append(indexDeletes, steps.indexDeletes...)
		indexCreates = append(indexCreates, steps.indexCreates...)
		columnDeletes = append(columnDeletes, steps.columnDeletes...)
	}

	var out []Step
	out = append(out, renames...)
	out = append(out, tableCreates...)
	out = append(out, adds...)
	out = append(out, indexDeletes...)
	out = append(out, indexCreates...)
	out = append(out, columnDeletes...)
	out = append(out, tableDeletes...)
	return out, nil
}

func hasTable(m map[string]catalog.Table, name string) bool {
	_, ok := m[name]
	return ok
}

type tableSteps struct {
	renames, adds, indexDeletes, indexCreates, columnDeletes []Step
}

func diffTable(cur catalog.Table, d schema.Table) (tableSteps, error) {
	var out tableSteps
	name := d.Def.Name

	// Simulate the current table's column names after renames so the rest
	// of the diff compares like with like.
	curCols := map[string]catalog.Column{}
	for _, c := range cur.Columns {
		curCols[c.Name] = c
	}
	desiredCols := map[string]bool{}
	for _, c := range d.Def.Columns {
		desiredCols[c.Name] = true
	}

	for _, dc := range d.Def.Columns {
		if _, ok := curCols[dc.Name]; ok {
			continue
		}
		if old, hinted := d.ColumnRenames[dc.Name]; hinted {
			if c, ok := curCols[old]; ok && !desiredCols[old] {
				out.renames = append(out.renames, RenameColumn{Table: name, From: old, To: dc.Name})
				delete(curCols, old)
				curCols[dc.Name] = renamedColumn(c, dc.Name)
				continue
			}
		}
		out.adds = append(out.adds, CreateColumn{Table: name, Def: dc})
	}

	// Validate matched columns and collect deletes.
	for _, dc := range d.Def.Columns {
		c, ok := curCols[dc.Name]
		if !ok {
			continue // newly added above
		}
		if c.Type != dc.Type {
			return tableSteps{}, reject.Inputf("migrate: %s.%s: changing type %s -> %s is not supported", name, dc.Name, c.Type, dc.Type)
		}
		if c.Nullable != dc.Nullable {
			return tableSteps{}, reject.Inputf("migrate: %s.%s: changing nullability is not supported", name, dc.Name)
		}
	}
	for colName := range curCols {
		if !desiredCols[colName] {
			out.columnDeletes = append(out.columnDeletes, DeleteColumn{Table: name, Column: colName})
		}
	}

	// Primary keys are immutable.
	renamedPK := make([]string, len(cur.PrimaryKey))
	copy(renamedPK, cur.PrimaryKey)
	applyRenames(renamedPK, out.renames)
	if !slices.Equal(renamedPK, d.Def.PrimaryKey) {
		return tableSteps{}, reject.Inputf("migrate: %s: changing the primary key is not supported", name)
	}

	// Foreign keys are immutable on existing tables.
	if err := compareFKs(cur, d, out.renames); err != nil {
		return tableSteps{}, err
	}

	// Indexes: compared structurally (columns + uniqueness) on post-rename
	// column lists, not by name — renames must not force a delete and
	// backfill of an index whose shape is unchanged. Names are identifiers
	// only.
	sig := func(cols []string, unique bool) string {
		u := ""
		if unique {
			u = "!"
		}
		return strings.Join(cols, ",") + u
	}
	curIdx := map[string]catalog.Index{}
	for _, idx := range cur.Indexes {
		applyRenames(idx.Columns, out.renames)
		curIdx[sig(idx.Columns, idx.Unique)] = idx
	}
	desiredIdx := map[string]catalog.IndexDef{}
	for _, idx := range d.Def.Indexes {
		desiredIdx[sig(idx.Columns, idx.Unique)] = idx
	}
	for key, di := range desiredIdx {
		if _, ok := curIdx[key]; !ok {
			out.indexCreates = append(out.indexCreates, CreateIndex{Table: name, Def: di})
		}
	}
	for key, ci := range curIdx {
		if _, ok := desiredIdx[key]; !ok {
			out.indexDeletes = append(out.indexDeletes, DeleteIndex{Table: name, Index: ci.Name})
		}
	}

	sortSteps(out.renames)
	sortSteps(out.adds)
	sortSteps(out.indexDeletes)
	sortSteps(out.indexCreates)
	sortSteps(out.columnDeletes)
	return out, nil
}

func renamedColumn(c catalog.Column, newName string) catalog.Column {
	c.Name = newName
	return c
}

// applyRenames rewrites names in-place according to RenameColumn steps.
func applyRenames(names []string, renames []Step) {
	for _, s := range renames {
		r, ok := s.(RenameColumn)
		if !ok {
			continue
		}
		for i, n := range names {
			if n == r.From {
				names[i] = r.To
			}
		}
	}
}

// compareFKs verifies the desired foreign keys equal the current ones
// (after column renames). RefTableID must be resolved back to a name by the
// caller-supplied desired defs, so comparison uses column shapes and
// reference columns only — sufficient for the POC's FK-to-primary-key rule.
func compareFKs(cur catalog.Table, d schema.Table, renames []Step) error {
	canon := func(cols, refCols []string) string {
		return strings.Join(cols, ",") + "->" + strings.Join(refCols, ",")
	}
	curSet := map[string]bool{}
	for _, fk := range cur.ForeignKeys {
		cols := slices.Clone(fk.Columns)
		applyRenames(cols, renames)
		curSet[canon(cols, fk.RefColumns)] = true
	}
	desSet := map[string]bool{}
	for _, fk := range d.Def.ForeignKeys {
		desSet[canon(fk.Columns, fk.RefColumns)] = true
	}
	if len(curSet) != len(desSet) {
		return reject.Inputf("migrate: %s: adding or removing foreign keys on an existing table is not supported", d.Def.Name)
	}
	for k := range desSet {
		if !curSet[k] {
			return reject.Inputf("migrate: %s: changing foreign keys on an existing table is not supported", d.Def.Name)
		}
	}
	return nil
}

// orderCreates topologically sorts new tables by their FK dependencies on
// other new tables (references to existing tables and to themselves are
// always satisfiable).
func orderCreates(creates []schema.Table) ([]schema.Table, error) {
	newNames := map[string]bool{}
	for _, d := range creates {
		newNames[d.Def.Name] = true
	}
	// Stable iteration: sort by name first.
	slices.SortFunc(creates, func(a, b schema.Table) int {
		return strings.Compare(a.Def.Name, b.Def.Name)
	})

	var ordered []schema.Table
	done := map[string]bool{}
	for range creates {
		progressed := false
		for _, d := range creates {
			if done[d.Def.Name] {
				continue
			}
			ready := true
			for _, fk := range d.Def.ForeignKeys {
				if fk.RefTable == d.Def.Name {
					continue // self-reference
				}
				if newNames[fk.RefTable] && !done[fk.RefTable] {
					ready = false
					break
				}
			}
			if ready {
				ordered = append(ordered, d)
				done[d.Def.Name] = true
				progressed = true
			}
		}
		if len(ordered) == len(creates) {
			return ordered, nil
		}
		if !progressed {
			break
		}
	}
	if len(ordered) != len(creates) {
		return nil, reject.Inputf("migrate: circular foreign key dependency among new tables")
	}
	return ordered, nil
}

// orderDeletes sorts tables to delete so that every table precedes the tables
// it references — the reverse of creation order. Foreign keys cannot be
// added to existing tables and creation rejects cycles, so only
// self-references can exist among a delete set; they don't constrain order.
// Input arrives name-sorted (ListTables), which keeps the result stable.
func orderDeletes(tables []catalog.Table) []catalog.Table {
	byID := map[string]string{} // table ID -> name, delete set only
	for _, t := range tables {
		byID[t.ID] = t.Name
	}
	// refs[parent] counts deleted tables still referencing parent.
	refs := map[string]int{}
	for _, t := range tables {
		for _, fk := range t.ForeignKeys {
			if fk.RefTableID == t.ID {
				continue // self-reference dies with the table
			}
			if parent, ok := byID[fk.RefTableID]; ok {
				refs[parent]++
			}
		}
	}

	var ordered []catalog.Table
	done := map[string]bool{}
	for len(ordered) < len(tables) {
		progressed := false
		for _, t := range tables {
			if done[t.Name] || refs[t.Name] > 0 {
				continue
			}
			ordered = append(ordered, t)
			done[t.Name] = true
			progressed = true
			for _, fk := range t.ForeignKeys {
				if fk.RefTableID == t.ID {
					continue
				}
				if parent, ok := byID[fk.RefTableID]; ok {
					refs[parent]--
				}
			}
		}
		if !progressed {
			// Unreachable while FK creation rejects cycles; emit the rest
			// in name order rather than dropping steps on the floor.
			for _, t := range tables {
				if !done[t.Name] {
					ordered = append(ordered, t)
				}
			}
			break
		}
	}
	return ordered
}

// sortSteps orders steps deterministically by their string form so plans
// are stable across runs.
func sortSteps(steps []Step) {
	slices.SortFunc(steps, func(a, b Step) int { return strings.Compare(a.String(), b.String()) })
}
