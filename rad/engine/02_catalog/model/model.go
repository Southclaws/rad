// Package model defines the catalog's storage-independent domain values.
package model

import (
	"fmt"
	"time"
)

type SchemaID uint32

const MaxSchemaID SchemaID = 1<<31 - 1

type Type string

const (
	TypeText    Type = "text"
	TypeInt64   Type = "int64"
	TypeFloat64 Type = "float64"
	TypeBool    Type = "bool"
)

type Default struct {
	Func    DefaultFunc `json:"func,omitempty"`
	Text    string      `json:"text,omitempty"`
	Int64   int64       `json:"int64,omitempty"`
	Float64 float64     `json:"float64,omitempty"`
	Bool    bool        `json:"bool,omitempty"`
}

type DefaultFunc string

const (
	DefaultUUID  DefaultFunc = "uuid"
	DefaultNowMS DefaultFunc = "now_ms"
)

type Table struct {
	ID          string       `json:"id"`
	SchemaID    SchemaID     `json:"schema_id"`
	Name        string       `json:"name"`
	Columns     []Column     `json:"columns"`
	PrimaryKey  []string     `json:"primary_key"`
	Indexes     []Index      `json:"indexes"`
	ForeignKeys []ForeignKey `json:"foreign_keys"`
}

type Column struct {
	ID       string   `json:"id"`
	SchemaID SchemaID `json:"schema_id"`
	Name     string   `json:"name"`
	Type     Type     `json:"type"`
	Nullable bool     `json:"nullable"`
	Format   string   `json:"format,omitempty"`
	Default  *Default `json:"default,omitempty"`
}

type Index struct {
	ID      string   `json:"id"`
	Name    string   `json:"name"`
	Columns []string `json:"columns"`
	Unique  bool     `json:"unique"`
}

type ForeignKey struct {
	ID         string   `json:"id"`
	Name       string   `json:"name"`
	Columns    []string `json:"columns"`
	RefTableID string   `json:"ref_table_id"`
	RefColumns []string `json:"ref_columns"`
}

func (t *Table) Column(name string) (Column, bool) {
	for _, column := range t.Columns {
		if column.Name == name {
			return column, true
		}
	}
	return Column{}, false
}

func (t *Table) Index(name string) (Index, bool) {
	for _, index := range t.Indexes {
		if index.Name == name {
			return index, true
		}
	}
	return Index{}, false
}

type TableDef struct {
	ID          SchemaID        `json:"id"`
	Name        string          `json:"name"`
	Columns     []ColumnDef     `json:"columns"`
	PrimaryKey  []string        `json:"primary_key"`
	Indexes     []IndexDef      `json:"indexes,omitempty"`
	ForeignKeys []ForeignKeyDef `json:"foreign_keys,omitempty"`
}

type ColumnDef struct {
	ID       SchemaID `json:"id"`
	Name     string   `json:"name"`
	Type     Type     `json:"type"`
	Nullable bool     `json:"nullable,omitempty"`
	Format   string   `json:"format,omitempty"`
	Default  *Default `json:"default,omitempty"`
}

type IndexDef struct {
	Name    string   `json:"name"`
	Columns []string `json:"columns"`
	Unique  bool     `json:"unique,omitempty"`
}

type ForeignKeyDef struct {
	Name       string   `json:"name"`
	Columns    []string `json:"columns"`
	RefTable   string   `json:"ref_table"`
	RefColumns []string `json:"ref_columns"`
}

type Revision struct {
	Version   uint64    `json:"version"`
	CreatedAt time.Time `json:"created_at"`
	Hash      string    `json:"hash"`
	Schema    Schema    `json:"schema"`
}

type Mode string

const (
	ModeDirect Mode = "direct"
	ModeSchema Mode = "schema"
)

func ParseMode(value string) (Mode, error) {
	switch Mode(value) {
	case ModeDirect, ModeSchema:
		return Mode(value), nil
	default:
		return "", fmt.Errorf("catalog: unknown catalog mode %q (direct or schema)", value)
	}
}
