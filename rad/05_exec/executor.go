package exec

import (
	"context"
	"fmt"
	"maps"

	kv "rad/rad/01_kv"
	lir "rad/rad/03_lir"
	planner "rad/rad/04_planner"
)

// Executor produces rows one at a time. Next returns ok=false when exhausted.
type Executor interface {
	Next(ctx context.Context) (lir.Row, bool, error)
	Close() error
}

// Run executes a physical plan against a KV view (the committed store, or a
// transaction snapshot) and returns all result rows.
func Run(ctx context.Context, view kv.KV, plan planner.Node) ([]lir.Row, error) {
	exec, err := Build(ctx, view, plan)
	if err != nil {
		return nil, err
	}
	return drain(ctx, exec)
}

// Build turns a physical plan node into an executor tree.
func Build(ctx context.Context, view kv.KV, node planner.Node) (Executor, error) {
	switch n := node.(type) {
	case planner.TableScan:
		it, err := scanTable(ctx, view, n.Table)
		if err != nil {
			return nil, err
		}
		return &tableScanExec{it: it, alias: n.Alias}, nil

	case planner.IndexScan:
		// Materializes the matching rows up front; fine for a POC.
		rows, err := scanIndex(ctx, view, n.Table, n.Index, n.Prefix)
		if err != nil {
			return nil, err
		}
		qualified := make([]lir.Row, len(rows))
		for i, r := range rows {
			qualified[i] = qualify(n.Alias, r)
		}
		return &sliceExec{rows: qualified}, nil

	case planner.Filter:
		in, err := Build(ctx, view, n.Input)
		if err != nil {
			return nil, err
		}
		return &filterExec{input: in, expr: n.Expr}, nil

	case planner.Project:
		in, err := Build(ctx, view, n.Input)
		if err != nil {
			return nil, err
		}
		return &projectExec{input: in, columns: n.Columns}, nil

	case planner.NestedLoopJoin:
		left, err := Build(ctx, view, n.Left)
		if err != nil {
			return nil, err
		}
		right, err := Build(ctx, view, n.Right)
		if err != nil {
			left.Close()
			return nil, err
		}
		rightRows, err := drain(ctx, right)
		if err != nil {
			left.Close()
			return nil, err
		}
		return &joinExec{left: left, rightRows: rightRows, on: n.On}, nil

	case planner.Limit:
		in, err := Build(ctx, view, n.Input)
		if err != nil {
			return nil, err
		}
		return &limitExec{input: in, n: n.N}, nil

	default:
		return nil, fmt.Errorf("exec: unsupported plan node %T", node)
	}
}

func drain(ctx context.Context, exec Executor) ([]lir.Row, error) {
	defer exec.Close()
	var rows []lir.Row
	for {
		row, ok, err := exec.Next(ctx)
		if err != nil {
			return nil, err
		}
		if !ok {
			return rows, nil
		}
		rows = append(rows, row)
	}
}

// qualify renames plain column names to alias.column.
func qualify(alias string, row lir.Row) lir.Row {
	out := make(lir.Row, len(row))
	for name, v := range row {
		out[alias+"."+name] = v
	}
	return out
}

// evalValue evaluates an expression to a value (for projections).
func evalValue(e lir.Expr, row lir.Row) (lir.Value, error) {
	switch x := e.(type) {
	case lir.ColRef:
		v, ok := row[x.Alias+"."+x.Column]
		if !ok {
			return lir.Value{}, fmt.Errorf("exec: unknown column %s.%s", x.Alias, x.Column)
		}
		return v, nil
	case lir.Literal:
		return x.Value, nil
	default:
		return lir.Value{}, fmt.Errorf("exec: expression %T does not produce a value", e)
	}
}

