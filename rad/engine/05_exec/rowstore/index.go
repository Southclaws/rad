package rowstore

// scanIndexRange: the index access primitive for the relation-graph
// executor. It scans an index by equality prefix plus an optional trailing
// range, fetching the base row for each entry as it goes — one entry, one
// Get, interleaved with the open iterator, so a lazy consumer (a slice
// stopping early) never pays for the rest of the range.
//
// The KV contract permits Gets while the range iterator remains open.

import (
	"bytes"
	"context"
	"fmt"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	keyenc "github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
)

// Range is the executor-facing trailing range: encoded from planner
// bounds against the index column right after the equality prefix.
type Range struct {
	Lo, Hi         *lir.Value
	LoIncl, HiIncl bool
}

// scanIndexRange yields base rows for index entries in [prefix ++ range)
// order. eqVals are the leading index columns' pinned values, in index
// order.
func ScanIndexRange(ctx context.Context, view kv.KV, tbl model.Table, idx model.Index, eqVals []lir.Value, rng *Range) (Iterator, error) {
	if err := admitTable(ctx, view, tbl); err != nil {
		return nil, err
	}
	if err := store.ReadIndexAccessFence(ctx, view, tbl, idx); err != nil {
		return nil, err
	}
	return ScanIndexRangeColumns(ctx, view, tbl, idx, eqVals, rng, tbl.Columns)
}

// ScanIndexRangeColumns scans an index and decodes only columns already
// admitted through a bound plan's CatalogDependencies.
func ScanIndexRangeColumns(
	ctx context.Context,
	view kv.KV,
	tbl model.Table,
	idx model.Index,
	eqVals []lir.Value,
	rng *Range,
	columns []model.Column,
) (Iterator, error) {
	prefix := codec.IndexPrefix(tbl.ID, idx.ID)
	if len(eqVals) > 0 {
		tup, err := codec.EncodeTuple(eqVals)
		if err != nil {
			return nil, err
		}
		prefix = append(prefix, tup...)
	}

	start, end := prefix, keyenc.PrefixEnd(prefix)
	if rng != nil {
		if rng.Lo != nil {
			enc, err := codec.EncodeValue(*rng.Lo)
			if err != nil {
				return nil, err
			}
			if rng.LoIncl {
				start = append(append([]byte{}, prefix...), enc...)
			} else {
				// Exclusive: skip every entry whose next column IS this
				// value — the smallest key past that value-prefix.
				start = keyenc.PrefixEnd(append(append([]byte{}, prefix...), enc...))
			}
		}
		if rng.Hi != nil {
			enc, err := codec.EncodeValue(*rng.Hi)
			if err != nil {
				return nil, err
			}
			if rng.HiIncl {
				end = keyenc.PrefixEnd(append(append([]byte{}, prefix...), enc...))
			} else {
				end = append(append([]byte{}, prefix...), enc...)
			}
		}
	}

	// An unsatisfiable range — e.g. `x > k AND x < k`, or crossing bounds —
	// can encode start at or past end. That is simply the empty result, but the
	// KV requires a non-empty [start, end): a zero-width or inverted range is
	// rejected (and would surface as a 500). Short-circuit to an empty iterator
	// instead of scanning.
	if bytes.Compare(start, end) >= 0 {
		return emptyRowIter{}, nil
	}

	it, err := view.Scan(ctx, start, end)
	if err != nil {
		return nil, err
	}
	return &indexRangeIterator{
		ctx: ctx, view: view, tbl: tbl, columns: columns, idx: idx.Name, it: it,
	}, nil
}

// emptyRowIter is an exhausted RowIterator, used when a range is provably
// empty so no KV scan is issued.
type emptyRowIter struct{}

func (emptyRowIter) Next() (lir.Row, bool, error) { return nil, false, nil }
func (emptyRowIter) Close() error                 { return nil }

// indexRangeIterator walks index entries and fetches each base row
// immediately — the entry's value is the primary-key tuple.
type indexRangeIterator struct {
	ctx     context.Context
	view    kv.KV
	tbl     model.Table
	columns []model.Column
	idx     string
	it      kv.Iterator
}

func (r *indexRangeIterator) Next() (lir.Row, bool, error) {
	if !r.it.Next() {
		if err := r.it.Err(); err != nil {
			return nil, false, err
		}
		return nil, false, nil
	}
	// The iterator's value is only valid until the next Next; the Get below
	// may invalidate it, so copy first.
	pkTuple := append([]byte{}, r.it.Value()...)
	raw, ok, err := r.view.Get(r.ctx, codec.DataKey(r.tbl.ID, pkTuple))
	if err != nil {
		return nil, false, err
	}
	if !ok {
		return nil, false, fmt.Errorf("exec: index %q points at a missing row of %q", r.idx, r.tbl.Name)
	}
	row, err := codec.UnmarshalRowColumns(r.tbl, r.columns, raw)
	if err != nil {
		return nil, false, err
	}
	return row, true, nil
}

func (r *indexRangeIterator) Close() error { return r.it.Close() }
