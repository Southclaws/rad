// Package codegen emits a typed Go client from a schema.rad file — models,
// CRUD, query builders, relationship includes, and transactions. The
// generated package speaks lir/frontend internally; applications built on
// it never see the IR, keys, or SQL of any kind.
package codegen

import (
	"fmt"
	"strings"

	catalog "rad/rad/02_catalog"
	"rad/rad/02_catalog/schema"
)

// genModel is the code-generation view of a schema: resolved names,
// relations in both directions, and Go type mappings.
type genModel struct {
	Pkg    string
	Tables []*genTable
}

type genTable struct {
	SQLName string // table name in the schema
	Model   string // Go struct name, e.g. TeamMember
	Cols    []genCol
	PK      []genCol
	Forward []genRel // FKs on this table -> parent objects
	Reverse []genRel // FKs elsewhere referencing this table -> child arrays
	Uniques [][]genCol
}

type genCol struct {
	SQLName  string
	Field    string // Go field name
	Type     catalog.Type
	GoType   string // base Go type (string, int64, float64, bool)
	Nullable bool
	HasDef   bool
	IsPK     bool
}

// genRel is one direction of a foreign key as seen from a table.
type genRel struct {
	Field  string // Go field name on the model (Assignee, Comments, ...)
	As     string // include name on the wire (snake of Field)
	FKName string // schema FK name (tasks_assignee_id_fk)
	// Target is the model on the other side: the parent for forward rels,
	// the child for reverse rels.
	Target *genTable
	// Cols are the FK columns on the referencing table (forward only).
	Cols []genCol
}

// build computes the generation model from a parsed schema.
func build(pkg string, sch *schema.Schema) (*genModel, error) {
	m := &genModel{Pkg: pkg}
	byName := map[string]*genTable{}

	for _, st := range sch.Tables {
		t := &genTable{SQLName: st.Def.Name, Model: modelName(st.Def.Name)}
		pk := map[string]bool{}
		for _, c := range st.Def.PrimaryKey {
			pk[c] = true
		}
		for _, cd := range st.Def.Columns {
			t.Cols = append(t.Cols, genCol{
				SQLName:  cd.Name,
				Field:    goName(cd.Name),
				Type:     cd.Type,
				GoType:   goType(cd.Type),
				Nullable: cd.Nullable,
				HasDef:   cd.Default != nil,
				IsPK:     pk[cd.Name],
			})
		}
		for _, pkCol := range st.Def.PrimaryKey {
			for _, c := range t.Cols {
				if c.SQLName == pkCol {
					t.PK = append(t.PK, c)
				}
			}
		}
		for _, idx := range st.Def.Indexes {
			if !idx.Unique {
				continue
			}
			var cols []genCol
			for _, name := range idx.Columns {
				for _, c := range t.Cols {
					if c.SQLName == name {
						cols = append(cols, c)
					}
				}
			}
			t.Uniques = append(t.Uniques, cols)
		}
		m.Tables = append(m.Tables, t)
		byName[t.SQLName] = t
	}

	// Relations, both directions. fkCount tracks how many FKs each child
	// table has toward each parent, to disambiguate reverse names.
	type fkRef struct {
		child, parent *genTable
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
			fkCount[[2]string{child.SQLName, parent.SQLName}]++
		}
	}
	for _, r := range refs {
		forwardField := forwardName(r.fk.Columns[0])
		var cols []genCol
		for _, name := range r.fk.Columns {
			for _, c := range r.child.Cols {
				if c.SQLName == name {
					cols = append(cols, c)
				}
			}
		}
		r.child.Forward = append(r.child.Forward, genRel{
			Field:  forwardField,
			As:     snake(forwardField),
			FKName: r.fk.Name,
			Target: r.parent,
			Cols:   cols,
		})

		reverseField := goName(r.child.SQLName)
		if fkCount[[2]string{r.child.SQLName, r.parent.SQLName}] > 1 {
			reverseField += "By" + forwardField
		}
		r.parent.Reverse = append(r.parent.Reverse, genRel{
			Field:  reverseField,
			As:     snake(reverseField),
			FKName: r.fk.Name,
			Target: r.child,
			Cols:   cols,
		})
	}
	return m, nil
}

func goType(t catalog.Type) string {
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

// lirCtor returns the lir constructor for a base Go value of type t.
func lirCtor(t catalog.Type) string {
	switch t {
	case catalog.TypeText:
		return "lir.Text"
	case catalog.TypeInt64:
		return "lir.Int64"
	case catalog.TypeFloat64:
		return "lir.Float64"
	case catalog.TypeBool:
		return "lir.Bool"
	}
	return "lir.Text"
}

// lirField returns the lir.Value payload accessor for type t.
func lirField(t catalog.Type) string {
	switch t {
	case catalog.TypeText:
		return "Text"
	case catalog.TypeInt64:
		return "Int64"
	case catalog.TypeFloat64:
		return "Float64"
	case catalog.TypeBool:
		return "Bool"
	}
	return "Text"
}

// goName converts snake_case to exported CamelCase, uppercasing the id
// initialism (board_id -> BoardID).
func goName(s string) string {
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

// forwardName derives a relation field from an FK column: assignee_id ->
// Assignee. Columns without the _id suffix get a Ref suffix to avoid
// clashing with the column field itself.
func forwardName(col string) string {
	if name, ok := strings.CutSuffix(col, "_id"); ok && name != "" {
		return goName(name)
	}
	return goName(col) + "Ref"
}

// modelName singularizes a table name and converts it: team_members ->
// TeamMember. Naive plural handling, fine for the POC.
func modelName(table string) string {
	parts := strings.Split(table, "_")
	last := parts[len(parts)-1]
	if strings.HasSuffix(last, "s") && !strings.HasSuffix(last, "ss") && len(last) > 1 {
		parts[len(parts)-1] = strings.TrimSuffix(last, "s")
	}
	return goName(strings.Join(parts, "_"))
}

// snake converts CamelCase (with ID initialism) back to snake_case for
// include names: TasksByAssignee -> tasks_by_assignee.
func snake(s string) string {
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
