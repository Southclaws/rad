package catalog

import (
	"context"
	"fmt"

	"github.com/Southclaws/rad/rad/engine/reject"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
)

// SchemaID is a human-authored logical identity. It is independent of the
// catalog's opaque physical IDs and remains stable when a table or column is
// renamed. Zero is reserved to mean "allocate one" on direct catalog calls;
// rad.schema.yaml requires every ID explicitly.
type SchemaID uint32

// MaxSchemaID keeps IDs in a simple positive signed-31-bit range that every
// public API client can represent exactly.
const MaxSchemaID SchemaID = 1<<31 - 1

func assignTableDefinitionIDs(ctx context.Context, view kv.KV, def TableDef) (TableDef, error) {
	used, maximum, err := usedTableSchemaIDs(ctx, view)
	if err != nil {
		return TableDef{}, err
	}
	if def.ID == 0 {
		def.ID, err = nextSchemaID(maximum)
		if err != nil {
			return TableDef{}, err
		}
	} else if def.ID > MaxSchemaID {
		return TableDef{}, reject.Inputf("catalog: table schema ID %d exceeds maximum %d", def.ID, MaxSchemaID)
	} else if used[def.ID] {
		return TableDef{}, reject.Inputf("catalog: table schema ID %d has already been used", def.ID)
	}

	seen := make(map[SchemaID]string, len(def.Columns))
	var columnMaximum SchemaID
	for _, column := range def.Columns {
		if column.ID == 0 {
			continue
		}
		if column.ID > MaxSchemaID {
			return TableDef{}, reject.Inputf(
				"catalog: column %q on table %q has schema ID %d above maximum %d",
				column.Name, def.Name, column.ID, MaxSchemaID)
		}
		if previous, exists := seen[column.ID]; exists {
			return TableDef{}, reject.Inputf(
				"catalog: columns %q and %q on table %q share schema ID %d",
				previous, column.Name, def.Name, column.ID)
		}
		seen[column.ID] = column.Name
		columnMaximum = max(columnMaximum, column.ID)
	}
	for i := range def.Columns {
		if def.Columns[i].ID != 0 {
			continue
		}
		columnMaximum, err = nextSchemaID(columnMaximum)
		if err != nil {
			return TableDef{}, err
		}
		def.Columns[i].ID = columnMaximum
	}
	return def, nil
}

func assignColumnDefinitionID(ctx context.Context, view kv.KV, table Table, def ColumnDef) (ColumnDef, error) {
	used, maximum, err := usedColumnSchemaIDs(ctx, view, table)
	if err != nil {
		return ColumnDef{}, err
	}
	if def.ID == 0 {
		def.ID, err = nextSchemaID(maximum)
		if err != nil {
			return ColumnDef{}, err
		}
		return def, nil
	}
	if def.ID > MaxSchemaID {
		return ColumnDef{}, reject.Inputf(
			"catalog: column %q on table %q has schema ID %d above maximum %d",
			def.Name, table.Name, def.ID, MaxSchemaID)
	}
	if used[def.ID] {
		return ColumnDef{}, reject.Inputf(
			"catalog: column schema ID %d on table %q has already been used", def.ID, table.Name)
	}
	return def, nil
}

func usedTableSchemaIDs(ctx context.Context, view kv.KV) (map[SchemaID]bool, SchemaID, error) {
	used := map[SchemaID]bool{}
	var maximum SchemaID
	revisions, err := revisions(ctx, view)
	if err != nil {
		return nil, 0, err
	}
	for _, revision := range revisions {
		for _, table := range revision.Schema.Tables {
			if table.ID == 0 || table.ID > MaxSchemaID {
				return nil, 0, reject.Fail(reject.ReasonCatalogCorrupt,
					"catalog: schema revision %d contains table %q with invalid schema ID %d",
					revision.Version, table.Name, table.ID)
			}
			used[table.ID] = true
			maximum = max(maximum, table.ID)
		}
	}
	tables, err := listTables(ctx, view)
	if err != nil {
		return nil, 0, err
	}
	for _, table := range tables {
		if table.SchemaID == 0 || table.SchemaID > MaxSchemaID {
			return nil, 0, reject.Fail(reject.ReasonCatalogCorrupt,
				"catalog: physical table %q has invalid schema ID %d", table.Name, table.SchemaID)
		}
		used[table.SchemaID] = true
		maximum = max(maximum, table.SchemaID)
	}
	return used, maximum, nil
}

func usedColumnSchemaIDs(ctx context.Context, view kv.KV, table Table) (map[SchemaID]bool, SchemaID, error) {
	used := map[SchemaID]bool{}
	var maximum SchemaID
	revisions, err := revisions(ctx, view)
	if err != nil {
		return nil, 0, err
	}
	for _, revision := range revisions {
		for _, historicalTable := range revision.Schema.Tables {
			if historicalTable.ID != table.SchemaID {
				continue
			}
			for _, column := range historicalTable.Columns {
				if column.ID == 0 || column.ID > MaxSchemaID {
					return nil, 0, reject.Fail(reject.ReasonCatalogCorrupt,
						"catalog: schema revision %d contains column %q.%q with invalid schema ID %d",
						revision.Version, historicalTable.Name, column.Name, column.ID)
				}
				used[column.ID] = true
				maximum = max(maximum, column.ID)
			}
		}
	}
	for _, column := range table.Columns {
		if column.SchemaID == 0 || column.SchemaID > MaxSchemaID {
			return nil, 0, reject.Fail(reject.ReasonCatalogCorrupt,
				"catalog: physical column %q.%q has invalid schema ID %d",
				table.Name, column.Name, column.SchemaID)
		}
		used[column.SchemaID] = true
		maximum = max(maximum, column.SchemaID)
	}
	return used, maximum, nil
}

func nextSchemaID(current SchemaID) (SchemaID, error) {
	if current >= MaxSchemaID {
		return 0, fmt.Errorf("catalog: schema ID space exhausted")
	}
	return current + 1, nil
}
