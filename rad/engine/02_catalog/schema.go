package catalog

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/json"
	"fmt"
	"slices"
	"strings"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// Schema is the canonical logical description of a Rad catalog. It includes
// stable schema identities but excludes opaque physical catalog IDs. It is the
// durable shape used by revision history and rebuilt from physical metadata
// when checking for drift. An empty schema marshals as {}.
type Schema struct {
	Tables []TableDef `json:"tables,omitempty"`
}

// CanonicalJSON returns the stable JSON representation of s.
func (s Schema) CanonicalJSON() ([]byte, error) {
	return json.Marshal(s)
}

// Hash identifies the schema's canonical logical content. Physical catalog
// IDs, source formatting, comments, and YAML key order never participate.
func (s Schema) Hash() (string, error) {
	raw, err := s.CanonicalJSON()
	if err != nil {
		return "", err
	}
	sum := sha256.Sum256(raw)
	return fmt.Sprintf("sha256:%x", sum), nil
}

// Equal reports whether two canonical schemas have identical JSON forms.
func (s Schema) Equal(other Schema) (bool, error) {
	a, err := s.CanonicalJSON()
	if err != nil {
		return false, err
	}
	b, err := other.CanonicalJSON()
	if err != nil {
		return false, err
	}
	return bytes.Equal(a, b), nil
}

// SchemaFromDefinitions canonicalizes logical table definitions. Table order
// is identity-based; index and foreign-key order is name-based. Column and
// key-column order is retained because it is part of the declared shape.
func SchemaFromDefinitions(defs []TableDef) Schema {
	tables := make([]TableDef, len(defs))
	for i, def := range defs {
		tables[i] = cloneTableDef(def)
		slices.SortFunc(tables[i].Indexes, compareIndexDefs)
		slices.SortFunc(tables[i].ForeignKeys, compareForeignKeyDefs)
	}
	slices.SortFunc(tables, func(a, b TableDef) int {
		if a.ID < b.ID {
			return -1
		}
		if a.ID > b.ID {
			return 1
		}
		return strings.Compare(a.Name, b.Name)
	})
	return Schema{Tables: tables}
}

func compareIndexDefs(a, b IndexDef) int {
	if byName := strings.Compare(a.Name, b.Name); byName != 0 {
		return byName
	}
	if byColumns := strings.Compare(strings.Join(a.Columns, "\x00"), strings.Join(b.Columns, "\x00")); byColumns != 0 {
		return byColumns
	}
	if a.Unique == b.Unique {
		return 0
	}
	if !a.Unique {
		return -1
	}
	return 1
}

func compareForeignKeyDefs(a, b ForeignKeyDef) int {
	valuesA := []string{a.Name, strings.Join(a.Columns, "\x00"), a.RefTable, strings.Join(a.RefColumns, "\x00")}
	valuesB := []string{b.Name, strings.Join(b.Columns, "\x00"), b.RefTable, strings.Join(b.RefColumns, "\x00")}
	for i := range valuesA {
		if compared := strings.Compare(valuesA[i], valuesB[i]); compared != 0 {
			return compared
		}
	}
	return 0
}

