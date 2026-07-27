package store

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"strconv"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

const (
	tableDefinitionPrefix = "/rad/catalog/object/table/"
	tableHeadPrefix       = "/rad/catalog/head/table/"
)

func TableDefinitionKey(id model.SchemaID, generation uint64) []byte {
	return []byte(fmt.Sprintf("%s%010d/definition/%020d", tableDefinitionPrefix, id, generation))
}

func TableDefinitionRange(id model.SchemaID) (start, end []byte) {
	prefix := []byte(fmt.Sprintf("%s%010d/definition/", tableDefinitionPrefix, id))
	return prefix, keyenc.PrefixEnd(prefix)
}

func TableHeadKey(id model.SchemaID) []byte {
	return []byte(fmt.Sprintf("%s%010d", tableHeadPrefix, id))
}

// PublishDefinitions persists the physical definitions that produced one
// canonical catalog revision. Execution pins these immutable values in memory;
// the heads are only for admission/bootstrap and are never reread by an
// already-bound plan.
func PublishDefinitions(ctx context.Context, view kv.KV, version uint64, tables []model.Table, previous model.Schema) error {
	live := make(map[model.SchemaID]bool, len(tables))
	for _, table := range tables {
		live[table.SchemaID] = true
		_, previousGeneration, hadPrevious, err := DefinitionHead(ctx, view, table.SchemaID)
		if err != nil {
			return err
		}
		raw, err := json.Marshal(table)
		if err != nil {
			return err
		}
		definitionKey := TableDefinitionKey(table.SchemaID, table.DefinitionGeneration)
		if existing, ok, err := view.Get(ctx, definitionKey); err != nil {
			return err
		} else if ok && !bytes.Equal(existing, raw) {
			return fmt.Errorf("catalog: immutable table definition %d generation %d already has different contents", table.SchemaID, table.DefinitionGeneration)
		} else if !ok {
			if err := view.Put(ctx, definitionKey, raw); err != nil {
				return err
			}
		}
		head := fmt.Sprintf("%d:%d", version, table.DefinitionGeneration)
		if err := view.Put(ctx, TableHeadKey(table.SchemaID), []byte(head)); err != nil {
			return err
		}
		if hadPrevious && previousGeneration != table.DefinitionGeneration {
			if err := QueueReclamation(ctx, view, model.Reclamation{
				ID:   TableDefinitionReclamationID(table.SchemaID, previousGeneration),
				Kind: model.ReclamationTableDefinition, RetiredCatalogVersion: version,
				TableSchemaID: table.SchemaID, DefinitionGeneration: previousGeneration,
			}); err != nil {
				return err
			}
		}
	}
	for _, table := range previous.Tables {
		if live[table.ID] {
			continue
		}
		_, previousGeneration, ok, err := DefinitionHead(ctx, view, table.ID)
		if err != nil {
			return err
		}
		if ok {
			if err := QueueReclamation(ctx, view, model.Reclamation{
				ID:   TableDefinitionReclamationID(table.ID, previousGeneration),
				Kind: model.ReclamationTableDefinition, RetiredCatalogVersion: version,
				TableSchemaID: table.ID, DefinitionGeneration: previousGeneration,
			}); err != nil {
				return err
			}
		}
		if err := view.Delete(ctx, TableHeadKey(table.ID)); err != nil {
			return err
		}
	}
	return nil
}

func DefinitionHead(ctx context.Context, view kv.KV, id model.SchemaID) (catalogVersion, definitionGeneration uint64, ok bool, err error) {
	raw, ok, err := view.Get(ctx, TableHeadKey(id))
	if err != nil || !ok {
		return 0, 0, ok, err
	}
	var split int
	for split < len(raw) && raw[split] != ':' {
		split++
	}
	if split == len(raw) {
		return 0, 0, false, fmt.Errorf("catalog: corrupt table definition head %q", raw)
	}
	catalogVersion, err = strconv.ParseUint(string(raw[:split]), 10, 64)
	if err != nil {
		return 0, 0, false, fmt.Errorf("catalog: corrupt table definition catalog version %q: %w", raw, err)
	}
	definitionGeneration, err = strconv.ParseUint(string(raw[split+1:]), 10, 64)
	if err != nil {
		return 0, 0, false, fmt.Errorf("catalog: corrupt table definition generation %q: %w", raw, err)
	}
	return catalogVersion, definitionGeneration, true, nil
}
