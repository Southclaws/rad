package bind

// The binder is the engine's front door: it resolves an unbound lir.Query —
// table, column, and scope names plus raw literals, exactly as a frontend
// produced it — into bound IR the planner and executor trust completely.
// One recursive walk performs scope resolution, name/ID binding, slot
// assignment, literal coercion, type inference, cardinality inference
// (uniqueness-aware), and the full validation matrix.
//
// Cycle detection is deliberately absent: unbound nodes are value structs,
// which cannot form cycles. The wire's graph decoder — the only place string
// node references exist — rejects cyclic graphs while materialising them
// into values.

import (
	"context"
	"fmt"
	"strings"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	lirinspect "github.com/Southclaws/rad/rad/engine/03_lir/inspect"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// Catalog is what binding needs from the schema. Callers choose the read
// view by choosing the implementation: store.New over a statement's
// snapshot keeps schema resolution consistent with its data reads.
type Catalog interface {
	GetTable(ctx context.Context, name string) (model.Table, bool, error)
}

// Bind resolves q against the catalog. Every error is client-caused and
// carries the "planner:" prefix the server maps to an invalid-request
// problem.
func Bind(ctx context.Context, cat Catalog, q lir.Query) (*bound.Query, error) {
	b := &binder{ctx: ctx, cat: cat, labels: map[string]bool{}, bindings: map[string]*bound.Binding{}}
	return b.bindQuery(q, nil)
}

// bindQuery binds one query against the catalog, with `program` bindings
// (prior statement results, in PIR) available for `ref` resolution but not
// re-planned as this query's own. It resets the per-query scope/label/binding
// state but keeps the slot counter, so a program's statements share one dense
// slot space and each result's frames remap cleanly through later refs.
func (b *binder) bindQuery(q lir.Query, program map[string]*bound.Binding) (*bound.Query, error) {
	switch q.Card {
	case lir.CardMany, lir.CardFirst, lir.CardExactlyOne, lir.CardScalar:
	default:
		return nil, reject.Inputf("planner: unknown root cardinality %q", q.Card)
	}

	root, bindings, err := b.bindBody(q, program)
	if err != nil {
		return nil, err
	}

	// The root of a many/first/exactly_one query renders each row as an object
	// keyed by output attribute name, so those names must be unique — otherwise
	// a join that concatenated two colliding columns would silently collapse to
	// one on the wire. (scalar renders a bare value and is checked for a single
	// column just below.) Column *references* stay unambiguous via their scope;
	// only the object rendering forces uniqueness, so it is enforced here at the
	// observable boundary rather than on the join itself.
	if q.Card != lir.CardScalar {
		if err := requireUniqueOutput(root, "the query root"); err != nil {
			return nil, err
		}
	}
	if q.Card == lir.CardScalar && len(root.Output().Fields) != 1 {
		return nil, reject.Inputf("planner: a scalar query needs a single-column root, got %d columns", len(root.Output().Fields))
	}
	if q.Card == lir.CardScalar && !root.Card().AtMostOne() {
		return nil, reject.Inputf("planner: root scalar asserts at most one row, but the relation may produce more — aggregate it, slice it, or pin a unique key")
	}
	if q.Card == lir.CardMany && !bound.Ordered(root) {
		return nil, reject.Inputf("planner: root cardinality %q needs an explicit order — observable collections must not depend on the access path", q.Card)
	}
	// first is deliberate row selection, same determinism rule as the First
	// crossing: at most one row statically, or an explicit logical order.
	if q.Card == lir.CardFirst && !root.Card().AtMostOne() && !bound.Ordered(root) {
		return nil, reject.Inputf("planner: root cardinality %q over an unordered multi-row relation would make results depend on the access path — add an order or make the relation at-most-one", q.Card)
	}

	return &bound.Query{Root: root, Card: q.Card, Bindings: bindings, Slots: b.nextSlot}, nil
}

// bindBag binds a query's relation tree as a bag: the local bindings and the
// root, with `program` bindings available, but none of the root-cardinality
// shaping a query applies. A PIR mutation statement consumes its input as an
// unordered set, so the observable-collection order rule does not apply — the
// rows are applied, not observed in sequence. The returned Query carries
// CardMany purely so the planner shapes it as a stream of every row.
func (b *binder) bindBag(q lir.Query, program map[string]*bound.Binding) (*bound.Query, error) {
	root, bindings, err := b.bindBody(q, program)
	if err != nil {
		return nil, err
	}
	return &bound.Query{Root: root, Card: lir.CardMany, Bindings: bindings, Slots: b.nextSlot}, nil
}

// bindBody is the shared core: reset the per-query scope/label/binding state
// (keeping the slot counter), seed the program bindings for resolution, bind
// the local bindings in dependency order, then bind the root. Cardinality
// rules are the caller's — bindQuery applies them, bindBag does not.
func (b *binder) bindBody(q lir.Query, program map[string]*bound.Binding) (bound.Relation, []*bound.Binding, error) {
	b.labels = map[string]bool{}
	b.scopes = b.scopes[:0]
	b.used = map[string]bool{}
	b.bindings = make(map[string]*bound.Binding, len(program))
	for name, bnd := range program {
		b.bindings[name] = bnd
	}

	// Bindings bind first, in dependency order, each body against an empty
	// scope stack — a binding is closed by construction: any reference to a
	// scope outside its own tree fails to resolve. The body binds once into
	// canonical slots; each occurrence re-exposes them under fresh slots.
	order, err := bindingOrder(q.Bindings)
	if err != nil {
		return nil, nil, err
	}
	bindings := make([]*bound.Binding, 0, len(order))
	for _, name := range order {
		// A local binding may not shadow any program statement name — not
		// just an already-bound earlier one — so the whole program's
		// namespace is collision-free regardless of statement order, and
		// resolution never has to choose between a local and a statement
		// result.
		if b.reserved[name] {
			return nil, nil, reject.Inputf("planner: binding %q shadows a statement name", name)
		}
		if rec, ok := q.Bindings[name].(lir.Recursive); ok {
			bnd, err := b.bindRecursiveBinding(name, rec)
			if err != nil {
				return nil, nil, bindingErr(name, err)
			}
			bindings = append(bindings, bnd)
			continue
		}
		body, err := b.bindRel(q.Bindings[name])
		b.scopes = b.scopes[:0] // interior scopes never escape the binding
		if err != nil {
			return nil, nil, bindingErr(name, err)
		}
		// A binding's public contract is its output schema, so that schema
		// must be well-formed: a raw join body with colliding column names
		// has no addressable output through a single occurrence scope.
		if duplicate, ok := duplicateColumn(body.Output()); ok {
			return nil, nil, reject.Inputf("planner: binding %q output has duplicate column %q — project the body to a unique set of columns", name, duplicate)
		}
		bnd := &bound.Binding{Name: name, Root: body, PlanSensitive: lirinspect.PlanSensitive(body)}
		b.bindings[name] = bnd
		bindings = append(bindings, bnd)
	}

	root, err := b.bindRel(q.Root)
	if err != nil {
		return nil, nil, err
	}

	// Every local binding must be observed by at least one ref. An unreferenced
	// binding is a declared relational value that denotes nothing — the binding
	// analogue of an unreachable node, and always a mistake. Program-statement
	// bindings are exempt: an unconsumed statement result is legitimate.
	for _, name := range order {
		if !b.used[name] {
			return nil, nil, reject.Inputf("planner: binding %q is never referenced", name)
		}
	}
	return root, bindings, nil
}

// requireUniqueOutput rejects a relation whose output row carries two
// attributes with the same name. It guards the boundaries where a relation is
// flattened into an object keyed by name — the query root and the First/Array
// crossings — since duplicate keys would silently collapse (last wins) in the
// rendered result. A join is the only operator that concatenates two row types
// into colliding names; project and aggregate already enforce uniqueness when
// they build their own output. This is the same rule ProjectNode states,
// applied at the remaining observable boundaries.
func requireUniqueOutput(rel bound.Relation, what string) error {
	if duplicate, ok := duplicateColumn(rel.Output()); ok {
		return reject.Fail(reject.ReasonProjectionCollision, "planner: %s has duplicate column %q — project it to a unique set of columns", what, duplicate)
	}
	return nil
}

// binder carries the walk state: the dense slot allocator and the scope
// visibility stack.
type binder struct {
	ctx      context.Context
	cat      Catalog
	nextSlot lir.SlotID
	scopes   []scopeEntry    // innermost last
	labels   map[string]bool // every label bound anywhere (query-wide uniqueness)
	bindings map[string]*bound.Binding
	used     map[string]bool // binding names observed by at least one ref
	// reserved holds every PIR program statement name, so a statement-local
	// binding cannot shadow any statement — even one defined later. Empty
	// for a standalone query.
	reserved map[string]bool
	// recursing names the recursive binding whose step is currently being
	// bound, so a recursive_ref resolves to its provisional (anchor) shape
	// and is rejected anywhere else.
	recursing string
}

type scopeEntry struct {
	label string
	rel   bound.Relation
}

func (b *binder) bindRel(r lir.Relation) (bound.Relation, error) {
	switch n := r.(type) {
	case lir.Scan:
		return b.bindScan(n)

	case lir.Rows:
		return b.bindRows(n)

	case lir.Ref:
		return b.bindRef(n)

	case lir.RecursiveRef:
		return b.bindRecursiveRef(n)

	case lir.Recursive:
		return nil, reject.Inputf("planner: a recursive relation is only valid as a binding body, not an ordinary node")

	case lir.Distinct:
		in, err := b.bindRel(n.Input)
		if err != nil {
			return nil, err
		}
		return bound.NewDistinct(in), nil

	case lir.Filter:
		in, err := b.bindRel(n.Input)
		if err != nil {
			return nil, err
		}
		pred, err := b.bindExpr(n.Pred)
		if err != nil {
			return nil, err
		}
		if pred.Type().Kind != lir.KindBool {
			return nil, reject.Inputf("planner: filter predicate must be boolean, got %s", pred.Type())
		}
		f := bound.NewFilter(in, pred)
		b.refineUnique(f)
		return f, nil

	case lir.Project:
		return b.bindProject(n)

	case lir.Join:
		return b.bindJoin(n)

	case lir.Aggregate:
		return b.bindAggregate(n)

	case lir.Order:
		in, err := b.bindRel(n.Input)
		if err != nil {
			return nil, err
		}
		if len(n.Terms) == 0 {
			return nil, reject.Inputf("planner: order needs at least one term")
		}
		terms := make([]bound.OrderTerm, 0, len(n.Terms)+2)
		for _, t := range n.Terms {
			e, err := b.bindExpr(t.Expr)
			if err != nil {
				return nil, err
			}
			if !e.Type().Kind.Scalar() {
				return nil, reject.Inputf("planner: cannot order by a %s value", e.Type().Kind)
			}
			terms = append(terms, bound.OrderTerm{Expr: e, Desc: t.Desc})
		}
		return bound.NewOrder(in, appendTieBreaker(in, terms)), nil

	case lir.Slice:
		in, err := b.bindRel(n.Input)
		if err != nil {
			return nil, err
		}
		if n.Offset < 0 {
			return nil, reject.Inputf("planner: slice offset must be >= 0, got %d", n.Offset)
		}
		if n.Limit != nil && *n.Limit < 0 {
			return nil, reject.Inputf("planner: slice limit must be >= 0, got %d", *n.Limit)
		}
		if n.Offset > 0 && !bound.Ordered(in) {
			return nil, reject.Inputf("planner: slice offset over an unordered relation would make membership depend on the access path — add an order")
		}
		return bound.NewSlice(in, n.Offset, n.Limit), nil

	case nil:
		return nil, reject.Inputf("planner: missing relation")
	default:
		return nil, reject.Inputf("planner: unknown relation node %T", r)
	}
}

func (b *binder) bindScan(n lir.Scan) (*bound.Scan, error) {
	if n.Scope == "" {
		return nil, reject.Inputf("planner: scan of %q needs a scope label", n.Table)
	}
	if b.labels[n.Scope] {
		return nil, reject.Inputf("planner: duplicate scope %q", n.Scope)
	}
	tbl, ok, err := b.cat.GetTable(b.ctx, n.Table)
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, reject.Fail(reject.ReasonUnknownTable, "planner: unknown table %q", n.Table)
	}
	slots := b.freshSlots(len(tbl.Columns))
	s := bound.NewScan(tbl, n.Scope, slots)
	if err := b.exposeScope(n.Scope, s); err != nil {
		return nil, err
	}
	return s, nil
}

