package bound

import (
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

type Scan struct {
	laws
	Table model.Table
	Scope string
}

func NewScan(table model.Table, scope string, slots []lir.SlotID) *Scan {
	fields := make([]lir.Field, len(table.Columns))
	for i, column := range table.Columns {
		fields[i] = lir.Field{
			Name: column.Name,
			Slot: slots[i],
			Type: lir.ScalarType(column.Type, column.Nullable),
		}
	}
	return &Scan{
		laws: laws{
			out:      lir.RowType{Fields: fields},
			produced: NewSlotSet(slots...),
			card:     lir.Cardinality{Min: 0, Max: lir.Unbounded},
		},
		Table: table,
		Scope: scope,
	}
}

func (s *Scan) Inputs() []Relation { return nil }

type Rows struct {
	laws
	Scope string
	Vals  [][]lir.Value
}

func NewRows(scope string, fields []lir.Field, values [][]lir.Value) *Rows {
	out := lir.RowType{Fields: fields}
	count := len(values)
	return &Rows{
		laws: laws{
			out:      out,
			produced: NewSlotSet(out.Slots()...),
			card:     lir.Cardinality{Min: count, Max: count},
		},
		Scope: scope,
		Vals:  values,
	}
}

func (r *Rows) Inputs() []Relation { return nil }
