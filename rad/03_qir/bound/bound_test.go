package bound

import (
	"testing"

	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
)

// tasksTable is a hand-built catalog table for law tests: no store needed.
func tasksTable() catalog.Table {
	return catalog.Table{
		ID:   "t1",
		Name: "tasks",
		Columns: []catalog.Column{
			{ID: "c1", Name: "id", Type: catalog.TypeText},
			{ID: "c2", Name: "board_id", Type: catalog.TypeText},
			{ID: "c3", Name: "status", Type: catalog.TypeText},
			{ID: "c4", Name: "priority", Type: catalog.TypeInt64},
			{ID: "c5", Name: "assignee_id", Type: catalog.TypeText, Nullable: true},
		},
		PrimaryKey: []string{"id"},
	}
}

func scanTasks() *Scan {
	return NewScan(tasksTable(), "t", []qir.SlotID{0, 1, 2, 3, 4})
}

func slot(s *Scan, name string) SlotRef {
	f, ok := s.Output().Lookup(name)
	if !ok {
		panic("no field " + name)
	}
	return SlotRef{Slot: f.Slot, Name: name, T: f.Type}
}

func TestScanLaws(t *testing.T) {
	s := scanTasks()
	out := s.Output()
	if len(out.Fields) != 5 || out.Fields[3].Name != "priority" || out.Fields[3].Slot != 3 {
		t.Fatalf("scan output = %+v", out)
	}
	if out.Fields[4].Type.Nullable != true || out.Fields[0].Type.Nullable {
		t.Fatal("nullability not carried from catalog")
	}
	if !s.FreeSlots().Empty() {
		t.Fatal("scan has free slots")
	}
	if s.Card() != (qir.Cardinality{Min: 0, Max: qir.Unbounded}) {
		t.Fatalf("scan card = %v", s.Card())
	}
	if got := s.Produced().Slots(); len(got) != 5 {
		t.Fatalf("produced = %v", got)
	}
}

func TestFilterLawsAndRefinement(t *testing.T) {
	s := scanTasks()
	pred := NewBinary(qir.OpEq, slot(s, "status"), Literal{V: qir.Text("open")})
	f := NewFilter(s, pred)

	if !f.FreeSlots().Empty() {
		t.Fatalf("uncorrelated filter has free slots: %v", f.FreeSlots().Slots())
	}
	if f.Card().Max != qir.Unbounded {
		t.Fatalf("filter card = %v", f.Card())
	}
	// The binder tightens with uniqueness knowledge.
	f.RefineCard(qir.Cardinality{Min: 0, Max: 1})
	if !f.Card().AtMostOne() {
		t.Fatalf("refined card = %v", f.Card())
	}
	// Refinement never loosens.
	f.RefineCard(qir.Cardinality{Min: 0, Max: 100})
	if f.Card().Max != 1 {
		t.Fatalf("card loosened to %v", f.Card())
	}
}

func TestCorrelationIsFreeSlots(t *testing.T) {
	s := scanTasks()
	outer := SlotRef{Slot: 100, Name: "b.id", T: qir.Type{Kind: qir.KindText}}
	pred := NewBinary(qir.OpEq, slot(s, "board_id"), outer)
	f := NewFilter(s, pred)

	free := f.FreeSlots()
	if free.Empty() || !free.Contains(100) || free.Contains(1) {
		t.Fatalf("free slots = %v", free.Slots())
	}
	// A crossing's free slots are its relation's.
	if !NewArray(f).FreeSlots().Contains(100) {
		t.Fatal("crossing dropped free slots")
	}
}

func TestProjectLaws(t *testing.T) {
	s := scanTasks()
	doubled := NewBinary(qir.OpMul, slot(s, "priority"), Literal{V: qir.Int64(2)})
	p := NewProject(s, "shaped", []ProjField{
		{Name: "id", Slot: 10, Expr: slot(s, "id")},
		{Name: "score", Slot: 11, Expr: doubled},
	})

	out := p.Output()
	if len(out.Fields) != 2 || out.Fields[1].Name != "score" || out.Fields[1].Slot != 11 {
		t.Fatalf("project output = %+v", out)
	}
	if out.Fields[1].Type.Kind != qir.KindInt64 {
		t.Fatalf("score type = %v", out.Fields[1].Type)
	}
	// Closure: the projected slot is addressable above.
	above := NewFilter(p, NewBinary(qir.OpGt,
		SlotRef{Slot: 11, Name: "score", T: out.Fields[1].Type},
		Literal{V: qir.Int64(10)}))
	if !above.FreeSlots().Empty() {
		t.Fatalf("score not produced beneath: free = %v", above.FreeSlots().Slots())
	}
	if !p.Produced().Contains(11) || !p.Produced().Contains(3) {
		t.Fatal("produced must include new and intermediate slots")
	}
}