// evalPredicate evaluates a boolean expression. Comparisons involving NULL
// are false.
func evalPredicate(e lir.Expr, row lir.Row) (bool, error) {
	switch x := e.(type) {
	case lir.Eq:
		l, err := evalValue(x.Left, row)
		if err != nil {
			return false, err
		}
		r, err := evalValue(x.Right, row)
		if err != nil {
			return false, err
		}
		return l.Equal(r), nil
	case lir.And:
		for _, sub := range x.Exprs {
			ok, err := evalPredicate(sub, row)
			if err != nil || !ok {
				return false, err
			}
		}
		return true, nil
	default:
		return false, fmt.Errorf("exec: expression %T is not a predicate", e)
	}
}

type tableScanExec struct {
	it    RowIterator
	alias string
}

func (s *tableScanExec) Next(ctx context.Context) (lir.Row, bool, error) {
	if err := ctx.Err(); err != nil {
		return nil, false, err
	}
	row, ok, err := s.it.Next()
	if err != nil || !ok {
		return nil, false, err
	}
	return qualify(s.alias, row), true, nil
}

func (s *tableScanExec) Close() error { return s.it.Close() }

type sliceExec struct {
	rows []lir.Row
	pos  int
}

func (s *sliceExec) Next(ctx context.Context) (lir.Row, bool, error) {
	if err := ctx.Err(); err != nil {
		return nil, false, err
	}
	if s.pos >= len(s.rows) {
		return nil, false, nil
	}
	row := s.rows[s.pos]
	s.pos++
	return row, true, nil
}

func (s *sliceExec) Close() error { return nil }

type filterExec struct {
	input Executor
	expr  lir.Expr
}

func (f *filterExec) Next(ctx context.Context) (lir.Row, bool, error) {
	for {
		row, ok, err := f.input.Next(ctx)
		if err != nil || !ok {
			return nil, false, err
		}
		match, err := evalPredicate(f.expr, row)
		if err != nil {
			return nil, false, err
		}
		if match {
			return row, true, nil
		}
	}
}

func (f *filterExec) Close() error { return f.input.Close() }

type projectExec struct {
	input   Executor
	columns []lir.Projection
}

func (p *projectExec) Next(ctx context.Context) (lir.Row, bool, error) {
	row, ok, err := p.input.Next(ctx)
	if err != nil || !ok {
		return nil, false, err
	}
	out := make(lir.Row, len(p.columns))
	for _, proj := range p.columns {
		v, err := evalValue(proj.Expr, row)
		if err != nil {
			return nil, false, err
		}
		out[proj.As] = v
	}
	return out, true, nil
}

func (p *projectExec) Close() error { return p.input.Close() }

type joinExec struct {
	left      Executor
	rightRows []lir.Row
	on        lir.Expr

	leftRow  lir.Row
	rightPos int
}

func (j *joinExec) Next(ctx context.Context) (lir.Row, bool, error) {
	for {
		if err := ctx.Err(); err != nil {
			return nil, false, err
		}
		if j.leftRow == nil {
			row, ok, err := j.left.Next(ctx)
			if err != nil || !ok {
				return nil, false, err
			}
			j.leftRow = row
			j.rightPos = 0
		}
		for j.rightPos < len(j.rightRows) {
			right := j.rightRows[j.rightPos]
			j.rightPos++

			merged := make(lir.Row, len(j.leftRow)+len(right))
			maps.Copy(merged, j.leftRow)
			maps.Copy(merged, right)
			match := true
			if j.on != nil {
				var err error
				match, err = evalPredicate(j.on, merged)
				if err != nil {
					return nil, false, err
				}
			}
			if match {
				return merged, true, nil
			}
		}
		j.leftRow = nil
	}
}

func (j *joinExec) Close() error { return j.left.Close() }

type limitExec struct {
	input Executor
	n     int
	seen  int
}

func (l *limitExec) Next(ctx context.Context) (lir.Row, bool, error) {
	if l.seen >= l.n {
		return nil, false, nil
	}
	row, ok, err := l.input.Next(ctx)
	if err != nil || !ok {
		return nil, false, err
	}
	l.seen++
	return row, true, nil
}

func (l *limitExec) Close() error { return l.input.Close() }
