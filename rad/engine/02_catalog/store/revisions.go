package store

import (
	"context"
	"encoding/json"
	"fmt"
	"strconv"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/reject"
)

const (
	schemaVersionKey     = "/rad/catalog/meta/schema_version"
	schemaRevisionPrefix = "/rad/catalog/meta/schema_revision/"
)

func (r Reader) Revision(ctx context.Context) (model.Revision, error) {
	return CurrentRevision(ctx, r.view)
}

func CurrentRevision(ctx context.Context, view kv.KV) (model.Revision, error) {
	raw, ok, err := view.Get(ctx, []byte(schemaVersionKey))
	if err != nil {
		return model.Revision{}, err
	}
	if !ok {
		empty := model.Schema{}
		hash, err := empty.Hash()
		return model.Revision{Hash: hash, Schema: empty}, err
	}
	version, err := strconv.ParseUint(string(raw), 10, 64)
	if err != nil {
		return model.Revision{}, reject.Mark(reject.ReasonCatalogCorrupt,
			fmt.Errorf("catalog: corrupt schema_version %q: %w", raw, err))
	}
	revision, ok, err := readRevision(ctx, view, version)
	if err != nil {
		return model.Revision{}, err
	}
	if !ok {
		return model.Revision{}, reject.Fail(reject.ReasonCatalogCorrupt,
			"catalog: schema_version %d has no revision record", version)
	}
	return revision, nil
}

func BumpRevision(ctx context.Context, view kv.KV) (model.Revision, error) {
	current, err := CurrentRevision(ctx, view)
	if err != nil {
		return model.Revision{}, err
	}
	tables, err := New(view).ListTables(ctx)
	if err != nil {
		return model.Revision{}, err
	}
	schema, err := model.BuildSchema(tables)
	if err != nil {
		return model.Revision{}, err
	}
	hash, err := schema.Hash()
	if err != nil {
		return model.Revision{}, reject.Mark(reject.ReasonCatalogDrift,
			fmt.Errorf("catalog: hash schema revision %d: %w", current.Version+1, err))
	}
	next := model.Revision{
		Version: current.Version + 1, CreatedAt: time.Now().UTC(), Hash: hash, Schema: schema,
	}
	raw, err := json.Marshal(next)
	if err != nil {
		return model.Revision{}, reject.Mark(reject.ReasonCatalogDrift,
			fmt.Errorf("catalog: encode schema revision %d: %w", next.Version, err))
	}
	if err := view.Put(ctx, revisionKey(next.Version), raw); err != nil {
		return model.Revision{}, err
	}
	if err := view.Put(ctx, []byte(schemaVersionKey), []byte(strconv.FormatUint(next.Version, 10))); err != nil {
		return model.Revision{}, err
	}
	if err := PublishDefinitions(ctx, view, next.Version, tables, current.Schema); err != nil {
		return model.Revision{}, err
	}
	return next, nil
}

func Revisions(ctx context.Context, view kv.KV) ([]model.Revision, error) {
	prefix := []byte(schemaRevisionPrefix)
	iterator, err := view.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		return nil, err
	}
	defer iterator.Close()

	var revisions []model.Revision
	for iterator.Next() {
		var revision model.Revision
		if err := json.Unmarshal(iterator.Value(), &revision); err != nil {
			return nil, reject.Mark(reject.ReasonCatalogCorrupt,
				fmt.Errorf("catalog: corrupt schema revision %q: %w", iterator.Key(), err))
		}
		if err := validateRevision(revision); err != nil {
			return nil, err
		}
		revisions = append(revisions, revision)
	}
	if err := iterator.Err(); err != nil {
		return nil, err
	}
	return revisions, nil
}

func readRevision(ctx context.Context, view kv.KV, version uint64) (model.Revision, bool, error) {
	raw, ok, err := view.Get(ctx, revisionKey(version))
	if err != nil || !ok {
		return model.Revision{}, ok, err
	}
	var revision model.Revision
	if err := json.Unmarshal(raw, &revision); err != nil {
		return model.Revision{}, false, reject.Mark(reject.ReasonCatalogCorrupt,
			fmt.Errorf("catalog: corrupt schema revision %d: %w", version, err))
	}
	if revision.Version != version {
		return model.Revision{}, false, reject.Fail(reject.ReasonCatalogCorrupt,
			"catalog: schema revision key %d contains version %d", version, revision.Version)
	}
	if err := validateRevision(revision); err != nil {
		return model.Revision{}, false, err
	}
	return revision, true, nil
}

func validateRevision(revision model.Revision) error {
	hash, err := revision.Schema.Hash()
	if err != nil {
		return reject.Mark(reject.ReasonCatalogCorrupt,
			fmt.Errorf("catalog: hash schema revision %d: %w", revision.Version, err))
	}
	if revision.Hash != hash {
		return reject.Fail(reject.ReasonCatalogCorrupt,
			"catalog: schema revision %d hash is %q, want %q", revision.Version, revision.Hash, hash)
	}
	return nil
}

func revisionKey(version uint64) []byte {
	return []byte(fmt.Sprintf("%s%020d", schemaRevisionPrefix, version))
}
