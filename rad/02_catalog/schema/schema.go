// Package schema parses RAD's declarative schema files (schema.rad) into
// catalog definitions.
//
// The format is YAML, structurally validated against an embedded JSON
// Schema (radschema.json) — no bespoke grammar. A schema file looks like:
//
//	tables:
//	  - name: users
//	    columns:
//	      - { name: id,         type: string, pk: true, default: uuid() }
//	      - { name: username,   type: string, unique: true }
//	      - { name: email,      type: string, nullable: true, format: email }
//	      - { name: created_at, type: int64, format: unix_ms, default: now_ms() }
//
//	  - name: team_members
//	    columns:
//	      - { name: team_id, type: string, ref: teams.id }
//	      - { name: user_id, type: string, ref: users.id }
//	      - { name: role,    type: string, default: member }
//	    primary_key: [team_id, user_id]
//	    indexes:
//	      - { columns: [user_id] }
//	      - { columns: [team_id, role], unique: true }
//
// Column attributes: pk (bool; multiple columns form a composite key in
// declaration order — or use table-level primary_key), unique / index
// (single-column shorthands for indexes), ref: table.column (foreign key to
// the target's primary key), format (uninterpreted metadata like email or
// unix_ms), default (a literal of the column's type or the generators
// uuid() / now_ms()), nullable, and renamed_from (a migration hint, also
// available at table level).
//
// Rename hints are not part of the table definition — they exist so the
// migration differ can distinguish a rename from a drop+add.
package schema

import (
	"bytes"
	_ "embed"
	"fmt"

	"github.com/santhosh-tekuri/jsonschema/v6"
	"gopkg.in/yaml.v3"

	catalog "rad/rad/02_catalog"
)

//go:embed radschema.json
var jsonSchemaSrc []byte

// compiled JSON Schema, built once at package init.
var jsonSchema = func() *jsonschema.Schema {
	doc, err := jsonschema.UnmarshalJSON(bytes.NewReader(jsonSchemaSrc))
	if err != nil {
		panic(fmt.Sprintf("schema: embedded radschema.json is invalid JSON: %v", err))
	}
	c := jsonschema.NewCompiler()
	if err := c.AddResource("radschema.json", doc); err != nil {
		panic(err)
	}
	return c.MustCompile("radschema.json")
}()

// Schema is a parsed schema file.
type Schema struct {
	Tables []Table
}

// Table pairs a catalog definition with migration rename hints.
type Table struct {
	Def catalog.TableDef
	// RenamedFrom is the table's previous name, when the schema author
	// renamed it (renamed_from on the table).
	RenamedFrom string
	// ColumnRenames maps new column name -> previous column name.
	ColumnRenames map[string]string
}

// Table returns the parsed table with the given name.
func (s *Schema) Table(name string) (Table, bool) {
	for _, t := range s.Tables {
		if t.Def.Name == name {
			return t, true
		}
	}
	return Table{}, false
}

// File-shape structs for the typed second decode (after JSON Schema
// validation has pinned the structure).
type fileSchema struct {
	Tables []fileTable `yaml:"tables"`
}

type fileTable struct {
	Name        string       `yaml:"name"`
	RenamedFrom string       `yaml:"renamed_from"`
	Columns     []fileColumn `yaml:"columns"`
	PrimaryKey  []string     `yaml:"primary_key"`
	Indexes     []fileIndex  `yaml:"indexes"`
}

type fileColumn struct {
	Name        string `yaml:"name"`
	Type        string `yaml:"type"`
	Nullable    bool   `yaml:"nullable"`
	PK          bool   `yaml:"pk"`
	Unique      bool   `yaml:"unique"`
	Index       bool   `yaml:"index"`
	Ref         string `yaml:"ref"`
	Format      string `yaml:"format"`
	Default     any    `yaml:"default"`
	RenamedFrom string `yaml:"renamed_from"`
}

type fileIndex struct {
	Columns []string `yaml:"columns"`
	Unique  bool     `yaml:"unique"`
}

// Parse parses and validates a schema.rad source. The filename is used in
// error messages only.
func Parse(filename string, src []byte) (*Schema, error) {
	// Structural validation against the JSON Schema first: decode
	// generically, validate, then decode into typed structs.
	var generic any
	if err := yaml.Unmarshal(src, &generic); err != nil {
		return nil, fmt.Errorf("%s: %w", filename, err)
	}
	if err := jsonSchema.Validate(normalizeForJSON(generic)); err != nil {
		return nil, fmt.Errorf("%s: %w", filename, err)
	}

	var file fileSchema
	if err := yaml.Unmarshal(src, &file); err != nil {
		return nil, fmt.Errorf("%s: %w", filename, err)
	}

	s := &Schema{}
	seen := map[string]bool{}
	for _, ft := range file.Tables {
		if seen[ft.Name] {
			return nil, fmt.Errorf("%s: duplicate table %q", filename, ft.Name)
		}
		seen[ft.Name] = true
		tbl, err := buildTable(filename, ft)
		if err != nil {
			return nil, err
		}
		s.Tables = append(s.Tables, tbl)
	}
	return s, nil
}

