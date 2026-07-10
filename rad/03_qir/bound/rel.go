package bound

import (
	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
)

// Relation is the bound relation law. Every node precomputes its laws at
// construction; nothing is derived lazily during planning or execution.
//
// Relational closure is the point: Output()'s fields carry slots, so the
// output of any node — a projection's computed columns, an aggregate's
// folds — is addressable by later operators exactly like a scanned column.
type Relation interface {
	// Output is the row type this relation produces; every field has a slot.
	Output() qir.RowType
	// Inputs lists child relations.
	Inputs() []Relation
	// FreeSlots are slots referenced but not produced beneath this node —
	// non-empty means the relation is correlated.
	FreeSlots() SlotSet
	// Produced is every slot defined anywhere beneath this node, including
	// intermediates that Output no longer exposes.
	Produced() SlotSet
	// Card bounds how many rows this relation can produce.
	Card() qir.Cardinality
}

// laws carries the precomputed law values every node embeds.
type laws struct {
	out      qir.RowType
	free     SlotSet
	produced SlotSet
	card     qir.Cardinality
}

func (l *laws) Output() qir.RowType     { return l.out }
func (l *laws) FreeSlots() SlotSet      { return l.free }
func (l *laws) Produced() SlotSet       { return l.produced }
func (l *laws) Card() qir.Cardinality   { return l.card }

// RefineCard lets the binder tighten a node's cardinality with knowledge the
// constructor lacks — e.g. a filter whose equality conjuncts cover a unique
// key has Max 1. Bounds only ever tighten.
func (l *laws) RefineCard(c qir.Cardinality) {
	if c.Min > l.card.Min {
		l.card.Min = c.Min
	}
	if c.Max != qir.Unbounded && (l.card.Max == qir.Unbounded || c.Max < l.card.Max) {
		l.card.Max = c.Max
	}
}

// ── nodes ───────────────────────────────────────────────────────────────────

// Scan reads a table. The binder assigns one slot per column, in column
// order; Scope is the unbound label, kept for diagnostics.
type Scan struct {
	laws
	Table catalog.Table
	Scope string
}

// NewScan binds a scan: slots must parallel tbl.Columns.
func NewScan(tbl catalog.Table, scope string, slots []qir.SlotID) *Scan {
	fields := make([]qir.Field, len(tbl.Columns))
	for i, col := range tbl.Columns {
		fields[i] = qir.Field{
			Name: col.Name,
			Slot: slots[i],
			Type: qir.ScalarType(col.Type, col.Nullable),
		}
	}
	return &Scan{
		laws: laws{
			out:      qir.RowType{Fields: fields},
			produced: NewSlotSet(slots...),
			card:     qir.Cardinality{Min: 0, Max: qir.Unbounded},
		},
		Table: tbl,
		Scope: scope,
	}
}

func (s *Scan) Inputs() []Relation { return nil }

// Filter keeps rows whose predicate evaluates to TRUE under three-valued
// logic. The predicate must be Bool-typed; the binder guarantees it.
type Filter struct {
	laws
	In   Relation
	Pred Expr
}

func NewFilter(in Relation, pred Expr) *Filter {
	return &Filter{
		laws: laws{
			out:      in.Output(),
			free:     in.FreeSlots().Union(pred.FreeSlots().Without(in.Produced())),
			produced: in.Produced(),
			card:     qir.Cardinality{Min: 0, Max: in.Card().Max},
		},
		In: in, Pred: pred,
	}
}

func (f *Filter) Inputs() []Relation { return []Relation{f.In} }

// ProjField is one bound output attribute: a name, a fresh slot, and the
// expression producing it. The binder resolves spread scopes into explicit
// fields whose expressions are slot references, so bound projections have no
// spread concept.
type ProjField struct {
	Name string
	Slot qir.SlotID
	Expr Expr
}

// Project establishes a new row type.
type Project struct {
	laws
	In     Relation
	Fields []ProjField
	Scope  string // optional unbound output label, for diagnostics
}

func NewProject(in Relation, scope string, fields []ProjField) *Project {
	out := make([]qir.Field, len(fields))
	slots := make([]qir.SlotID, len(fields))
	free := in.FreeSlots()
	for i, f := range fields {
		out[i] = qir.Field{Name: f.Name, Slot: f.Slot, Type: f.Expr.Type()}
		slots[i] = f.Slot
		free = free.Union(f.Expr.FreeSlots().Without(in.Produced()))
	}
	return &Project{
		laws: laws{
			out:      qir.RowType{Fields: out},
			free:     free,
			produced: in.Produced().Union(NewSlotSet(slots...)),
			card:     in.Card(),
		},
		In: in, Fields: fields, Scope: scope,
	}
}

func (p *Project) Inputs() []Relation { return []Relation{p.In} }

// Join combines two relations on a boolean condition. Left joins make the
// right side's output nullable.
type Join struct {
	laws
	L, R Relation
	Kind qir.JoinKind
	On   Expr
}