func TestAggregateLaws(t *testing.T) {
	s := scanTasks()

	global := NewAggregate(s, nil, []AggTerm{
		{Fn: qir.AggCount, Name: "n", Slot: 20, T: AggTermType(qir.AggCount, nil)},
		{Fn: qir.AggAvg, Arg: slot(s, "priority"), Name: "avg_p", Slot: 21, T: AggTermType(qir.AggAvg, slot(s, "priority"))},
		{Fn: qir.AggSum, Arg: slot(s, "priority"), Name: "sum_p", Slot: 22, T: AggTermType(qir.AggSum, slot(s, "priority"))},
	})
	if global.Card() != (qir.Cardinality{Min: 1, Max: 1}) {
		t.Fatalf("global fold card = %v", global.Card())
	}
	out := global.Output()
	if out.Fields[0].Type != (qir.Type{Kind: qir.KindInt64}) {
		t.Fatalf("count type = %v (must be non-nullable int64)", out.Fields[0].Type)
	}
	if out.Fields[1].Type != (qir.Type{Kind: qir.KindFloat64, Nullable: true}) {
		t.Fatalf("avg type = %v", out.Fields[1].Type)
	}
	if out.Fields[2].Type != (qir.Type{Kind: qir.KindInt64, Nullable: true}) {
		t.Fatalf("sum type = %v", out.Fields[2].Type)
	}

	grouped := NewAggregate(s,
		[]GroupTerm{{Name: "status", Slot: 30, Expr: slot(s, "status")}},
		[]AggTerm{{Fn: qir.AggCount, Name: "n", Slot: 31, T: AggTermType(qir.AggCount, nil)}})
	if grouped.Card().Max != qir.Unbounded || grouped.Card().Min != 0 {
		t.Fatalf("grouped card = %v", grouped.Card())
	}
	// Closure: group and term slots addressable above.
	above := NewFilter(grouped, NewBinary(qir.OpGt,
		SlotRef{Slot: 31, Name: "n", T: qir.Type{Kind: qir.KindInt64}},
		Literal{V: qir.Int64(10)}))
	if !above.FreeSlots().Empty() {
		t.Fatal("aggregate outputs not addressable")
	}
}

func TestJoinLaws(t *testing.T) {
	l := scanTasks()
	r := NewScan(catalog.Table{
		ID: "t2", Name: "users",
		Columns:    []catalog.Column{{ID: "c6", Name: "id", Type: catalog.TypeText}},
		PrimaryKey: []string{"id"},
	}, "u", []qir.SlotID{50})

	on := NewBinary(qir.OpEq, slot(l, "assignee_id"), SlotRef{Slot: 50, Name: "u.id", T: qir.Type{Kind: qir.KindText}})
	inner := NewJoin(l, r, qir.InnerJoin, on)
	if got := len(inner.Output().Fields); got != 6 {
		t.Fatalf("join output arity = %d", got)
	}
	if inner.Output().Fields[5].Type.Nullable {
		t.Fatal("inner join must not null-pad")
	}
	left := NewJoin(l, r, qir.LeftJoin, on)
	if !left.Output().Fields[5].Type.Nullable {
		t.Fatal("left join right side must be nullable")
	}
	if !inner.FreeSlots().Empty() {
		t.Fatal("join with internal on-slots is not correlated")
	}
}

func TestSliceCardAndOrdered(t *testing.T) {
	s := scanTasks()
	lim := 5
	sl := NewSlice(s, 0, &lim)
	if sl.Card().Max != 5 {
		t.Fatalf("slice card = %v", sl.Card())
	}
	one := 1
	if !NewSlice(s, 0, &one).Card().AtMostOne() {
		t.Fatal("slice 1 must be at-most-one")
	}
	if NewSlice(s, 3, nil).Card().Max != qir.Unbounded {
		t.Fatal("nil limit must stay unbounded")
	}

	ord := NewOrder(s, []OrderTerm{{Expr: slot(s, "priority"), Desc: true}})
	if !Ordered(NewSlice(NewFilter(ord, Literal{V: qir.Bool(true)}), 0, &lim)) {
		t.Fatal("ordering must survive filter+slice")
	}
	if Ordered(s) || Ordered(NewAggregate(s, nil, nil)) {
		t.Fatal("scan/aggregate must not claim logical order")
	}
}

func TestScalarCrossingArity(t *testing.T) {
	s := scanTasks()
	if _, err := NewScalar(s); err == nil {
		t.Fatal("scalar over 5-column relation accepted")
	}
	agg := NewAggregate(s, nil, []AggTerm{{Fn: qir.AggCount, Name: "n", Slot: 20, T: AggTermType(qir.AggCount, nil)}})
	sc, err := NewScalar(agg)
	if err != nil {
		t.Fatal(err)
	}
	if sc.Type() != (qir.Type{Kind: qir.KindInt64, Nullable: true}) {
		t.Fatalf("scalar type = %v (crossing must be nullable: no row = NULL)", sc.Type())
	}
}

func TestSlotSetWordBoundaries(t *testing.T) {
	s := NewSlotSet(0, 63, 64, 130)
	for _, id := range []qir.SlotID{0, 63, 64, 130} {
		if !s.Contains(id) {
			t.Fatalf("missing %d", id)
		}
	}
	if s.Contains(1) || s.Contains(129) {
		t.Fatal("phantom members")
	}
	diff := s.Without(NewSlotSet(64))
	if diff.Contains(64) || !diff.Contains(130) {
		t.Fatalf("without = %v", diff.Slots())
	}
	if got := s.Union(NewSlotSet(1)).Slots(); len(got) != 5 {
		t.Fatalf("union = %v", got)
	}
	if !NewSlotSet().Empty() || s.Empty() {
		t.Fatal("emptiness broken")
	}
}
