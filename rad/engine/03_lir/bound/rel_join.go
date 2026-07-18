package bound

import lir "github.com/Southclaws/rad/rad/engine/03_lir"

// Join combines two relations. Left joins make right-side outputs nullable.
type Join struct {
	laws
	L, R Relation
	Kind lir.JoinKind
	On   Expr
}

func NewJoin(left, right Relation, kind lir.JoinKind, on Expr) *Join {
	leftFields, rightFields := left.Output().Fields, right.Output().Fields
	fields := make([]lir.Field, 0, len(leftFields)+len(rightFields))
	fields = append(fields, leftFields...)
	for _, field := range rightFields {
		if kind == lir.LeftJoin {
			field.Type.Nullable = true
		}
		fields = append(fields, field)
	}
	produced := left.Produced().Union(right.Produced())
	free := left.FreeSlots().Union(right.FreeSlots()).Union(on.FreeSlots().Without(produced))

	leftCard, rightCard := left.Card(), right.Card()
	card := lir.Cardinality{Min: 0, Max: lir.Unbounded}
	if kind == lir.LeftJoin {
		card.Min = leftCard.Min
	}
	if leftCard.Max != lir.Unbounded && rightCard.Max != lir.Unbounded {
		rightMax := rightCard.Max
		if kind == lir.LeftJoin && rightMax < 1 {
			rightMax = 1
		}
		card.Max = leftCard.Max * rightMax
	}
	return &Join{
		laws: laws{
			out:      lir.RowType{Fields: fields},
			free:     free,
			produced: produced,
			card:     card,
		},
		L: left, R: right, Kind: kind, On: on,
	}
}

func (j *Join) Inputs() []Relation { return []Relation{j.L, j.R} }
