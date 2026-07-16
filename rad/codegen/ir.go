// Package codegen turns a parsed schema into a language-agnostic Model and
// hands it to a registered Generator (Go, TypeScript, …) which emits a typed
// client. The generated clients speak the wire protocol internally;
// applications built on them never see the IR, keys, or SQL of any kind.
//
// The pipeline is schema -> Model -> Generator -> files, mirroring the Schemancer
// tool that generates rad's wire types. The Model is the stable contract
// between rad and any generator — built in and in-process today, or an
// external `rad-gen-*` executable satisfying the same JSON contract.
package codegen

import (
	"fmt"
	"strings"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
)

// Model is the code-generation view of a schema: resolved names, relations in
// both directions, and scalar type mappings. It is the input every Generator
// consumes.
type Model struct {
	Pkg    string
	Tables []*Table
}

type Table struct {
	Name    string // table name in the schema
	Model   string // model type name, e.g. TeamMember
	Cols    []Col
	PK      []Col
	Forward []Rel // FKs on this table -> parent objects
	Reverse []Rel // FKs elsewhere referencing this table -> child arrays
	Uniques [][]Col
}

type Col struct {
	Name     string
	Field    string // exported field name (Go)
	Type     catalog.Type
	GoType   string // base Go type (string, int64, float64, bool)
	Nullable bool
	HasDef   bool
	IsPK     bool
}

// Rel is one direction of a foreign key as seen from a table.
type Rel struct {
	Field  string // field name on the model (Assignee, Comments, ...)
	As     string // include name on the wire (snake of Field)
	FKName string // schema FK name (tasks_assignee_id_fk)
	// Target is the model on the other side: the parent for forward rels,
	// the child for reverse rels.
	Target *Table
	// Cols are the FK columns on the referencing table (forward only).
	Cols []Col
	// Pairs are the correlation equations for the relation's sub-query:
	// [column on the scanned target, column on the enclosing scope].
	Pairs [][2]string
}

// Build computes the generation model from a parsed schema.
func Build(pkg string, sch *schema.Schema) (*Model, error) {
	m := &Model{Pkg: pkg}
	byName := map[string]*Table{}

	for _, st := range sch.Tables {
		t := &Table{Name: st.Def.Name, Model: modelName(st.Def.Name)}
		pk := map[string]bool{}
		for _, c := range st.Def.PrimaryKey {
			pk[c] = true
		}
		for _, cd := range st.Def.Columns {
			t.Cols = append(t.Cols, Col{
				Name:     cd.Name,
				Field:    GoName(cd.Name),
				Type:     cd.Type,
				GoType:   GoType(cd.Type),
				Nullable: cd.Nullable,
				HasDef:   cd.Default != nil,
				IsPK:     pk[cd.Name],
			})
		}
		for _, pkCol := range st.Def.PrimaryKey {
			for _, c := range t.Cols {
				if c.Name == pkCol {
					t.PK = append(t.PK, c)
				}
			}
		}
		for _, idx := range st.Def.Indexes {
			if !idx.Unique {
				continue
			}
			var cols []Col
			for _, name := range idx.Columns {
				for _, c := range t.Cols {
					if c.Name == name {
						cols = append(cols, c)
					}
				}
			}
			t.Uniques = append(t.Uniques, cols)
		}
		m.Tables = append(m.Tables, t)
		byName[t.Name] = t
	}

	// Relations, both directions. fkCount tracks how many FKs each child
	// table has toward each parent, to disambiguate reverse names.
	type fkRef struct {
		child, parent *Table
		fk            catalog.ForeignKeyDef
	}
	var refs []fkRef
	fkCount := map[[2]string]int{}
	for _, st := range sch.Tables {
		child := byName[st.Def.Name]
		for _, fk := range st.Def.ForeignKeys {
			parent, ok := byName[fk.RefTable]
			if !ok {
				return nil, fmt.Errorf("codegen: table %q references unknown table %q", st.Def.Name, fk.RefTable)
			}
			refs = append(refs, fkRef{child: child, parent: parent, fk: fk})
			fkCount[[2]string{child.Name, parent.Name}]++
		}
	}
	for _, r := range refs {
		forwardField := forwardName(r.fk.Columns[0])
		var cols []Col
		for _, name := range r.fk.Columns {
			for _, c := range r.child.Cols {
				if c.Name == name {
					cols = append(cols, c)
				}
			}
		}
		forwardPairs := make([][2]string, len(r.fk.Columns))
		reversePairs := make([][2]string, len(r.fk.Columns))
		for i := range r.fk.Columns {
			forwardPairs[i] = [2]string{r.fk.RefColumns[i], r.fk.Columns[i]}
			reversePairs[i] = [2]string{r.fk.Columns[i], r.fk.RefColumns[i]}
		}
		r.child.Forward = append(r.child.Forward, Rel{
			Field:  forwardField,
			As:     Snake(forwardField),
			FKName: r.fk.Name,
			Target: r.parent,
			Cols:   cols,
			Pairs:  forwardPairs,
		})

		reverseField := GoName(r.child.Name)
		if fkCount[[2]string{r.child.Name, r.parent.Name}] > 1 {
			reverseField += "By" + forwardField
		}
		r.parent.Reverse = append(r.parent.Reverse, Rel{
			Field:  reverseField,
			As:     Snake(reverseField),
			FKName: r.fk.Name,
			Target: r.child,
			Cols:   cols,
			Pairs:  reversePairs,
		})
	}
	return m, nil
}