// bindRows binds a constant relation: each cell is validated and decoded
// against its declared column type under the same rules as scalar literals
// (declared, never inferred — a relation's schema must not depend on its
// data), a NULL cell is valid only for a nullable column, and the scope
// enters the stack exactly as a scan's does.
func (b *binder) bindRows(n lir.Rows) (*bound.Rows, error) {
	if n.Scope == "" {
		return nil, reject.Inputf("planner: rows needs a scope label")
	}
	if b.labels[n.Scope] {
		return nil, reject.Inputf("planner: duplicate scope %q", n.Scope)
	}
	if len(n.Columns) == 0 {
		return nil, reject.Inputf("planner: rows (%s) needs at least one column", n.Scope)
	}
	seen := map[string]bool{}
	for _, c := range n.Columns {
		if c.Name == "" {
			return nil, reject.Inputf("planner: rows (%s) has a column with no name", n.Scope)
		}
		if seen[c.Name] {
			return nil, reject.Inputf("planner: rows (%s) declares column %q twice", n.Scope, c.Name)
		}
		seen[c.Name] = true
		if !c.Kind.Scalar() {
			return nil, reject.Inputf("planner: rows (%s) column %q has unsupported type %q", n.Scope, c.Name, c.Kind)
		}
	}

	vals := make([][]lir.Value, len(n.Values))
	for i, row := range n.Values {
		if len(row) != len(n.Columns) {
			return nil, reject.Inputf("planner: rows (%s) row %d has %d values, want %d", n.Scope, i, len(row), len(n.Columns))
		}
		cells := make([]lir.Value, len(row))
		for j, raw := range row {
			col := n.Columns[j]
			lit, err := coerceLiteral(raw, lir.Type{Kind: col.Kind})
			if err != nil {
				return nil, reject.Inputf("planner: rows (%s) row %d column %q: %s",
					n.Scope, i, col.Name, strings.TrimPrefix(err.Error(), "planner: "))
			}
			if lit.V.Null && !col.Nullable {
				return nil, reject.Inputf("planner: rows (%s) row %d column %q is not nullable", n.Scope, i, col.Name)
			}
			cells[j] = lit.V
		}
		vals[i] = cells
	}

	slots := b.freshSlots(len(n.Columns))
	fields := make([]lir.Field, len(n.Columns))
	for i, c := range n.Columns {
		fields[i] = lir.Field{
			Name: c.Name,
			Slot: slots[i],
			Type: lir.Type{Kind: c.Kind, Nullable: c.Nullable},
		}
	}
	r := bound.NewRows(n.Scope, fields, vals)
	if err := b.exposeScope(n.Scope, r); err != nil {
		return nil, err
	}
	return r, nil
}

