package query

import (
	"context"
	"math/big"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
	"github.com/Southclaws/rad/rad/engine/reject"
)

type aggOp struct {
	in     operator
	groups []bound.GroupTerm
	terms  []bound.AggTerm
	outer  lireval.Env
	out    []lireval.Env
	pos    int
	primed bool
}

type aggAccum struct {
	count int64
	sumI  big.Int
	sumF  float64
	n     int64
	min   lir.Value
	max   lir.Value
}

func (o *aggOp) Next(ctx context.Context) (lireval.Env, bool, error) {
	if !o.primed {
		if err := o.fold(ctx); err != nil {
			return lireval.Env{}, false, err
		}
		o.primed = true
	}
	if o.pos >= len(o.out) {
		return lireval.Env{}, false, nil
	}
	result := o.out[o.pos]
	o.pos++
	return result, true, nil
}

func (o *aggOp) fold(ctx context.Context) error {
	type group struct {
		values []lir.Value
		accum  []aggAccum
	}
	groups := map[string]*group{}
	var order []string

	for {
		currentFrame, ok, err := o.in.Next(ctx)
		if err != nil {
			return err
		}
		if !ok {
			break
		}
		groupValues := make([]lir.Value, len(o.groups))
		for i, term := range o.groups {
			value, err := lireval.Eval(term.Expr, currentFrame)
			if err != nil {
				return err
			}
			groupValues[i] = value
		}
		key, err := codec.EncodeTuple(groupValues)
		if err != nil {
			return err
		}
		groupKey := string(key)
		current, ok := groups[groupKey]
		if !ok {
			current = &group{values: groupValues, accum: make([]aggAccum, len(o.terms))}
			groups[groupKey] = current
			order = append(order, groupKey)
		}
		for i, term := range o.terms {
			accum := &current.accum[i]
			if term.Arg == nil {
				accum.count++
				continue
			}
			value, err := lireval.Eval(term.Arg, currentFrame)
			if err != nil {
				return err
			}
			if value.Null {
				continue
			}
			accum.count++
			switch term.Fn {
			case lir.AggSum, lir.AggAvg:
				accum.n++
				if value.Type == "int64" {
					accum.sumI.Add(&accum.sumI, big.NewInt(value.Int64))
					accum.sumF += float64(value.Int64)
				} else {
					accum.sumF += value.Float64
				}
			case lir.AggMin, lir.AggMax:
				if accum.n == 0 {
					accum.min, accum.max = value, value
				} else {
					if comparison, _ := value.Compare(accum.min); comparison < 0 {
						accum.min = value
					}
					if comparison, _ := value.Compare(accum.max); comparison > 0 {
						accum.max = value
					}
				}
				accum.n++
			}
		}
	}

	// A global aggregate produces one row even when its input is empty.
	if len(o.groups) == 0 && len(order) == 0 {
		groups[""] = &group{accum: make([]aggAccum, len(o.terms))}
		order = append(order, "")
	}

	for _, key := range order {
		current := groups[key]
		currentFrame := newFrame(o.outer)
		for i, term := range o.groups {
			currentFrame.SetScalar(term.Slot, current.values[i])
		}
		for i, term := range o.terms {
			value, err := foldResult(term, &current.accum[i])
			if err != nil {
				return err
			}
			currentFrame.SetScalar(term.Slot, value)
		}
		o.out = append(o.out, currentFrame)
	}
	return nil
}

// Integer sums use an exact accumulator so overflow does not depend on input
// order. Every aggregate except count returns NULL for an empty input.
func foldResult(term bound.AggTerm, accum *aggAccum) (lir.Value, error) {
	switch term.Fn {
	case lir.AggCount:
		return lir.Int64(accum.count), nil
	case lir.AggSum:
		if accum.n == 0 {
			return lir.Null(term.T.Kind.CatalogType()), nil
		}
		if term.T.Kind == lir.KindInt64 {
			if !accum.sumI.IsInt64() {
				return lir.Value{}, reject.Runtimef("exec: integer overflow in sum")
			}
			return lir.Int64(accum.sumI.Int64()), nil
		}
		return lir.Float64(accum.sumF), nil
	case lir.AggAvg:
		if accum.n == 0 {
			return lir.Null(term.T.Kind.CatalogType()), nil
		}
		return lir.Float64(accum.sumF / float64(accum.n)), nil
	case lir.AggMin:
		if accum.n == 0 {
			return lir.Null(term.T.Kind.CatalogType()), nil
		}
		return accum.min, nil
	default:
		if accum.n == 0 {
			return lir.Null(term.T.Kind.CatalogType()), nil
		}
		return accum.max, nil
	}
}

func (o *aggOp) Close() error { return o.in.Close() }
