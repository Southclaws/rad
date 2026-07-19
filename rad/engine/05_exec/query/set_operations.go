package query

// The binary bag set operations. Both block on the right side: it is drained
// into a canonical-identity count map, then the left side streams through it,
// and surviving rows are remapped onto the operator's fresh output slots.
// Identity keys are value-based (lireval.CanonicalRowKey), so a left row
// probes the right multiset even though the two sides carry different slots.

import (
	"context"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	lireval "github.com/Southclaws/rad/rad/engine/03_lir/eval"
)

type setOperationOp struct {
	l, r       operator
	subtract   bool // except; false is intersect
	distinct   bool // the distinct quantifier
	lOut, rOut lir.RowType
	out        lir.RowType
	outer      lireval.Env

	right   map[string]int  // remaining right occurrences by identity
	emitted map[string]bool // identities already emitted (distinct quantifier)
	primed  bool
}

func (o *setOperationOp) Next(ctx context.Context) (lireval.Env, bool, error) {
	if !o.primed {
		rows, err := drainOp(ctx, o.r)
		if err != nil {
			return lireval.Env{}, false, err
		}
		o.right = make(map[string]int, len(rows))
		for _, row := range rows {
			o.right[lireval.CanonicalRowKey(o.rOut.Fields, row)]++
		}
		if o.distinct {
			o.emitted = map[string]bool{}
		}
		o.primed = true
	}
	for {
		src, ok, err := o.l.Next(ctx)
		if err != nil || !ok {
			return lireval.Env{}, false, err
		}
		key := lireval.CanonicalRowKey(o.lOut.Fields, src)
		if o.keep(key) {
			return remapPositional(o.out, o.lOut, src, o.outer), true, nil
		}
	}
}

// keep decides one left occurrence's fate and updates the consumption state.
func (o *setOperationOp) keep(key string) bool {
	if o.distinct {
		if o.emitted[key] {
			return false
		}
		inRight := o.right[key] > 0
		if o.subtract == inRight {
			return false
		}
		o.emitted[key] = true
		return true
	}
	if o.right[key] > 0 {
		o.right[key]--
		return !o.subtract // a matched occurrence: kept by intersect, cancelled by except
	}
	return o.subtract // an unmatched occurrence: kept by except, dropped by intersect
}

func (o *setOperationOp) Close() error {
	err := o.l.Close()
	if rightErr := o.r.Close(); err == nil {
		err = rightErr
	}
	return err
}