func NewJoin(l, r Relation, kind qir.JoinKind, on Expr) *Join {
	lf, rf := l.Output().Fields, r.Output().Fields
	fields := make([]qir.Field, 0, len(lf)+len(rf))
	fields = append(fields, lf...)
	for _, f := range rf {
		if kind == qir.LeftJoin {
			f.Type.Nullable = true
		}
		fields = append(fields, f)
	}
	produced := l.Produced().Union(r.Produced())
	free := l.FreeSlots().Union(r.FreeSlots()).Union(on.FreeSlots().Without(produced))

	lc, rc := l.Card(), r.Card()
	card := qir.Cardinality{Min: 0, Max: qir.Unbounded}
	if kind == qir.LeftJoin {
		card.Min = lc.Min
	}
	if lc.Max != qir.Unbounded && rc.Max != qir.Unbounded {
		rmax := rc.Max
		if kind == qir.LeftJoin && rmax < 1 {
			rmax = 1
		}
		card.Max = lc.Max * rmax
	}
	return &Join{
		laws: laws{
			out:      qir.RowType{Fields: fields},
			free:     free,
			produced: produced,
			card:     card,
		},
		L: l, R: r, Kind: kind, On: on,
	}
}

func (j *Join) Inputs() []Relation { return []Relation{j.L, j.R} }

// GroupTerm is one bound grouping attribute.
type GroupTerm struct {
	Name string
	Slot qir.SlotID
	Expr Expr
}

// AggTerm is one bound fold. Arg is nil only for count-rows. T is the fold's
// result type, fixed by the aggregate typing rules.
type AggTerm struct {
	Fn   qir.AggFn
	Arg  Expr
	Name string
	Slot qir.SlotID
	T    qir.Type
}

// AggTermType applies the aggregate typing rules: count is int64 and never
// NULL (an empty fold counts 0); sum/min/max keep the argument's type but
// are NULL over an empty or all-NULL input; avg is always float64, likewise
// nullable.
func AggTermType(fn qir.AggFn, arg Expr) qir.Type {
	switch fn {
	case qir.AggCount:
		return qir.Type{Kind: qir.KindInt64}
	case qir.AggAvg:
		return qir.Type{Kind: qir.KindFloat64, Nullable: true}
	default: // sum, min, max
		t := arg.Type()
		t.Nullable = true
		return t
	}
}

// Aggregate folds its input: one row per distinct group, or exactly one row
// with no groups.
type Aggregate struct {
	laws
	In     Relation
	Groups []GroupTerm
	Terms  []AggTerm
}

func NewAggregate(in Relation, groups []GroupTerm, terms []AggTerm) *Aggregate {
	fields := make([]qir.Field, 0, len(groups)+len(terms))
	slots := make([]qir.SlotID, 0, len(groups)+len(terms))
	free := in.FreeSlots()
	for _, g := range groups {
		fields = append(fields, qir.Field{Name: g.Name, Slot: g.Slot, Type: g.Expr.Type()})
		slots = append(slots, g.Slot)
		free = free.Union(g.Expr.FreeSlots().Without(in.Produced()))
	}
	for _, t := range terms {
		fields = append(fields, qir.Field{Name: t.Name, Slot: t.Slot, Type: t.T})
		slots = append(slots, t.Slot)
		if t.Arg != nil {
			free = free.Union(t.Arg.FreeSlots().Without(in.Produced()))
		}
	}
	card := qir.Cardinality{Min: 1, Max: 1} // global fold
	if len(groups) > 0 {
		card = qir.Cardinality{Min: 0, Max: in.Card().Max}
	}
	return &Aggregate{
		laws: laws{
			out:      qir.RowType{Fields: fields},
			free:     free,
			produced: in.Produced().Union(NewSlotSet(slots...)),
			card:     card,
		},
		In: in, Groups: groups, Terms: terms,
	}
}

func (a *Aggregate) Inputs() []Relation { return []Relation{a.In} }

// OrderTerm is one bound ordering term.
type OrderTerm struct {
	Expr Expr
	Desc bool
}

// Order gives the relation a logical ordering. The binder appends a
// deterministic tie-breaker (a known unique key of the output) when one
// exists, so ties are path-independent.
type Order struct {
	laws
	In    Relation
	Terms []OrderTerm
}

func NewOrder(in Relation, terms []OrderTerm) *Order {
	free := in.FreeSlots()
	for _, t := range terms {
		free = free.Union(t.Expr.FreeSlots().Without(in.Produced()))
	}
	return &Order{
		laws: laws{
			out:      in.Output(),
			free:     free,
			produced: in.Produced(),
			card:     in.Card(),
		},
		In: in, Terms: terms,
	}
}

func (o *Order) Inputs() []Relation { return []Relation{o.In} }

// Slice keeps Limit rows after skipping Offset; nil Limit is unlimited.
type Slice struct {
	laws
	In     Relation
	Offset int
	Limit  *int
}

func NewSlice(in Relation, offset int, limit *int) *Slice {
	card := in.Card()
	card.Min = 0
	if limit != nil && (card.Max == qir.Unbounded || *limit < card.Max) {
		card.Max = *limit
	}
	return &Slice{
		laws: laws{
			out:      in.Output(),
			free:     in.FreeSlots(),
			produced: in.Produced(),
			card:     card,
		},
		In: in, Offset: offset, Limit: limit,
	}
}

func (s *Slice) Inputs() []Relation { return []Relation{s.In} }

// Query is a bound root relation plus its materialisation cardinality.
type Query struct {
	Root Relation
	Card qir.RootCard
}

// Ordered reports whether rel carries an explicit logical ordering: an Order
// node at its root, seen through the order-preserving operators (Filter,
// Project, Slice). Scans, joins, and aggregates provide no logical order —
// their encounter order is physical and never observable.
func Ordered(rel Relation) bool {
	switch n := rel.(type) {
	case *Order:
		return true
	case *Filter:
		return Ordered(n.In)
	case *Project:
		return Ordered(n.In)
	case *Slice:
		return Ordered(n.In)
	}
	return false
}
