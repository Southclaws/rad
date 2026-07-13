package planner

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

	"github.com/Southclaws/rad/rad/engine/reject"
	"slices"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
)

// Catalog is what binding needs from the schema. Callers choose the read
// view by choosing the implementation: catalog.NewReader over a statement's
// snapshot keeps schema resolution consistent with its data reads.
type Catalog interface {
	GetTable(ctx context.Context, name string) (catalog.Table, bool, error)
}

// Bind resolves q against the catalog. Every error is client-caused and
// carries the "planner:" prefix the server maps to an invalid-request
// problem.
func Bind(ctx context.Context, cat Catalog, q lir.Query) (*bound.Query, error) {
	switch q.Card {
	case lir.CardMany, lir.CardFirst, lir.CardExactlyOne, lir.CardScalar:
	default:
		return nil, reject.Inputf("planner: unknown root cardinality %q", q.Card)
	}

	b := &binder{ctx: ctx, cat: cat, labels: map[string]bool{}, bindings: map[string]*bound.Binding{}}

	// Bindings bind first, in dependency order, each body against an empty
	// scope stack — a binding is closed by construction: any reference to a
	// scope outside its own tree fails to resolve. The body binds once into
	// canonical slots; each occurrence re-exposes them under fresh slots.
	order, err := bindingOrder(q.Bindings)
	if err != nil {
		return nil, err
	}
	bindings := make([]*bound.Binding, 0, len(order))
	for _, name := range order {
		body, err := b.bindRel(q.Bindings[name])
		b.scopes = b.scopes[:0] // interior scopes never escape the binding
		if err != nil {
			return nil, bindingErr(name, err)
		}
		// A binding's public contract is its output schema, so that schema
		// must be well-formed: a raw join body with colliding column names
		// has no addressable output through a single occurrence scope.
		seen := map[string]bool{}
		for _, f := range body.Output().Fields {
			if seen[f.Name] {
				return nil, reject.Inputf("planner: binding %q output has duplicate column %q — project the body to a unique set of columns", name, f.Name)
			}
			seen[f.Name] = true
		}
		bnd := &bound.Binding{Name: name, Root: body, PlanSensitive: bound.PlanSensitive(body)}
		b.bindings[name] = bnd
		bindings = append(bindings, bnd)
	}

	root, err := b.bindRel(q.Root)
	if err != nil {
		return nil, err
	}

	if q.Card == lir.CardScalar && len(root.Output().Fields) != 1 {
		return nil, reject.Inputf("planner: a scalar query needs a single-column root, got %d columns", len(root.Output().Fields))
	}
	if q.Card == lir.CardScalar && !root.Card().AtMostOne() {
		return nil, reject.Inputf("planner: root scalar asserts at most one row, but the relation may produce more — aggregate it, slice it, or pin a unique key")
	}
	// first is deliberate row selection, same determinism rule as the First
	// crossing: at most one row statically, or an explicit logical order.
	if q.Card == lir.CardFirst && !root.Card().AtMostOne() && !bound.Ordered(root) {
		return nil, reject.Inputf("planner: root cardinality %q over an unordered multi-row relation would make results depend on the access path — add an order or make the relation at-most-one", q.Card)
	}

	return &bound.Query{Root: root, Card: q.Card, Bindings: bindings, Slots: b.nextSlot}, nil
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
}

// bindingErr annotates a binding-body error with the binding's name; %w
// keeps the reject classification intact through the wrap.
func bindingErr(name string, err error) error {
	return fmt.Errorf("planner: binding %q: %w", name, err)
}

// bindingOrder returns binding names in dependency order — every binding a
// body references precedes it. Cycles are rejected (the wire preflight also
// rejects them; direct engine callers get the same rule).
func bindingOrder(bindings map[string]lir.Relation) ([]string, error) {
	names := make([]string, 0, len(bindings))
	for name := range bindings {
		if name == "" {
			return nil, reject.Inputf("planner: binding names must not be empty")
		}
		names = append(names, name)
	}
	slices.Sort(names)

	const (
		visiting = 1
		visited  = 2
	)
	state := map[string]uint8{}
	var order []string
	var visit func(string) error
	visit = func(name string) error {
		if _, ok := bindings[name]; !ok {
			return nil // dangling refs surface as unknown-binding at bind time
		}
		switch state[name] {
		case visiting:
			return reject.Inputf("planner: binding %q is part of a binding cycle", name)
		case visited:
			return nil
		}
		state[name] = visiting
		for _, dep := range lirBindingDeps(bindings[name]) {
			if err := visit(dep); err != nil {
				return err
			}
		}
		state[name] = visited
		order = append(order, name)
		return nil
	}
	for _, name := range names {
		if err := visit(name); err != nil {
			return nil, err
		}
	}
	return order, nil
}

