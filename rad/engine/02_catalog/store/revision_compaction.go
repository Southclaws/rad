package store

import (
	"context"
	"fmt"
	"strconv"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
)

const revisionCompactedThroughKey = "/rad/catalog/meta/schema_revision_compacted_through"

func RevisionCompactedThrough(ctx context.Context, view kv.KV) (uint64, error) {
	raw, ok, err := view.Get(ctx, []byte(revisionCompactedThroughKey))
	if err != nil || !ok {
		return 0, err
	}
	version, err := strconv.ParseUint(string(raw), 10, 64)
	if err != nil {
		return 0, fmt.Errorf("catalog: corrupt compacted revision horizon %q: %w", raw, err)
	}
	return version, nil
}

// RevisionCompactionNeeded reports whether a canonical revision older than
// the retained recent window can be removed. CurrentRevision remains a point
// read of the current canonical revision and does not depend on this history.
func RevisionCompactionNeeded(ctx context.Context, view kv.KV, retainRecent uint64) (bool, error) {
	if retainRecent == 0 {
		return false, nil
	}
	current, err := CurrentRevision(ctx, view)
	if err != nil {
		return false, err
	}
	if current.Version <= retainRecent {
		return false, nil
	}
	compactedThrough, err := RevisionCompactedThrough(ctx, view)
	if err != nil {
		return false, err
	}
	return compactedThrough < current.Version-retainRecent, nil
}

// CompactRevisionHistoryBatch removes at most batchSize old canonical
// revisions while retaining the current revision and retainRecent-1 immediate
// predecessors. It touches no immutable object definitions, retention pins,
// compatibility fences, transition records, or physical data.
func CompactRevisionHistoryBatch(
	ctx context.Context,
	view kv.KV,
	retainRecent uint64,
	batchSize int,
) (deleted int, more bool, err error) {
	if retainRecent == 0 {
		return 0, false, fmt.Errorf("catalog: revision retention must be positive")
	}
	if batchSize <= 0 {
		return 0, false, fmt.Errorf("catalog: revision compaction batch size must be positive")
	}
	current, err := CurrentRevision(ctx, view)
	if err != nil {
		return 0, false, err
	}
	if current.Version <= retainRecent {
		return 0, false, nil
	}
	target := current.Version - retainRecent
	compactedThrough, err := RevisionCompactedThrough(ctx, view)
	if err != nil {
		return 0, false, err
	}
	for version := compactedThrough + 1; version <= target && deleted < batchSize; version++ {
		if err := view.Delete(ctx, revisionKey(version)); err != nil {
			return deleted, false, err
		}
		compactedThrough = version
		deleted++
	}
	if deleted != 0 {
		if err := view.Put(
			ctx,
			[]byte(revisionCompactedThroughKey),
			[]byte(strconv.FormatUint(compactedThrough, 10)),
		); err != nil {
			return deleted, false, err
		}
	}
	return deleted, compactedThrough < target, nil
}
