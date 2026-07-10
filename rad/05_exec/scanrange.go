package exec

// scanIndexRange: the index access primitive for the relation-graph
// executor. It scans an index by equality prefix plus an optional trailing
// range, fetching the base row for each entry as it goes — one entry, one
// Get, interleaved with the open iterator, so a lazy consumer (a slice
// stopping early) never pays for the rest of the range.
//
// A conformance test pins that interleaving Gets with an open iterator works
// inside kvslate transactions; if a future backend can't do it, this file is
// where a chunked fallback goes.

import (
	"context"
	"fmt"

	kv "rad/rad/01_kv"
	keyenc "rad/rad/01_kv/keyenc"
	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
)

// rangeBounds is the executor-facing trailing range: encoded from planner
// bounds against the index column right after the equality prefix.
type rangeBounds struct {
	lo, hi          *qir.Value
	loIncl, hiIncl  bool
}

// scanIndexRange yields base rows for index entries in [prefix ++ range)
// order. eqVals are the leading index columns' pinned values, in index
// order.
func scanIndexRange(ctx context.Context, view kv.KV, tbl catalog.Table, idx catalog.Index, eqVals []qir.Value, rng *rangeBounds) (RowIterator, error) {
	prefix := IndexPrefix(tbl.ID, idx.ID)
	if len(eqVals) > 0 {
		tup, err := EncodeTuple(eqVals)
		if err != nil {
			return nil, err
		}
		prefix = append(prefix, tup...)
	}

	start, end := prefix, keyenc.PrefixEnd(prefix)
	if rng != nil {
		if rng.lo != nil {
			enc, err := encodeValue(*rng.lo)
			if err != nil {
				return nil, err
			}
			if rng.loIncl {
				start = append(append([]byte{}, prefix...), enc...)
			} else {
				// Exclusive: skip every entry whose next column IS this
				// value — the smallest key past that value-prefix.
				start = keyenc.PrefixEnd(append(append([]byte{}, prefix...), enc...))
			}
		}
		if rng.hi != nil {
			enc, err := encodeValue(*rng.hi)
			if err != nil {
				return nil, err
			}
			if rng.hiIncl {
				end = keyenc.PrefixEnd(append(append([]byte{}, prefix...), enc...))
			} else {
				end = append(append([]byte{}, prefix...), enc...)
			}
		}
	}

	it, err := view.Scan(ctx, start, end)
	if err != nil {
		return nil, err
	}
	return &indexRangeIterator{ctx: ctx, view: view, tbl: tbl, it: it}, nil
}

// indexRangeIterator walks index entries and fetches each base row
// immediately — the entry's value is the primary-key tuple.
type indexRangeIterator struct {
	ctx  context.Context
	view kv.KV
	tbl  catalog.Table
	it   kv.Iterator
}

func (r *indexRangeIterator) Next() (qir.Row, bool, error) {
	if !r.it.Next() {
		if err := r.it.Err(); err != nil {
			return nil, false, err
		}
		return nil, false, nil
	}
	// The iterator's value is only valid until the next Next; the Get below
	// may invalidate it, so copy first.
	pkTuple := append([]byte{}, r.it.Value()...)
	raw, ok, err := r.view.Get(r.ctx, DataKey(r.tbl.ID, pkTuple))
	if err != nil {
		return nil, false, err
	}
	if !ok {
		return nil, false, fmt.Errorf("exec: index %q points at a missing row of %q", r.tbl.Name, r.tbl.Name)
	}
	row, err := UnmarshalRow(r.tbl, raw)
	if err != nil {
		return nil, false, err
	}
	return row, true, nil
}

func (r *indexRangeIterator) Close() error { return r.it.Close() }