// bindRef binds one occurrence of a binding: fresh slots over the
// binding's canonical output, exposed under the occurrence's own scope —
// exactly a scan's shape, with the binding's committed value where the
// table would be. Interior scopes of the binding are not re-exposed.
func (b *binder) bindRef(n lir.Ref) (*bound.Ref, error) {
	if n.Scope == "" {
		return nil, reject.Inputf("planner: ref of binding %q needs a scope label", n.Binding)
	}
	if b.labels[n.Scope] {
		return nil, reject.Inputf("planner: duplicate scope %q", n.Scope)
	}
	bnd, ok := b.bindings[n.Binding]
	if !ok {
		return nil, reject.Inputf("planner: unknown binding %q", n.Binding)
	}
	b.used[n.Binding] = true
	out := bnd.Root.Output()
	if bnd.Recursive {
		out = bnd.Out
	}
	fields, canon := b.freshOccurrence(out)
	r := bound.NewRef(n.Binding, n.Scope, fields, canon)
	if err := b.exposeScope(n.Scope, r); err != nil {
		return nil, err
	}
	return r, nil
}

// bindProject establishes a new row type. The scopes its subtree bound stop
// being visible above it — the projection's output (optionally labelled) is
// what later operators address. Spread scopes keep their source slots;
// computed fields get fresh ones.
func (b *binder) bindProject(n lir.Project) (*bound.Project, error) {
	mark := len(b.scopes)
	in, err := b.bindRel(n.Input)
	if err != nil {
		return nil, err
	}

	names := map[string]bool{}
	var fields []bound.ProjField
	addField := func(name string, slot lir.SlotID, e bound.Expr) error {
		if name == "" {
			return reject.Inputf("planner: projection field needs a name (as)")
		}
		if names[name] {
			return reject.Inputf("planner: duplicate projection field %q", name)
		}
		names[name] = true
		fields = append(fields, bound.ProjField{Name: name, Slot: slot, Expr: e})
		return nil
	}

	for _, label := range n.Spread {
		entry, ok := b.findScope(label, mark)
		if !ok {
			return nil, reject.Inputf("planner: spread scope %q is not produced beneath the projection", label)
		}
		for _, f := range entry.rel.Output().Fields {
			if err := addField(f.Name, f.Slot, bound.SlotRef{Slot: f.Slot, Name: label + "." + f.Name, T: f.Type}); err != nil {
				return nil, err
			}
		}
	}
	for _, pf := range n.Fields {
		e, err := b.bindExpr(pf.Expr)
		if err != nil {
			return nil, err
		}
		slot := b.slotFor(e)
		if err := addField(pf.As, slot, e); err != nil {
			return nil, err
		}
	}
	if len(fields) == 0 {
		return nil, reject.Inputf("planner: projection has no fields")
	}

	b.scopes = b.scopes[:mark]
	p := bound.NewProject(in, n.Scope, fields)
	if n.Scope != "" {
		if err := b.exposeScope(n.Scope, p); err != nil {
			return nil, err
		}
	}
	return p, nil
}

