package bound

import (
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// Relation is the bound relation law. Every node precomputes its laws at
// construction; nothing is derived lazily during planning or execution.
//
// Relational closure is the point: Output()'s fields carry slots, so the
// output of any node — a projection's computed columns, an aggregate's
// folds — is addressable by later operators exactly like a scanned column.
type Relation interface {
	// Output is the row type this relation produces; every field has a slot.
	Output() lir.RowType
	// Inputs lists child relations.
	Inputs() []Relation
	// FreeSlots are slots referenced but not produced beneath this node —
	// non-empty means the relation is correlated.
	FreeSlots() SlotSet
	// Produced is every slot defined anywhere beneath this node, including
	// intermediates that Output no longer exposes.
	Produced() SlotSet
	// Card bounds how many rows this relation can produce.
	Card() lir.Cardinality
}

// laws carries the precomputed law values every node embeds.
type laws struct {
	out      lir.RowType
	free     SlotSet
	produced SlotSet
	card     lir.Cardinality
}

func (l *laws) Output() lir.RowType   { return l.out }
func (l *laws) FreeSlots() SlotSet    { return l.free }
func (l *laws) Produced() SlotSet     { return l.produced }
func (l *laws) Card() lir.Cardinality { return l.card }

// RefineCard lets the binder tighten a node's cardinality with knowledge the
// constructor lacks — e.g. a filter whose equality conjuncts cover a unique
// key has Max 1. Bounds only ever tighten.
func (l *laws) RefineCard(c lir.Cardinality) {
	if c.Min > l.card.Min {
		l.card.Min = c.Min
	}
	if c.Max != lir.Unbounded && (l.card.Max == lir.Unbounded || c.Max < l.card.Max) {
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
func NewScan(tbl catalog.Table, scope string, slots []lir.SlotID) *Scan {
	fields := make([]lir.Field, len(tbl.Columns))
	for i, col := range tbl.Columns {
		fields[i] = lir.Field{
			Name: col.Name,
			Slot: slots[i],
			Type: lir.ScalarType(col.Type, col.Nullable),
		}
	}
	return &Scan{
		laws: laws{
			out:      lir.RowType{Fields: fields},
			produced: NewSlotSet(slots...),
			card:     lir.Cardinality{Min: 0, Max: lir.Unbounded},
		},
		Table: tbl,
		Scope: scope,
	}
}

func (s *Scan) Inputs() []Relation { return nil }

// Rows is a bound constant relation: coerced literal rows under fresh
// slots. The second leaf beside Scan — deterministic by construction, never
// plan-choice-sensitive, cardinality exactly len(Vals).
type Rows struct {
	laws
	Scope string
	// Vals holds the coerced cell values, row-major, parallel to
	// Output().Fields.
	Vals [][]lir.Value
}

// NewRows binds a constant relation: fields carry the declared column
// types (nullability already derived by the binder) and fresh slots; vals
// are fully coerced.
func NewRows(scope string, fields []lir.Field, vals [][]lir.Value) *Rows {
	slots := make([]lir.SlotID, len(fields))
	for i, f := range fields {
		slots[i] = f.Slot
	}
	n := len(vals)
	return &Rows{
		laws: laws{
			out:      lir.RowType{Fields: fields},
			produced: NewSlotSet(slots...),
			card:     lir.Cardinality{Min: n, Max: n},
		},
		Scope: scope,
		Vals:  vals,
	}
}

func (r *Rows) Inputs() []Relation { return nil }

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
			card:     lir.Cardinality{Min: 0, Max: in.Card().Max},
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
	Slot lir.SlotID
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
	out := make([]lir.Field, len(fields))
	slots := make([]lir.SlotID, len(fields))
	free := in.FreeSlots()
	for i, f := range fields {
		out[i] = lir.Field{Name: f.Name, Slot: f.Slot, Type: f.Expr.Type()}
		slots[i] = f.Slot
		free = free.Union(f.Expr.FreeSlots().Without(in.Produced()))
	}
	return &Project{
		laws: laws{
			out:      lir.RowType{Fields: out},
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
	Kind lir.JoinKind
	On   Expr
}

func NewJoin(l, r Relation, kind lir.JoinKind, on Expr) *Join {
	lf, rf := l.Output().Fields, r.Output().Fields
	fields := make([]lir.Field, 0, len(lf)+len(rf))
	fields = append(fields, lf...)
	for _, f := range rf {
		if kind == lir.LeftJoin {
			f.Type.Nullable = true
		}
		fields = append(fields, f)
	}
	produced := l.Produced().Union(r.Produced())
	free := l.FreeSlots().Union(r.FreeSlots()).Union(on.FreeSlots().Without(produced))

	lc, rc := l.Card(), r.Card()
	card := lir.Cardinality{Min: 0, Max: lir.Unbounded}
	if kind == lir.LeftJoin {
		card.Min = lc.Min
	}
	if lc.Max != lir.Unbounded && rc.Max != lir.Unbounded {
		rmax := rc.Max
		if kind == lir.LeftJoin && rmax < 1 {
			rmax = 1
		}
		card.Max = lc.Max * rmax
	}
	return &Join{
		laws: laws{
			out:      lir.RowType{Fields: fields},
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
	Slot lir.SlotID
	Expr Expr
}

// AggTerm is one bound fold. Arg is nil only for count-rows. T is the fold's
// result type, fixed by the aggregate typing rules.
type AggTerm struct {
	Fn   lir.AggFn
	Arg  Expr
	Name string
	Slot lir.SlotID
	T    lir.Type
}

// AggTermType applies the aggregate typing rules: count is int64 and never
// NULL (an empty fold counts 0); sum/min/max keep the argument's type but
// are NULL over an empty or all-NULL input; avg is always float64, likewise
// nullable.
func AggTermType(fn lir.AggFn, arg Expr) lir.Type {
	switch fn {
	case lir.AggCount:
		return lir.Type{Kind: lir.KindInt64}
	case lir.AggAvg:
		return lir.Type{Kind: lir.KindFloat64, Nullable: true}
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
	fields := make([]lir.Field, 0, len(groups)+len(terms))
	slots := make([]lir.SlotID, 0, len(groups)+len(terms))
	free := in.FreeSlots()
	for _, g := range groups {
		fields = append(fields, lir.Field{Name: g.Name, Slot: g.Slot, Type: g.Expr.Type()})
		slots = append(slots, g.Slot)
		free = free.Union(g.Expr.FreeSlots().Without(in.Produced()))
	}
	for _, t := range terms {
		fields = append(fields, lir.Field{Name: t.Name, Slot: t.Slot, Type: t.T})
		slots = append(slots, t.Slot)
		if t.Arg != nil {
			free = free.Union(t.Arg.FreeSlots().Without(in.Produced()))
		}
	}
	card := lir.Cardinality{Min: 1, Max: 1} // global fold
	if len(groups) > 0 {
		card = lir.Cardinality{Min: 0, Max: in.Card().Max}
	}
	return &Aggregate{
		laws: laws{
			out:      lir.RowType{Fields: fields},
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
	if limit != nil && (card.Max == lir.Unbounded || *limit < card.Max) {
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

// Ref is one occurrence of a named binding: fresh occurrence slots ranging
// over the binding's committed value. Its cardinality is uniformly 0..many
// regardless of the body — the binding's public contract is its output
// shape, never derived properties of its implementation. The binding's
// body is not an input: occurrences observe a value, they do not consume a
// subtree.
type Ref struct {
	laws
	Binding string
	Scope   string
	// Canon aligns with Output().Fields: Canon[i] is the binding's
	// canonical slot for occurrence field i.
	Canon []lir.SlotID
}

// NewRef binds one occurrence: fields carry fresh occurrence slots, canon
// the binding's canonical slots in the same order.
func NewRef(binding, scope string, fields []lir.Field, canon []lir.SlotID) *Ref {
	slots := make([]lir.SlotID, len(fields))
	for i, f := range fields {
		slots[i] = f.Slot
	}
	return &Ref{
		laws: laws{
			out:      lir.RowType{Fields: fields},
			produced: NewSlotSet(slots...),
			card:     lir.Cardinality{Min: 0, Max: lir.Unbounded},
		},
		Binding: binding,
		Scope:   scope,
		Canon:   canon,
	}
}

func (r *Ref) Inputs() []Relation { return nil }

// Binding is one bound named relational value: the body bound once into
// canonical output slots, plus the derived plan-choice sensitivity — can
// two valid physical plans commit different legal bags?
type Binding struct {
	Name string
	Root Relation
	// PlanSensitive: the body contains a selection whose membership (or an
	// order-materialising crossing whose datum) is not uniquely determined,
	// so all occurrences must share one committed evaluation.
	PlanSensitive bool
}

// Query is a bound root relation plus its materialisation mode.
type Query struct {
	Root Relation
	Card lir.RootCard
	// Bindings in dependency order: every binding a body references
	// precedes it, so evaluation in list order always finds its
	// dependencies committed.
	Bindings []*Binding
	// Slots is how many slots the binder allocated. The planner's crossing
	// extraction allocates fresh slots starting here.
	Slots lir.SlotID
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