// BuildSchema reconstructs a canonical schema from physical catalog tables,
// retaining logical schema IDs, removing opaque physical IDs, and resolving
// foreign-key table IDs back to names.
func BuildSchema(tables []Table) (Schema, error) {
	nameByID := make(map[string]string, len(tables))
	seenNames := make(map[string]bool, len(tables))
	seenSchemaIDs := make(map[SchemaID]string, len(tables))
	for _, table := range tables {
		if previous, exists := nameByID[table.ID]; exists {
			return Schema{}, reject.Fail(reject.ReasonCatalogDrift,
				"catalog: tables %q and %q share physical ID %q", previous, table.Name, table.ID)
		}
		if seenNames[table.Name] {
			return Schema{}, reject.Fail(reject.ReasonCatalogDrift,
				"catalog: duplicate physical table name %q", table.Name)
		}
		if table.SchemaID == 0 || table.SchemaID > MaxSchemaID {
			return Schema{}, reject.Fail(reject.ReasonCatalogDrift,
				"catalog: physical table %q has invalid schema ID %d", table.Name, table.SchemaID)
		}
		if previous, exists := seenSchemaIDs[table.SchemaID]; exists {
			return Schema{}, reject.Fail(reject.ReasonCatalogDrift,
				"catalog: tables %q and %q share schema ID %d", previous, table.Name, table.SchemaID)
		}
		nameByID[table.ID] = table.Name
		seenNames[table.Name] = true
		seenSchemaIDs[table.SchemaID] = table.Name
	}

	defs := make([]TableDef, 0, len(tables))
	for _, table := range tables {
		def := TableDef{
			ID:         table.SchemaID,
			Name:       table.Name,
			PrimaryKey: slices.Clone(table.PrimaryKey),
		}
		seenColumnIDs := make(map[SchemaID]string, len(table.Columns))
		for _, column := range table.Columns {
			if column.SchemaID == 0 || column.SchemaID > MaxSchemaID {
				return Schema{}, reject.Fail(reject.ReasonCatalogDrift,
					"catalog: physical column %q.%q has invalid schema ID %d",
					table.Name, column.Name, column.SchemaID)
			}
			if previous, exists := seenColumnIDs[column.SchemaID]; exists {
				return Schema{}, reject.Fail(reject.ReasonCatalogDrift,
					"catalog: columns %q.%q and %q.%q share schema ID %d",
					table.Name, previous, table.Name, column.Name, column.SchemaID)
			}
			seenColumnIDs[column.SchemaID] = column.Name
			var defaultValue *Default
			if column.Default != nil {
				copy := *column.Default
				defaultValue = &copy
			}
			def.Columns = append(def.Columns, ColumnDef{
				ID: column.SchemaID, Name: column.Name, Type: column.Type, Nullable: column.Nullable,
				Format: column.Format, Default: defaultValue,
			})
		}
		for _, index := range table.Indexes {
			def.Indexes = append(def.Indexes, IndexDef{
				Name: index.Name, Columns: slices.Clone(index.Columns), Unique: index.Unique,
			})
		}
		for _, foreignKey := range table.ForeignKeys {
			refTable, ok := nameByID[foreignKey.RefTableID]
			if !ok {
				return Schema{}, reject.Fail(reject.ReasonCatalogDrift,
					"catalog: foreign key %q on table %q references missing physical table ID %q",
					foreignKey.Name, table.Name, foreignKey.RefTableID)
			}
			def.ForeignKeys = append(def.ForeignKeys, ForeignKeyDef{
				Name: foreignKey.Name, Columns: slices.Clone(foreignKey.Columns),
				RefTable: refTable, RefColumns: slices.Clone(foreignKey.RefColumns),
			})
		}
		defs = append(defs, def)
	}
	return SchemaFromDefinitions(defs), nil
}

// Schema rebuilds the current canonical schema from committed catalog state.
func (c *Catalog) Schema(ctx context.Context) (Schema, error) {
	return schemaIn(ctx, c.store)
}

// ValidateCurrentSchema compares the latest stored revision snapshot with the
// physical catalog under one read snapshot.
func (c *Catalog) ValidateCurrentSchema(ctx context.Context) error {
	txn, err := c.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		return err
	}
	defer txn.Rollback()

	revision, err := currentRevision(ctx, txn)
	if err != nil {
		return err
	}
	actual, err := schemaIn(ctx, txn)
	if err != nil {
		return err
	}
	equal, err := revision.Schema.Equal(actual)
	if err != nil {
		return reject.Mark(reject.ReasonCatalogDrift,
			fmt.Errorf("catalog: encode schema for drift validation: %w", err))
	}
	if !equal {
		return reject.Fail(reject.ReasonCatalogDrift,
			"catalog: stored schema at version %d does not match physical catalog metadata", revision.Version)
	}
	return nil
}

func schemaIn(ctx context.Context, view kv.KV) (Schema, error) {
	tables, err := listTables(ctx, view)
	if err != nil {
		return Schema{}, err
	}
	return BuildSchema(tables)
}

func cloneTableDef(def TableDef) TableDef {
	out := TableDef{
		ID:          def.ID,
		Name:        def.Name,
		PrimaryKey:  slices.Clone(def.PrimaryKey),
		Indexes:     make([]IndexDef, len(def.Indexes)),
		ForeignKeys: make([]ForeignKeyDef, len(def.ForeignKeys)),
	}
	for _, column := range def.Columns {
		copy := column
		if column.Default != nil {
			defaultCopy := *column.Default
			copy.Default = &defaultCopy
		}
		out.Columns = append(out.Columns, copy)
	}
	for i, index := range def.Indexes {
		out.Indexes[i] = IndexDef{
			Name: index.Name, Columns: slices.Clone(index.Columns), Unique: index.Unique,
		}
	}
	for i, foreignKey := range def.ForeignKeys {
		out.ForeignKeys[i] = ForeignKeyDef{
			Name: foreignKey.Name, Columns: slices.Clone(foreignKey.Columns),
			RefTable: foreignKey.RefTable, RefColumns: slices.Clone(foreignKey.RefColumns),
		}
	}
	return out
}