func (b *binder) bindJoin(n lir.Join) (*bound.Join, error) {
	switch n.Kind {
	case lir.InnerJoin, lir.LeftJoin:
	default:
		return nil, reject.Inputf("planner: unsupported join kind %q", n.Kind)
	}
	l, err := b.bindRel(n.Left)
	if err != nil {
		return nil, err
	}
	r, err := b.bindRel(n.Right)
	if err != nil {
		return nil, err
	}
	// A join input may be correlated with an enclosing query, but never with
	// its sibling: the executor builds each side independently, so a dependent
	// right side would not mean what it says. Reject it here.
	for _, slot := range r.FreeSlots().Slots() {
		if l.Produced().Contains(slot) {
			desc := slotDesc(l, slot)
			if desc == "" {
				desc = "a column"
			}
			return nil, reject.Inputf("planner: join right side references %s from the left side; a join input cannot depend on the other input — put the condition in the join's `on`, or correlate through a crossing instead", desc)
		}
	}
	on, err := b.bindExpr(n.On)
	if err != nil {
		return nil, err
	}
	if on.Type().Kind != lir.KindBool {
		return nil, reject.Inputf("planner: join condition must be boolean, got %s", on.Type())
	}
	// A crossing in the join condition would need evaluation per candidate
	// pair — a shape the executor does not batch. Filtering above the join
	// says the same thing and gets the full attach machinery.
	if containsCrossing(on) {
		return nil, reject.Inputf("planner: a join condition cannot contain a sub-relation crossing — filter above the join instead")
	}
	return bound.NewJoin(l, r, n.Kind, on), nil
}

