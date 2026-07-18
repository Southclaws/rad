// Package migrate computes the schema-change steps that reconcile a live
// catalog with a desired schema (a parsed rad.schema.yaml file).
//
// The differ is pure: it takes the current tables and the desired schema and
// returns an ordered list of steps. Applying them (catalog mutations plus
// index backfills) is the frontend's job, keeping this layer free of
// execution concerns.
//
// Tables and columns are matched by their stable schema IDs. A name change on
// a matched identity is a rename, even when other properties change in the
// same migration.
//
// Unsupported transformations (column type or nullability changes, foreign
// key changes on existing tables) produce errors instead of destructive
// guesses.
package migrate

import (
	"fmt"
	"slices"
	"strings"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/naming"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// Step is one schema-change operation of a migration plan. Steps are ordered so that
// applying them sequentially is always valid: renames first, then new
// tables (dependency-ordered), created columns, index changes, and deletes
// last (table deletes referencing-table-first, mirroring creation order).
type Step interface {
	step()
	String() string
}

type (
	RenameTable  struct{ From, To string }
	RenameColumn struct{ Table, From, To string }
	CreateTable  struct{ Def model.TableDef }
	CreateColumn struct {
		Table string
		Def   model.ColumnDef
	}
)

type CreateIndex struct {
	Table string
	Def   model.IndexDef
}
type (
	DeleteIndex  struct{ Table, Index string }
	DeleteColumn struct{ Table, Column string }
	DeleteTable  struct{ Table string }
)

func (RenameTable) step()  {}
func (RenameColumn) step() {}
func (CreateTable) step()  {}
func (CreateColumn) step() {}
func (CreateIndex) step()  {}
func (DeleteIndex) step()  {}
func (DeleteColumn) step() {}
func (DeleteTable) step()  {}

func (s RenameTable) String() string { return fmt.Sprintf("rename table %s -> %s", s.From, s.To) }
func (s RenameColumn) String() string {
	return fmt.Sprintf("rename column %s.%s -> %s", s.Table, s.From, s.To)
}
func (s CreateTable) String() string  { return fmt.Sprintf("create table %s", s.Def.Name) }
func (s CreateColumn) String() string { return fmt.Sprintf("create column %s.%s", s.Table, s.Def.Name) }
func (s CreateIndex) String() string {
	return fmt.Sprintf("create index %s on %s", s.Def.Name, s.Table)
}
func (s DeleteIndex) String() string  { return fmt.Sprintf("delete index %s on %s", s.Index, s.Table) }
func (s DeleteColumn) String() string { return fmt.Sprintf("delete column %s.%s", s.Table, s.Column) }
func (s DeleteTable) String() string  { return fmt.Sprintf("delete table %s", s.Table) }

// Diff computes the ordered steps that take current to desired.
func Diff(current []model.Table, desired *schema.Schema) ([]Step, error) {
	var renames, adds, indexDeletes, indexCreates, columnDeletes, tableDeletes []Step

	curByID := map[model.SchemaID]model.Table{}
	curByName := map[string]model.Table{}
	curByPhysicalID := map[string]model.Table{}
	for _, t := range current {
		if t.SchemaID == 0 || t.SchemaID > model.MaxSchemaID {
			return nil, reject.Fail(reject.ReasonCatalogDrift,
				"migrate: physical table %q has invalid schema ID %d", t.Name, t.SchemaID)
		}
		if previous, exists := curByID[t.SchemaID]; exists {
			return nil, reject.Fail(reject.ReasonCatalogDrift,
				"migrate: physical tables %q and %q share schema ID %d", previous.Name, t.Name, t.SchemaID)
		}
		curByID[t.SchemaID] = t
		curByName[t.Name] = t
		curByPhysicalID[t.ID] = t
	}
	desiredIDs := map[model.SchemaID]string{}
	desiredNames := map[string]model.SchemaID{}
	for _, d := range desired.Tables {
		if d.Def.ID == 0 || d.Def.ID > model.MaxSchemaID {
			return nil, reject.Inputf("migrate: table %q has invalid schema ID %d", d.Def.Name, d.Def.ID)
		}
		if previous, exists := desiredIDs[d.Def.ID]; exists {
			return nil, reject.Inputf("migrate: tables %q and %q share schema ID %d", previous, d.Def.Name, d.Def.ID)
		}
		if previous, exists := desiredNames[d.Def.Name]; exists {
			return nil, reject.Inputf("migrate: tables with schema IDs %d and %d share name %q", previous, d.Def.ID, d.Def.Name)
		}
		desiredIDs[d.Def.ID] = d.Def.Name
		desiredNames[d.Def.Name] = d.Def.ID
	}

	// Match desired tables to current ones by logical identity.
	matched := map[model.SchemaID]schema.Table{}
	var creates []schema.Table
	for _, d := range desired.Tables {
		cur, exists := curByID[d.Def.ID]
		if !exists {
			if occupied, collision := curByName[d.Def.Name]; collision {
				return nil, reject.Inputf(
					"migrate: table %q changes schema ID %d -> %d; remove it in one migration before creating the replacement",
					d.Def.Name, occupied.SchemaID, d.Def.ID)
			}
			creates = append(creates, d)
			continue
		}
		matched[d.Def.ID] = d
		if cur.Name == d.Def.Name {
			continue
		}
		if occupied, collision := curByName[d.Def.Name]; collision && occupied.SchemaID != cur.SchemaID {
			return nil, reject.Inputf(
				"migrate: cannot rename table %q to %q because that name belongs to schema ID %d",
				cur.Name, d.Def.Name, occupied.SchemaID)
		}
		renames = append(renames, RenameTable{From: cur.Name, To: d.Def.Name})
	}

	// Current tables with no desired counterpart get deleted. Deletes are
	// ordered referencing-table-first: the catalog refuses to delete a table
	// another table still points at through a foreign key, so children must
	// go before their parents.
	deleted := map[string]bool{}
	var deletedTables []model.Table
	for _, t := range current {
		if _, ok := matched[t.SchemaID]; !ok {
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
	for _, d := range desired.Tables {
		if _, exists := matched[d.Def.ID]; !exists {
			continue
		}
		cur := curByID[d.Def.ID]
		steps, err := diffTable(cur, d, curByPhysicalID, desiredNames)
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

type tableSteps struct {
	renames, adds, indexDeletes, indexCreates, columnDeletes []Step
}

func diffTable(
	cur model.Table,
	d schema.Table,
	currentByPhysicalID map[string]model.Table,
	desiredByName map[string]model.SchemaID,
) (tableSteps, error) {
	var out tableSteps
	name := d.Def.Name

	curColsByID := map[model.SchemaID]model.Column{}
	curColsByName := map[string]model.Column{}
	for _, c := range cur.Columns {
		if c.SchemaID == 0 || c.SchemaID > model.MaxSchemaID {
			return tableSteps{}, reject.Fail(reject.ReasonCatalogDrift,
				"migrate: physical column %q.%q has invalid schema ID %d", cur.Name, c.Name, c.SchemaID)
		}
		if previous, exists := curColsByID[c.SchemaID]; exists {
			return tableSteps{}, reject.Fail(reject.ReasonCatalogDrift,
				"migrate: physical columns %q.%q and %q.%q share schema ID %d",
				cur.Name, previous.Name, cur.Name, c.Name, c.SchemaID)
		}
		curColsByID[c.SchemaID] = c
		curColsByName[c.Name] = c
	}
	desiredIDs := map[model.SchemaID]string{}
	desiredNames := map[string]model.SchemaID{}
	for _, c := range d.Def.Columns {
		if c.ID == 0 || c.ID > model.MaxSchemaID {
			return tableSteps{}, reject.Inputf("migrate: column %q.%q has invalid schema ID %d", name, c.Name, c.ID)
		}
		if previous, exists := desiredIDs[c.ID]; exists {
			return tableSteps{}, reject.Inputf(
				"migrate: columns %q.%q and %q.%q share schema ID %d", name, previous, name, c.Name, c.ID)
		}
		if previous, exists := desiredNames[c.Name]; exists {
			return tableSteps{}, reject.Inputf(
				"migrate: columns with schema IDs %d and %d on table %q share name %q",
				previous, c.ID, name, c.Name)
		}
		desiredIDs[c.ID] = c.Name
		desiredNames[c.Name] = c.ID
	}

	for _, dc := range d.Def.Columns {
		current, exists := curColsByID[dc.ID]
		if !exists {
			if occupied, collision := curColsByName[dc.Name]; collision {
				return tableSteps{}, reject.Inputf(
					"migrate: column %q.%q changes schema ID %d -> %d; remove it in one migration before creating the replacement",
					name, dc.Name, occupied.SchemaID, dc.ID)
			}
			out.adds = append(out.adds, CreateColumn{Table: name, Def: dc})
			continue
		}
		if current.Name == dc.Name {
			continue
		}
		if occupied, collision := curColsByName[dc.Name]; collision && occupied.SchemaID != current.SchemaID {
			return tableSteps{}, reject.Inputf(
				"migrate: cannot rename column %q.%q to %q because that name belongs to schema ID %d",
				name, current.Name, dc.Name, occupied.SchemaID)
		}
		out.renames = append(out.renames, RenameColumn{Table: name, From: current.Name, To: dc.Name})
	}

	// Validate matched columns and collect deletes.
	for _, dc := range d.Def.Columns {
		c, ok := curColsByID[dc.ID]
		if !ok {
			continue // newly added above
		}
		if c.Type != dc.Type {
			return tableSteps{}, reject.Inputf("migrate: %s.%s: changing type %s -> %s is not supported", name, dc.Name, c.Type, dc.Type)
		}
		if c.Nullable != dc.Nullable {
			return tableSteps{}, reject.Inputf("migrate: %s.%s: changing nullability is not supported", name, dc.Name)
		}
		if c.Format != dc.Format {
			return tableSteps{}, reject.Inputf("migrate: %s.%s: changing format is not supported", name, dc.Name)
		}
		if (c.Default == nil) != (dc.Default == nil) ||
			c.Default != nil && *c.Default != *dc.Default {
			return tableSteps{}, reject.Inputf("migrate: %s.%s: changing the default is not supported", name, dc.Name)
		}
	}
	currentOrder := make([]model.SchemaID, 0, len(cur.Columns))
	desiredOrder := make([]model.SchemaID, 0, len(d.Def.Columns))
	for _, column := range cur.Columns {
		if _, survives := desiredIDs[column.SchemaID]; survives {
			currentOrder = append(currentOrder, column.SchemaID)
		}
	}
	for _, column := range d.Def.Columns {
		if _, exists := curColsByID[column.ID]; exists {
			desiredOrder = append(desiredOrder, column.ID)
		}
	}
	if !slices.Equal(currentOrder, desiredOrder) {
		return tableSteps{}, reject.Inputf("migrate: %s: changing column order is not supported", name)
	}
	for schemaID, column := range curColsByID {
		if _, exists := desiredIDs[schemaID]; !exists {
			out.columnDeletes = append(out.columnDeletes, DeleteColumn{Table: name, Column: column.Name})
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
	if err := compareFKs(cur, d, out.renames, currentByPhysicalID, desiredByName); err != nil {
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
	curIdx := map[string]model.Index{}
	for _, idx := range cur.Indexes {
		generated := idx.Name == naming.Index(cur.Name, idx.Columns, idx.Unique)
		applyRenames(idx.Columns, out.renames)
		if generated {
			idx.Name = naming.Index(name, idx.Columns, idx.Unique)
		}
		curIdx[sig(idx.Columns, idx.Unique)] = idx
	}
	desiredIdx := map[string]model.IndexDef{}
	for _, idx := range d.Def.Indexes {
		desiredIdx[sig(idx.Columns, idx.Unique)] = idx
	}
	for key, di := range desiredIdx {
		if current, ok := curIdx[key]; ok && current.Name != di.Name {
			return tableSteps{}, reject.Inputf(
				"migrate: %s: renaming index %q to %q is not supported", name, current.Name, di.Name)
		} else if !ok {
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

// compareFKs verifies that desired foreign keys retain the same local column
// shape and referenced table identity. Reference column names are omitted
// because foreign keys can only target the full primary key; retaining the
// target table ID therefore also retains the referenced key identity while
// allowing that key's columns to be renamed.
func compareFKs(
	cur model.Table,
	d schema.Table,
	renames []Step,
	currentByPhysicalID map[string]model.Table,
	desiredByName map[string]model.SchemaID,
) error {
	canon := func(name string, cols []string, refTable model.SchemaID) string {
		return fmt.Sprintf("%s:%s->%d", name, strings.Join(cols, ","), refTable)
	}
	curSet := map[string]bool{}
	for _, fk := range cur.ForeignKeys {
		refTable, exists := currentByPhysicalID[fk.RefTableID]
		if !exists {
			return reject.Fail(reject.ReasonCatalogDrift,
				"migrate: foreign key %q on table %q references missing physical table ID %q",
				fk.Name, cur.Name, fk.RefTableID)
		}
		cols := slices.Clone(fk.Columns)
		name := fk.Name
		generated := len(cols) == 1 && name == naming.ForeignKey(cur.Name, cols[0])
		applyRenames(cols, renames)
		if generated {
			name = naming.ForeignKey(d.Def.Name, cols[0])
		}
		curSet[canon(name, cols, refTable.SchemaID)] = true
	}
	desSet := map[string]bool{}
	for _, fk := range d.Def.ForeignKeys {
		refTable, exists := desiredByName[fk.RefTable]
		if !exists {
			return reject.Inputf("migrate: foreign key %q on table %q references unknown table %q",
				fk.Name, d.Def.Name, fk.RefTable)
		}
		desSet[canon(fk.Name, fk.Columns, refTable)] = true
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
func orderDeletes(tables []model.Table) []model.Table {
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

	var ordered []model.Table
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
