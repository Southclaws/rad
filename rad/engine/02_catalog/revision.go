package catalog

import (
	"context"
	"encoding/json"
	"fmt"
	"strconv"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	keyenc "github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/reject"
)

const (
	schemaVersionKey     = "/rad/catalog/meta/schema_version"
	schemaRevisionPrefix = "/rad/catalog/meta/schema_revision/"
)

// Revision identifies one committed catalog change and its resulting canonical
// schema. Version zero describes a fresh database as {} and has no timestamp
// because no schema change has happened.
type Revision struct {
	Version   uint64    `json:"version"`
	CreatedAt time.Time `json:"created_at"`
	Schema    Schema    `json:"schema"`
}

// Mutation is a group of catalog operations which will become one revision.
// It is bound to a caller-owned KV view, normally a serializable transaction.
type Mutation struct {
	view    kv.KV
	changed bool
}

// MutateIn applies fn to a caller-owned transaction and records one revision
// if fn made any catalog changes. Requiring a transaction keeps catalog edits,
// schema history, and associated index backfills on one atomic boundary.
func MutateIn(ctx context.Context, view kv.Txn, fn func(change *Mutation) error) (Revision, error) {
	change := &Mutation{view: view}
	if err := fn(change); err != nil {
		return Revision{}, err
	}
	if !change.changed {
		return currentRevision(ctx, view)
	}
	return bumpRevision(ctx, view)
}

// Revision reports the latest committed catalog revision.
func (c *Catalog) Revision(ctx context.Context) (Revision, error) {
	return currentRevision(ctx, c.store)
}

// Revisions returns every committed catalog revision in version order.
func (c *Catalog) Revisions(ctx context.Context) ([]Revision, error) {
	return revisions(ctx, c.store)
}

// ListTables reads the catalog through this mutation's transaction view.
func (m *Mutation) ListTables(ctx context.Context) ([]Table, error) {
	return listTables(ctx, m.view)
}

func currentRevision(ctx context.Context, view kv.KV) (Revision, error) {
	raw, ok, err := view.Get(ctx, []byte(schemaVersionKey))
	if err != nil {
		return Revision{}, err
	}
	if !ok {
		return Revision{Schema: Schema{}}, nil
	}
	version, err := strconv.ParseUint(string(raw), 10, 64)
	if err != nil {
		return Revision{}, reject.Mark(reject.ReasonCatalogCorrupt,
			fmt.Errorf("catalog: corrupt schema_version %q: %w", raw, err))
	}
	record, ok, err := readRevision(ctx, view, version)
	if err != nil {
		return Revision{}, err
	}
	if !ok {
		return Revision{}, reject.Fail(reject.ReasonCatalogCorrupt,
			"catalog: schema_version %d has no revision record", version)
	}
	return record, nil
}

func bumpRevision(ctx context.Context, view kv.KV) (Revision, error) {
	current, err := currentRevision(ctx, view)
	if err != nil {
		return Revision{}, err
	}
	schema, err := schemaIn(ctx, view)
	if err != nil {
		return Revision{}, err
	}
	next := Revision{Version: current.Version + 1, CreatedAt: time.Now().UTC(), Schema: schema}
	raw, err := json.Marshal(next)
	if err != nil {
		return Revision{}, reject.Mark(reject.ReasonCatalogDrift,
			fmt.Errorf("catalog: encode schema revision %d: %w", next.Version, err))
	}
	if err := view.Put(ctx, []byte(revisionKey(next.Version)), raw); err != nil {
		return Revision{}, err
	}
	if err := view.Put(ctx, []byte(schemaVersionKey), []byte(strconv.FormatUint(next.Version, 10))); err != nil {
		return Revision{}, err
	}
	return next, nil
}

func revisions(ctx context.Context, view kv.KV) ([]Revision, error) {
	prefix := []byte(schemaRevisionPrefix)
	it, err := view.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		return nil, err
	}
	defer it.Close()

	var out []Revision
	for it.Next() {
		var revision Revision
		if err := json.Unmarshal(it.Value(), &revision); err != nil {
			return nil, reject.Mark(reject.ReasonCatalogCorrupt,
				fmt.Errorf("catalog: corrupt schema revision %q: %w", it.Key(), err))
		}
		out = append(out, revision)
	}
	if err := it.Err(); err != nil {
		return nil, err
	}
	return out, nil
}

func readRevision(ctx context.Context, view kv.KV, version uint64) (Revision, bool, error) {
	raw, ok, err := view.Get(ctx, []byte(revisionKey(version)))
	if err != nil || !ok {
		return Revision{}, ok, err
	}
	var revision Revision
	if err := json.Unmarshal(raw, &revision); err != nil {
		return Revision{}, false, reject.Mark(reject.ReasonCatalogCorrupt,
			fmt.Errorf("catalog: corrupt schema revision %d: %w", version, err))
	}
	if revision.Version != version {
		return Revision{}, false, reject.Fail(reject.ReasonCatalogCorrupt,
			"catalog: schema revision key %d contains version %d", version, revision.Version)
	}
	return revision, true, nil
}

func revisionKey(version uint64) string {
	return fmt.Sprintf("%s%020d", schemaRevisionPrefix, version)
}