// containsCrossing reports whether any sub-expression is a cardinality
// crossing.
func containsCrossing(e bound.Expr) bool {
	found := false
	lirinspect.WalkExpr(e, func(expr bound.Expr) {
		switch expr.(type) {
		case bound.Exists, bound.First, bound.Scalar, bound.Array:
			found = true
		}
	})
	return found
}

// bindAggregate folds its input. Above it, only the group and term outputs
// are addressable — the input's scopes close here, which is what makes
// "columns above an aggregate resolve only to groups/terms" structural
// rather than a rule.
func (b *binder) bindAggregate(n lir.Aggregate) (*bound.Aggregate, error) {
	mark := len(b.scopes)
	in, err := b.bindRel(n.Input)
	if err != nil {
		return nil, err
	}

	names := map[string]bool{}
	unique := func(name string) error {
		if names[name] {
			return reject.Inputf("planner: duplicate aggregate output name %q", name)
		}
		names[name] = true
		return nil
	}

	groups := make([]bound.GroupTerm, 0, len(n.Groups))
	for _, g := range n.Groups {
		e, err := b.bindExpr(g.Expr)
		if err != nil {
			return nil, err
		}
		if !e.Type().Kind.Scalar() {
			return nil, reject.Inputf("planner: cannot group by a %s value", e.Type().Kind)
		}
		name := g.As
		if name == "" {
			col, ok := g.Expr.(lir.Column)
			if !ok {
				return nil, reject.Inputf("planner: group expression needs an output name (as)")
			}
			name = col.Name
		}
		if err := unique(name); err != nil {
			return nil, err
		}
		groups = append(groups, bound.GroupTerm{Name: name, Slot: b.slotFor(e), Expr: e})
	}

	terms := make([]bound.AggTerm, 0, len(n.Terms))
	for _, t := range n.Terms {
		if t.As == "" {
			return nil, reject.Inputf("planner: aggregate %s needs an output name (as)", t.Fn)
		}
		if err := unique(t.As); err != nil {
			return nil, err
		}
		var arg bound.Expr
		if t.Arg != nil {
			if arg, err = b.bindExpr(t.Arg); err != nil {
				return nil, err
			}
		}
		switch t.Fn {
		case lir.AggCount:
			if arg != nil && !arg.Type().Kind.Scalar() {
				return nil, reject.Inputf("planner: count needs a scalar argument, got %s", arg.Type().Kind)
			}
		case lir.AggSum, lir.AggAvg:
			if arg == nil {
				return nil, reject.Inputf("planner: %s needs an argument", t.Fn)
			}
			if !arg.Type().Kind.Numeric() {
				return nil, reject.Inputf("planner: %s requires a numeric argument, got %s", t.Fn, arg.Type())
			}
		case lir.AggMin, lir.AggMax:
			if arg == nil {
				return nil, reject.Inputf("planner: %s needs an argument", t.Fn)
			}
			if !arg.Type().Kind.Scalar() {
				return nil, reject.Inputf("planner: %s needs a scalar argument, got %s", t.Fn, arg.Type().Kind)
			}
		default:
			return nil, reject.Inputf("planner: unknown aggregate function %q", t.Fn)
		}
		slot := b.nextSlot
		b.nextSlot++
		terms = append(terms, bound.AggTerm{Fn: t.Fn, Arg: arg, Name: t.As, Slot: slot, T: bound.AggTermType(t.Fn, arg)})
	}
	if len(groups) == 0 && len(terms) == 0 {
		return nil, reject.Inputf("planner: aggregate needs at least one group or term")
	}

	b.scopes = b.scopes[:mark]
	a := bound.NewAggregate(in, groups, terms)
	if n.Scope != "" {
		if err := b.exposeScope(n.Scope, a); err != nil {
			return nil, err
		}
	}
	return a, nil
}