func buildTable(filename string, ft fileTable) (Table, error) {
	t := Table{
		Def:           catalog.TableDef{Name: ft.Name},
		RenamedFrom:   ft.RenamedFrom,
		ColumnRenames: map[string]string{},
	}
	var pkFromColumns []string

	for _, fc := range ft.Columns {
		col := catalog.ColumnDef{
			Name:     fc.Name,
			Nullable: fc.Nullable,
			Format:   fc.Format,
		}
		switch fc.Type {
		case "string":
			col.Type = catalog.TypeText
		case "int64":
			col.Type = catalog.TypeInt64
		case "float64":
			col.Type = catalog.TypeFloat64
		case "bool":
			col.Type = catalog.TypeBool
		}

		if fc.Default != nil {
			d, err := parseDefault(fc.Default, col.Type)
			if err != nil {
				return Table{}, fmt.Errorf("%s: table %q, column %q: %w", filename, ft.Name, fc.Name, err)
			}
			col.Default = d
		}
		if fc.PK {
			pkFromColumns = append(pkFromColumns, fc.Name)
		}
		if fc.Unique {
			t.Def.Indexes = append(t.Def.Indexes, catalog.IndexDef{
				Name: indexName(ft.Name, []string{fc.Name}, true), Columns: []string{fc.Name}, Unique: true,
			})
		}
		if fc.Index {
			t.Def.Indexes = append(t.Def.Indexes, catalog.IndexDef{
				Name: indexName(ft.Name, []string{fc.Name}, false), Columns: []string{fc.Name},
			})
		}
		if fc.Ref != "" {
			refTable, refCol, _ := cutRef(fc.Ref)
			t.Def.ForeignKeys = append(t.Def.ForeignKeys, catalog.ForeignKeyDef{
				Name:     fmt.Sprintf("%s_%s_fk", ft.Name, fc.Name),
				Columns:  []string{fc.Name},
				RefTable: refTable, RefColumns: []string{refCol},
			})
		}
		if fc.RenamedFrom != "" {
			t.ColumnRenames[fc.Name] = fc.RenamedFrom
		}
		t.Def.Columns = append(t.Def.Columns, col)
	}

	if len(pkFromColumns) > 0 && len(ft.PrimaryKey) > 0 {
		return Table{}, fmt.Errorf("%s: table %q: both column-level pk and table-level primary_key", filename, ft.Name)
	}
	t.Def.PrimaryKey = ft.PrimaryKey
	if len(pkFromColumns) > 0 {
		t.Def.PrimaryKey = pkFromColumns
	}

	for _, fi := range ft.Indexes {
		t.Def.Indexes = append(t.Def.Indexes, catalog.IndexDef{
			Name: indexName(ft.Name, fi.Columns, fi.Unique), Columns: fi.Columns, Unique: fi.Unique,
		})
	}
	return t, nil
}

// parseDefault interprets a YAML default value for a column of type typ.
// Strings may be the generators uuid() / now_ms(); everything else is a
// literal that must match the column type.
func parseDefault(raw any, typ catalog.Type) (*catalog.Default, error) {
	switch v := raw.(type) {
	case string:
		switch v {
		case "uuid()":
			return &catalog.Default{Func: catalog.DefaultUUID}, nil
		case "now_ms()":
			return &catalog.Default{Func: catalog.DefaultNowMS}, nil
		}
		if typ != catalog.TypeText {
			return nil, fmt.Errorf("string default on %s column", typ)
		}
		return &catalog.Default{Text: v}, nil
	case bool:
		if typ != catalog.TypeBool {
			return nil, fmt.Errorf("bool default on %s column", typ)
		}
		return &catalog.Default{Bool: v}, nil
	case int:
		if typ == catalog.TypeFloat64 {
			return &catalog.Default{Float64: float64(v)}, nil
		}
		if typ != catalog.TypeInt64 {
			return nil, fmt.Errorf("integer default on %s column", typ)
		}
		return &catalog.Default{Int64: int64(v)}, nil
	case float64:
		if typ != catalog.TypeFloat64 {
			return nil, fmt.Errorf("float default on %s column", typ)
		}
		return &catalog.Default{Float64: v}, nil
	default:
		return nil, fmt.Errorf("cannot use %T as a default", raw)
	}
}

// indexName derives the deterministic name for an index, e.g.
// tasks_board_id_status_idx or boards_team_id_name_uq.
func indexName(table string, cols []string, unique bool) string {
	suffix := "idx"
	if unique {
		suffix = "uq"
	}
	name := table
	for _, c := range cols {
		name += "_" + c
	}
	return name + "_" + suffix
}

func cutRef(ref string) (table, column string, ok bool) {
	for i := range len(ref) {
		if ref[i] == '.' {
			return ref[:i], ref[i+1:], true
		}
	}
	return "", "", false
}

// normalizeForJSON converts YAML-decoded values into the shapes the JSON
// Schema validator expects (json.Number-free, string-keyed maps — yaml.v3
// already produces map[string]interface{} for string keys, but nested
// non-string keys or ints need normalizing).
func normalizeForJSON(v any) any {
	switch x := v.(type) {
	case map[string]any:
		out := make(map[string]any, len(x))
		for k, val := range x {
			out[k] = normalizeForJSON(val)
		}
		return out
	case map[any]any:
		out := make(map[string]any, len(x))
		for k, val := range x {
			out[fmt.Sprint(k)] = normalizeForJSON(val)
		}
		return out
	case []any:
		out := make([]any, len(x))
		for i, val := range x {
			out[i] = normalizeForJSON(val)
		}
		return out
	case int:
		return float64(x)
	case int64:
		return float64(x)
	default:
		return v
	}
}
