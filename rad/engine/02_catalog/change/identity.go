package change

import (
	"context"
	"fmt"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func assignTableDefinitionIDs(ctx context.Context, view kv.KV, def model.TableDef) (model.TableDef, error) {
	used, maximum, err := usedTableSchemaIDs(ctx, view)
	if err != nil {
		return model.TableDef{}, err
	}
	if def.ID == 0 {
		def.ID, err = nextSchemaID(maximum)
		if err != nil {
			return model.TableDef{}, err
		}
	} else if def.ID > model.MaxSchemaID {
		return model.TableDef{}, reject.Inputf("catalog: table schema ID %d exceeds maximum %d", def.ID, model.MaxSchemaID)
	} else if used[def.ID] {
		return model.TableDef{}, reject.Inputf("catalog: table schema ID %d has already been used", def.ID)
	}

	seen := make(map[model.SchemaID]string, len(def.Columns))
	var columnMaximum model.SchemaID
	for _, column := range def.Columns {
		if column.ID == 0 {
			continue
		}
		if column.ID > model.MaxSchemaID {
			return model.TableDef{}, reject.Inputf(
				"catalog: column %q on table %q has schema ID %d above maximum %d",
				column.Name, def.Name, column.ID, model.MaxSchemaID)
		}
		if previous, exists := seen[column.ID]; exists {
			return model.TableDef{}, reject.Inputf(
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
			return model.TableDef{}, err
		}
		def.Columns[i].ID = columnMaximum
	}
	return def, nil
}

func assignColumnDefinitionID(ctx context.Context, view kv.KV, table model.Table, def model.ColumnDef) (model.ColumnDef, error) {
	used, maximum, err := usedColumnSchemaIDs(ctx, view, table)
	if err != nil {
		return model.ColumnDef{}, err
	}
	if def.ID == 0 {
		def.ID, err = nextSchemaID(maximum)
		if err != nil {
			return model.ColumnDef{}, err
		}
		return def, nil
	}
	if def.ID > model.MaxSchemaID {
		return model.ColumnDef{}, reject.Inputf(
			"catalog: column %q on table %q has schema ID %d above maximum %d",
			def.Name, table.Name, def.ID, model.MaxSchemaID)
	}
	if used[def.ID] {
		return model.ColumnDef{}, reject.Inputf(
			"catalog: column schema ID %d on table %q has already been used", def.ID, table.Name)
	}
	return def, nil
}

func usedTableSchemaIDs(ctx context.Context, view kv.KV) (map[model.SchemaID]bool, model.SchemaID, error) {
	used := map[model.SchemaID]bool{}
	var maximum model.SchemaID
	revisions, err := store.Revisions(ctx, view)
	if err != nil {
		return nil, 0, err
	}
	for _, revision := range revisions {
		for _, table := range revision.Schema.Tables {
			if table.ID == 0 || table.ID > model.MaxSchemaID {
				return nil, 0, reject.Fail(reject.ReasonCatalogCorrupt,
					"catalog: schema revision %d contains table %q with invalid schema ID %d",
					revision.Version, table.Name, table.ID)
			}
			used[table.ID] = true
			maximum = max(maximum, table.ID)
		}
	}
	tables, err := store.New(view).ListTables(ctx)
	if err != nil {
		return nil, 0, err
	}
	for _, table := range tables {
		if table.SchemaID == 0 || table.SchemaID > model.MaxSchemaID {
			return nil, 0, reject.Fail(reject.ReasonCatalogCorrupt,
				"catalog: physical table %q has invalid schema ID %d", table.Name, table.SchemaID)
		}
		used[table.SchemaID] = true
		maximum = max(maximum, table.SchemaID)
	}
	return used, maximum, nil
}

func usedColumnSchemaIDs(ctx context.Context, view kv.KV, table model.Table) (map[model.SchemaID]bool, model.SchemaID, error) {
	used := map[model.SchemaID]bool{}
	var maximum model.SchemaID
	revisions, err := store.Revisions(ctx, view)
	if err != nil {
		return nil, 0, err
	}
	for _, revision := range revisions {
		for _, historicalTable := range revision.Schema.Tables {
			if historicalTable.ID != table.SchemaID {
				continue
			}
			for _, column := range historicalTable.Columns {
				if column.ID == 0 || column.ID > model.MaxSchemaID {
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
		if column.SchemaID == 0 || column.SchemaID > model.MaxSchemaID {
			return nil, 0, reject.Fail(reject.ReasonCatalogCorrupt,
				"catalog: physical column %q.%q has invalid schema ID %d",
				table.Name, column.Name, column.SchemaID)
		}
		used[column.SchemaID] = true
		maximum = max(maximum, column.SchemaID)
	}
	return used, maximum, nil
}

func nextSchemaID(current model.SchemaID) (model.SchemaID, error) {
	if current >= model.MaxSchemaID {
		return 0, fmt.Errorf("catalog: schema ID space exhausted")
	}
	return current + 1, nil
}