// GoType maps a catalog scalar type to its base Go type.
func GoType(t catalog.Type) string {
	switch t {
	case catalog.TypeText:
		return "string"
	case catalog.TypeInt64:
		return "int64"
	case catalog.TypeFloat64:
		return "float64"
	case catalog.TypeBool:
		return "bool"
	}
	return "any"
}

// GoName converts snake_case to exported CamelCase, uppercasing the id
// initialism (board_id -> BoardID).
func GoName(s string) string {
	parts := strings.Split(s, "_")
	var b strings.Builder
	for _, p := range parts {
		if p == "" {
			continue
		}
		if p == "id" {
			b.WriteString("ID")
			continue
		}
		b.WriteString(strings.ToUpper(p[:1]))
		b.WriteString(p[1:])
	}
	return b.String()
}

// UqCols renders a unique index's columns as "a, b" for doc comments.
func UqCols(uq []Col) string {
	out := ""
	for i, c := range uq {
		if i > 0 {
			out += ", "
		}
		out += c.Name
	}
	return out
}

// forwardName derives a relation field from an FK column: assignee_id ->
// Assignee. Columns without the _id suffix get a Ref suffix to avoid
// clashing with the column field itself.
func forwardName(col string) string {
	if name, ok := strings.CutSuffix(col, "_id"); ok && name != "" {
		return GoName(name)
	}
	return GoName(col) + "Ref"
}

// modelName singularizes a table name and converts it: team_members ->
// TeamMember. Naive plural handling, fine for the POC.
func modelName(table string) string {
	parts := strings.Split(table, "_")
	last := parts[len(parts)-1]
	if strings.HasSuffix(last, "s") && !strings.HasSuffix(last, "ss") && len(last) > 1 {
		parts[len(parts)-1] = strings.TrimSuffix(last, "s")
	}
	return GoName(strings.Join(parts, "_"))
}

// Snake converts CamelCase (with ID initialism) back to snake_case for include
// names: TasksByAssignee -> tasks_by_assignee.
func Snake(s string) string {
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		c := s[i]
		if c >= 'A' && c <= 'Z' {
			if i > 0 {
				prevLower := s[i-1] >= 'a' && s[i-1] <= 'z'
				nextLower := i+1 < len(s) && s[i+1] >= 'a' && s[i+1] <= 'z'
				if prevLower || nextLower {
					b.WriteByte('_')
				}
			}
			b.WriteByte(c - 'A' + 'a')
			continue
		}
		b.WriteByte(c)
	}
	return b.String()
}