func (b *binder) exposeScope(label string, rel bound.Relation) error {
	if b.labels[label] {
		return reject.Inputf("planner: duplicate scope %q", label)
	}
	b.labels[label] = true
	b.scopes = append(b.scopes, scopeEntry{label: label, rel: rel})
	return nil
}

func (b *binder) freshSlots(count int) []lir.SlotID {
	slots := make([]lir.SlotID, count)
	for i := range slots {
		slots[i] = b.nextSlot
		b.nextSlot++
	}
	return slots
}

func (b *binder) freshOccurrence(out lir.RowType) ([]lir.Field, []lir.SlotID) {
	fields := make([]lir.Field, len(out.Fields))
	canonical := make([]lir.SlotID, len(out.Fields))
	for i, field := range out.Fields {
		fields[i] = lir.Field{Name: field.Name, Slot: b.nextSlot, Type: field.Type}
		canonical[i] = field.Slot
		b.nextSlot++
	}
	return fields, canonical
}

// findScope resolves a label against the visibility stack, innermost first.
// from limits the search to entries pushed at or after that stack index (used
// by spread, which may only name scopes produced beneath the projection).
func (b *binder) findScope(label string, from int) (scopeEntry, bool) {
	for i := len(b.scopes) - 1; i >= from; i-- {
		if b.scopes[i].label == label {
			return b.scopes[i], true
		}
	}
	return scopeEntry{}, false
}

// slotFor reuses a bare slot reference's slot — a renamed or spread column is
// the same attribute, not a copy — and allocates a fresh slot for anything
// computed.
func (b *binder) slotFor(e bound.Expr) lir.SlotID {
	if ref, ok := e.(bound.SlotRef); ok {
		return ref.Slot
	}
	s := b.nextSlot
	b.nextSlot++
	return s
}

// slotDesc names a slot for error messages by searching rel's subtree —
// scans give the qualified spelling, other outputs just the field name.
// Empty means the slot was not found.
func slotDesc(rel bound.Relation, slot lir.SlotID) string {
	if sc, ok := rel.(*bound.Scan); ok {
		for _, f := range sc.Output().Fields {
			if f.Slot == slot {
				return fmt.Sprintf("column %q of scope %q", f.Name, sc.Scope)
			}
		}
	}
	for _, in := range rel.Inputs() {
		if d := slotDesc(in, slot); d != "" {
			return d
		}
	}
	for _, f := range rel.Output().Fields {
		if f.Slot == slot {
			return fmt.Sprintf("column %q", f.Name)
		}
	}
	return ""
}
