package exec

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
)

type pkGetOp struct {
	scan  *bound.Scan
	row   lir.Row
	outer bound.Env
	done  bool
}

func (ex *executor) buildPKGet(ctx context.Context, n *planner.PKGetExec, outer bound.Env) (operator, error) {
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
	row, _, err := loadByPK(ctx, ex.view, n.Scan.Table, key)
	if err != nil {
		return nil, err
	}
	return &pkGetOp{scan: n.Scan, row: row, outer: outer}, nil
}

func (o *pkGetOp) Next(context.Context) (frame, bool, error) {
	if o.done || o.row == nil {
		return frame{}, false, nil
	}
	o.done = true
	return rowToFrame(o.scan, o.row, o.outer), true, nil
}

func (o *pkGetOp) Close() error { return nil }

type rowIterOp struct {
	scan  *bound.Scan
	it    RowIterator
	outer bound.Env
}

func (o *rowIterOp) Next(context.Context) (frame, bool, error) {
	row, ok, err := o.it.Next()
	if err != nil || !ok {
		return frame{}, false, err
	}
	return rowToFrame(o.scan, row, o.outer), true, nil
}

func (o *rowIterOp) Close() error { return o.it.Close() }

func (ex *executor) buildIndexScan(ctx context.Context, n *planner.IndexRangeScanExec, outer bound.Env) (operator, error) {
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
	var bounds *rangeBounds
	if n.Range != nil {
		bounds = &rangeBounds{}
		if n.Range.Lo != nil {
			value := n.Range.Lo.V
			bounds.lo, bounds.loIncl = &value, n.Range.Lo.Inclusive
		}
		if n.Range.Hi != nil {
			value := n.Range.Hi.V
			bounds.hi, bounds.hiIncl = &value, n.Range.Hi.Inclusive
		}
	}
	it, err := scanIndexRange(ctx, ex.view, n.Scan.Table, n.Index, equal, bounds)
	if err != nil {
		return nil, err
	}
	return &rowIterOp{scan: n.Scan, it: it, outer: outer}, nil
}

type emptyOp struct{}

func (emptyOp) Next(context.Context) (frame, bool, error) { return frame{}, false, nil }
func (emptyOp) Close() error                              { return nil }