// lirBindingDeps collects the binding names an unbound relation references.
func lirBindingDeps(r lir.Relation) []string {
	var deps []string
	var rel func(lir.Relation)
	var expr func(lir.Expr)
	rel = func(r lir.Relation) {
		switch n := r.(type) {
		case lir.Ref:
			deps = append(deps, n.Binding)
		case lir.Filter:
			rel(n.Input)
			expr(n.Pred)
		case lir.Project:
			rel(n.Input)
			for _, f := range n.Fields {
				expr(f.Expr)
			}
		case lir.Join:
			rel(n.Left)
			rel(n.Right)
			expr(n.On)
		case lir.Aggregate:
			rel(n.Input)
			for _, g := range n.Groups {
				expr(g.Expr)
			}
			for _, t := range n.Terms {
				expr(t.Arg)
			}
		case lir.Order:
			rel(n.Input)
			for _, t := range n.Terms {
				expr(t.Expr)
			}
		case lir.Slice:
			rel(n.Input)
		}
	}
	expr = func(e lir.Expr) {
		switch x := e.(type) {
		case lir.Unary:
			expr(x.X)
		case lir.Binary:
			expr(x.L)
			expr(x.R)
		case lir.Cast:
			expr(x.X)
		case lir.Call:
			for _, a := range x.Args {
				expr(a)
			}
		case lir.Exists:
			rel(x.Rel)
		case lir.First:
			rel(x.Rel)
		case lir.Scalar:
			rel(x.Rel)
		case lir.Array:
			rel(x.Rel)
		}
	}
	rel(r)
	return deps
}

type scopeEntry struct {
	label string
	rel   bound.Relation
}

func (b *binder) bindRel(r lir.Relation) (bound.Relation, error) {
	switch n := r.(type) {
	case lir.Scan:
		return b.bindScan(n)

	case lir.Ref:
		return b.bindRef(n)

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
		return nil, reject.Inputf("planner: unknown table %q", n.Table)
	}
	slots := make([]lir.SlotID, len(tbl.Columns))
	for i := range slots {
		slots[i] = b.nextSlot
		b.nextSlot++
	}
	s := bound.NewScan(tbl, n.Scope, slots)
	b.labels[n.Scope] = true
	b.scopes = append(b.scopes, scopeEntry{label: n.Scope, rel: s})
	return s, nil
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
	out := bnd.Root.Output()
	fields := make([]lir.Field, len(out.Fields))
	canon := make([]lir.SlotID, len(out.Fields))
	for i, f := range out.Fields {
		fields[i] = lir.Field{Name: f.Name, Slot: b.nextSlot, Type: f.Type}
		canon[i] = f.Slot
		b.nextSlot++
	}
	r := bound.NewRef(n.Binding, n.Scope, fields, canon)
	b.labels[n.Scope] = true
	b.scopes = append(b.scopes, scopeEntry{label: n.Scope, rel: r})
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
		if b.labels[n.Scope] {
			return nil, reject.Inputf("planner: duplicate scope %q", n.Scope)
		}
		b.labels[n.Scope] = true
		b.scopes = append(b.scopes, scopeEntry{label: n.Scope, rel: p})
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
	switch x := e.(type) {
	case bound.Exists, bound.First, bound.Scalar, bound.Array:
		return true
	case bound.Unary:
		return containsCrossing(x.X)
	case bound.Binary:
		return containsCrossing(x.L) || containsCrossing(x.R)
	case bound.Cast:
		return containsCrossing(x.X)
	case bound.Call:
		for _, a := range x.Args {
			if containsCrossing(a) {
				return true
			}
		}
	}
	return false
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
		if b.labels[n.Scope] {
			return nil, reject.Inputf("planner: duplicate scope %q", n.Scope)
		}
		b.labels[n.Scope] = true
		b.scopes = append(b.scopes, scopeEntry{label: n.Scope, rel: a})
	}
	return a, nil
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

// ── uniqueness-aware cardinality ────────────────────────────────────────────

