package query

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
	planner "github.com/Southclaws/rad/rad/engine/04_planner/physical"
	"github.com/Southclaws/rad/rad/engine/05_exec/rowstore"
)

type pkGetOp struct {
	scan  *bound.Scan
	row   lir.Row
	outer lireval.Env
	done  bool
}

func (ex *Executor) buildPKGet(ctx context.Context, n *planner.PKGetExec, outer lireval.Env) (operator, error) {
	key := lir.Row{}
	for i, column := range n.Scan.Table.PrimaryKey {
		value, err := resolveConst(n.Key[i], outer)
		if err != nil {
			return nil, err
		}
		if value.Null {
			return &pkGetOp{scan: n.Scan, outer: outer}, nil
		}
		key[column] = value
	}
	row, _, _, err := rowstore.Get(ctx, ex.view, n.Scan.Table, key)
	if err != nil {
		return nil, err
	}
	return &pkGetOp{scan: n.Scan, row: row, outer: outer}, nil
}

func (o *pkGetOp) Next(context.Context) (lireval.Env, bool, error) {
	if o.done || o.row == nil {
		return lireval.Env{}, false, nil
	}
	o.done = true
	return rowToFrame(o.scan, o.row, o.outer), true, nil
}

func (o *pkGetOp) Close() error { return nil }

type rowIterOp struct {
	scan  *bound.Scan
	it    rowstore.Iterator
	outer lireval.Env
}

func (o *rowIterOp) Next(context.Context) (lireval.Env, bool, error) {
	row, ok, err := o.it.Next()
	if err != nil || !ok {
		return lireval.Env{}, false, err
	}
	return rowToFrame(o.scan, row, o.outer), true, nil
}

func (o *rowIterOp) Close() error { return o.it.Close() }

func (ex *Executor) buildIndexScan(ctx context.Context, n *planner.IndexRangeScanExec, outer lireval.Env) (operator, error) {
	equal := make([]lir.Value, len(n.EqPrefix))
	for i, constant := range n.EqPrefix {
		value, err := resolveConst(constant, outer)
		if err != nil {
			return nil, err
		}
		if value.Null {
			return emptyOp{}, nil
		}
		equal[i] = value
	}
	var bounds *rowstore.Range
	if n.Range != nil {
		bounds = &rowstore.Range{}
		if n.Range.Lo != nil {
			value := n.Range.Lo.V
			bounds.Lo, bounds.LoIncl = &value, n.Range.Lo.Inclusive
		}
		if n.Range.Hi != nil {
			value := n.Range.Hi.V
			bounds.Hi, bounds.HiIncl = &value, n.Range.Hi.Inclusive
		}
	}
	it, err := rowstore.ScanIndexRange(ctx, ex.view, n.Scan.Table, n.Index, equal, bounds)
	if err != nil {
		return nil, err
	}
	return &rowIterOp{scan: n.Scan, it: it, outer: outer}, nil
}

type emptyOp struct{}

func (emptyOp) Next(context.Context) (lireval.Env, bool, error) { return lireval.Env{}, false, nil }
func (emptyOp) Close() error                                    { return nil }
