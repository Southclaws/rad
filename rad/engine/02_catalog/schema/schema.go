// Package schema parses Rad's declarative schema files (rad.schema.yaml) into
// catalog definitions.
//
// The format is YAML, structurally validated against an embedded JSON
// Schema (radschema.json) — no bespoke grammar. A schema file looks like:
//
//	tables:
//	  - id: 1
//	    name: users
//	    columns:
//	      - { id: 1, name: id,         type: string, pk: true, default: uuid() }
//	      - { id: 2, name: username,   type: string, unique: true }
//	      - { id: 3, name: email,      type: string, nullable: true, format: email }
//	      - { id: 4, name: created_at, type: int64, format: unix_ms, default: now_ms() }
//
//	  - id: 2
//	    name: team_members
//	    columns:
//	      - { id: 1, name: team_id, type: string, ref: teams.id }
//	      - { id: 2, name: user_id, type: string, ref: users.id }
//	      - { id: 3, name: role,    type: string, default: member }
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
// uuid() / now_ms()), and nullable. Table IDs are unique across the schema;
// column IDs are unique within their table. IDs are stable logical identity:
// renaming changes a name while retaining its ID.
package schema

import (
	_ "embed"
	"encoding/json"
	"fmt"

	"github.com/Southclaws/rad/rad/engine/reject"

	yaml "github.com/goccy/go-yaml"
	"github.com/google/jsonschema-go/jsonschema"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
)

//go:embed radschema.json
var jsonSchemaSrc []byte

// compiled JSON Schema, built once at package init.
var jsonSchema = func() *jsonschema.Resolved {
	var s jsonschema.Schema
	if err := json.Unmarshal(jsonSchemaSrc, &s); err != nil {
		panic(fmt.Sprintf("schema: embedded radschema.json is invalid JSON: %v", err))
	}
	resolved, err := s.Resolve(nil)
	if err != nil {
		panic(fmt.Sprintf("schema: embedded radschema.json failed to resolve: %v", err))
	}
	return resolved
}()

// Schema is a parsed schema file.
type Schema struct {
	Tables []Table
}

// Table contains one catalog definition from the schema source.
type Table struct {
	Def catalog.TableDef
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

// Canonical returns the canonical catalog schema represented by this source.
func (s *Schema) Canonical() catalog.Schema {
	defs := make([]catalog.TableDef, len(s.Tables))
	for i, table := range s.Tables {
		defs[i] = table.Def
	}
	return catalog.SchemaFromDefinitions(defs)
}

// File-shape structs for the typed second decode (after JSON Schema
// validation has pinned the structure).
type fileSchema struct {
	Tables []fileTable `yaml:"tables"`
}

type fileTable struct {
	ID         catalog.SchemaID `yaml:"id"`
	Name       string           `yaml:"name"`
	Columns    []fileColumn     `yaml:"columns"`
	PrimaryKey []string         `yaml:"primary_key"`
	Indexes    []fileIndex      `yaml:"indexes"`
}

type fileColumn struct {
	ID       catalog.SchemaID `yaml:"id"`
	Name     string           `yaml:"name"`
	Type     string           `yaml:"type"`
	Nullable bool             `yaml:"nullable"`
	PK       bool             `yaml:"pk"`
	Unique   bool             `yaml:"unique"`
	Index    bool             `yaml:"index"`
	Ref      string           `yaml:"ref"`
	Format   string           `yaml:"format"`
	Default  any              `yaml:"default"`
}

type fileIndex struct {
	Columns []string `yaml:"columns"`
	Unique  bool     `yaml:"unique"`
}

// Parse parses and validates a rad.schema.yaml source. The filename is used in
// error messages only. Every parse error is the schema author's to fix, so
// the whole surface is marked caller-fault for transport classification.
func Parse(filename string, src []byte) (*Schema, error) {
	s, err := parse(filename, src)
	if err != nil {
		return nil, reject.Input(err)
	}
	return s, nil
}

func parse(filename string, src []byte) (*Schema, error) {
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
	seenNames := map[string]bool{}
	seenIDs := map[catalog.SchemaID]string{}
	for _, ft := range file.Tables {
		if seenNames[ft.Name] {
			return nil, fmt.Errorf("%s: duplicate table %q", filename, ft.Name)
		}
		if previous, exists := seenIDs[ft.ID]; exists {
			return nil, fmt.Errorf("%s: tables %q and %q share ID %d", filename, previous, ft.Name, ft.ID)
		}
		seenNames[ft.Name] = true
		seenIDs[ft.ID] = ft.Name
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
		Def: catalog.TableDef{ID: ft.ID, Name: ft.Name},
	}
	var pkFromColumns []string
	seenColumnIDs := map[catalog.SchemaID]string{}

	for _, fc := range ft.Columns {
		if previous, exists := seenColumnIDs[fc.ID]; exists {
			return Table{}, fmt.Errorf("%s: columns %q.%q and %q.%q share ID %d",
				filename, ft.Name, previous, ft.Name, fc.Name, fc.ID)
		}
		seenColumnIDs[fc.ID] = fc.Name
		col := catalog.ColumnDef{
			ID:       fc.ID,
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
	// Integers first: the YAML decoder yields them as uint64/int64, so match on
	// value rather than a single Go type.
	if n, ok := asInt64(raw); ok {
		if typ == catalog.TypeFloat64 {
			return &catalog.Default{Float64: float64(n)}, nil
		}
		if typ != catalog.TypeInt64 {
			return nil, fmt.Errorf("integer default on %s column", typ)
		}
		return &catalog.Default{Int64: n}, nil
	}
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
// Schema validator expects: string-keyed maps and float64 numbers. goccy
// produces map[string]interface{} for string keys, but decodes integers as
// uint64/int64, which the validator does not accept, so numbers are widened
// to float64 here.
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
	case uint64:
		return float64(x)
	case uint:
		return float64(x)
	default:
		return v
	}
}

// asInt64 reports whether raw is an integer value and returns it. The YAML
// decoder yields integers as uint64 (non-negative) or int64, so this accepts
// the signed and unsigned integer kinds rather than a single type.
func asInt64(raw any) (int64, bool) {
	switch n := raw.(type) {
	case int:
		return int64(n), true
	case int64:
		return n, true
	case uint:
		if uint64(n)>>63 != 0 {
			return 0, false
		}
		return int64(n), true
	case uint64:
		if n>>63 != 0 {
			return 0, false
		}
		return int64(n), true
	default:
		return 0, false
	}
}