// refineUnique tightens a filter to at-most-one row when its equality
// conjuncts pin a unique key of the underlying scan: every conjunct
// `scanColumn = rhs` where rhs is fixed per evaluation (a literal or an
// outer reference — anything not reading the scan itself) pins that column,
// and pinning a whole primary key or unique index means at most one row.
// This is what lets the to-parent pattern (First over an FK→PK filter) pass
// the determinism rule statically.
func (b *binder) refineUnique(f *bound.Filter) {
	scan := underlyingScan(f)
	if scan == nil {
		return
	}
	slotToCol := map[lir.SlotID]string{}
	for _, fld := range scan.Output().Fields {
		slotToCol[fld.Slot] = fld.Name
	}

	pinned := map[string]bool{}
	var walk func(rel bound.Relation)
	collect := func(pred bound.Expr) {
		for _, c := range conjuncts(pred) {
			bin, ok := c.(bound.Binary)
			if !ok || bin.Op != lir.OpEq {
				continue
			}
			for _, side := range [2][2]bound.Expr{{bin.L, bin.R}, {bin.R, bin.L}} {
				ref, ok := side[0].(bound.SlotRef)
				if !ok {
					continue
				}
				col, ours := slotToCol[ref.Slot]
				if !ours {
					continue
				}
				// rhs must not read the scan: then its value is fixed per
				// evaluation of this relation.
				if !readsAny(side[1], scan.Produced()) {
					pinned[col] = true
				}
			}
		}
	}
	walk = func(rel bound.Relation) {
		switch n := rel.(type) {
		case *bound.Filter:
			collect(n.Pred)
			walk(n.In)
		case *bound.Order:
			walk(n.In)
		case *bound.Slice:
			walk(n.In)
		}
	}
	walk(f)

	covers := func(cols []string) bool {
		if len(cols) == 0 {
			return false
		}
		for _, c := range cols {
			if !pinned[c] {
				return false
			}
		}
		return true
	}
	if covers(scan.Table.PrimaryKey) {
		f.RefineCard(lir.Cardinality{Min: 0, Max: 1})
		return
	}
	for _, idx := range scan.Table.Indexes {
		if idx.Unique && covers(idx.Columns) {
			f.RefineCard(lir.Cardinality{Min: 0, Max: 1})
			return
		}
	}
}

// underlyingScan walks order-preserving wrappers down to a scan, or nil.
func underlyingScan(rel bound.Relation) *bound.Scan {
	for {
		switch n := rel.(type) {
		case *bound.Scan:
			return n
		case *bound.Filter:
			rel = n.In
		case *bound.Order:
			rel = n.In
		case *bound.Slice:
			rel = n.In
		default:
			return nil
		}
	}
}

// conjuncts flattens a predicate's top-level AND tree.
func conjuncts(e bound.Expr) []bound.Expr {
	if bin, ok := e.(bound.Binary); ok && bin.Op == lir.OpAnd {
		return append(conjuncts(bin.L), conjuncts(bin.R)...)
	}
	return []bound.Expr{e}
}

// readsAny reports whether e references any slot in set.
func readsAny(e bound.Expr, set bound.SlotSet) bool {
	return slices.ContainsFunc(e.FreeSlots().Slots(), set.Contains)
}

// ── order tie-breaker ───────────────────────────────────────────────────────

// appendTieBreaker appends a known unique key of the relation's output as
// final ascending terms, so tied rows order identically under every access
// path. Without a known unique key, ties are documented as unspecified.
func appendTieBreaker(in bound.Relation, terms []bound.OrderTerm) []bound.OrderTerm {
	key := uniqueKeyFields(in)
	if key == nil {
		return terms
	}
	referenced := map[lir.SlotID]bool{}
	for _, t := range terms {
		if ref, ok := t.Expr.(bound.SlotRef); ok {
			referenced[ref.Slot] = true
		}
	}
	for _, f := range key {
		if !referenced[f.Slot] {
			terms = append(terms, bound.OrderTerm{
				Expr: bound.SlotRef{Slot: f.Slot, Name: f.Name, T: f.Type},
			})
		}
	}
	return terms
}

// uniqueKeyFields finds a unique key among the relation's output fields: a
// scan's primary key seen through order-preserving operators (and through
// projections that keep every key slot), or a grouped aggregate's group
// attributes. A global fold has one row — no ties to break.
func uniqueKeyFields(rel bound.Relation) []lir.Field {
	switch n := rel.(type) {
	case *bound.Scan:
		key := make([]lir.Field, 0, len(n.Table.PrimaryKey))
		for _, col := range n.Table.PrimaryKey {
			f, ok := n.Output().Lookup(col)
			if !ok {
				return nil
			}
			key = append(key, f)
		}
		return key
	case *bound.Filter:
		return uniqueKeyFields(n.In)
	case *bound.Order:
		return uniqueKeyFields(n.In)
	case *bound.Slice:
		return uniqueKeyFields(n.In)
	case *bound.Project:
		key := uniqueKeyFields(n.In)
		if key == nil {
			return nil
		}
		for _, f := range key {
			if !slices.Contains(n.Output().Slots(), f.Slot) {
				return nil
			}
		}
		return key
	case *bound.Aggregate:
		if len(n.Groups) == 0 {
			return nil
		}
		key := make([]lir.Field, 0, len(n.Groups))
		out := n.Output()
		for _, g := range n.Groups {
			f, ok := out.Lookup(g.Name)
			if !ok {
				return nil
			}
			key = append(key, f)
		}
		return key
	}
	return nil
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
