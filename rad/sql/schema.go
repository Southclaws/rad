package sql

import (
	"fmt"
	"strings"

	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// Schema is the catalog snapshot the compiler binds against. It carries only
// what name resolution and type inference need; it is built from the same
// introspection shape a client sees, so the compiler works identically
// in-server and client-side.
type Schema struct {
	tables map[string]*Table
}

type Table struct {
	ID         uint32
	Name       string
	Columns    []*Column
	PrimaryKey []string
	Indexes    []Index

	byName map[string]*Column
}

type Index struct {
	Name    string
	Columns []string
	Unique  bool
}

type Column struct {
	ID       uint32
	Name     string
	Scalar   lirwire.ScalarType
	Format   string
	Nullable bool
	Default  bool
}

func NewSchema(tables []protocol.TableInfo) (*Schema, error) {
	s := &Schema{tables: map[string]*Table{}}
	for _, ti := range tables {
		t := &Table{
			ID:         ti.ID,
			Name:       ti.Name,
			PrimaryKey: ti.PrimaryKey,
			byName:     map[string]*Column{},
		}
		for _, ix := range ti.Indexes {
			t.Indexes = append(t.Indexes, Index{Name: ix.Name, Columns: ix.Columns, Unique: ix.Unique})
		}
		for _, ci := range ti.Columns {
			scalar, err := scalarFromCatalog(ci.Type)
			if err != nil {
				return nil, fmt.Errorf("table %s column %s: %w", ti.Name, ci.Name, err)
			}
			c := &Column{
				ID:       ci.ID,
				Name:     ci.Name,
				Scalar:   scalar,
				Format:   ci.Format,
				Nullable: ci.Nullable,
				Default:  ci.Default != nil,
			}
			t.Columns = append(t.Columns, c)
			t.byName[c.Name] = c
		}
		s.tables[t.Name] = t
	}
	return s, nil
}

func (s *Schema) Table(name string) (*Table, bool) {
	t, ok := s.tables[strings.ToLower(name)]
	return t, ok
}

// findIndex locates an index by name across all tables (Postgres index
// names are schema-global; the engine scopes them per table).
func (s *Schema) findIndex(name string) (*Table, string) {
	for _, t := range s.tables {
		for _, ix := range t.Indexes {
			if ix.Name == name {
				return t, ix.Name
			}
		}
	}
	return nil, ""
}

func (t *Table) Column(name string) (*Column, bool) {
	c, ok := t.byName[name]
	return c, ok
}

func scalarFromCatalog(typ string) (lirwire.ScalarType, error) {
	switch typ {
	case "text", "string":
		return lirwire.ScalarTypeText, nil
	case "int64":
		return lirwire.ScalarTypeInt64, nil
	case "float64":
		return lirwire.ScalarTypeFloat64, nil
	case "bool":
		return lirwire.ScalarTypeBool, nil
	}
	return "", fmt.Errorf("unknown catalog column type %q", typ)
}
