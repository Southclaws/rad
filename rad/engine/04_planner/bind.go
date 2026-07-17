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
	"maps"
	"strings"

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
		seen := map[string]bool{}
		for _, f := range body.Output().Fields {
			if seen[f.Name] {
				return nil, nil, reject.Inputf("planner: binding %q output has duplicate column %q — project the body to a unique set of columns", name, f.Name)
			}
			seen[f.Name] = true
		}
		bnd := &bound.Binding{Name: name, Root: body, PlanSensitive: bound.PlanSensitive(body)}
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
	seen := map[string]bool{}
	for _, f := range rel.Output().Fields {
		if seen[f.Name] {
			return reject.Fail(reject.ReasonProjectionCollision, "planner: %s has duplicate column %q — project it to a unique set of columns", what, f.Name)
		}
		seen[f.Name] = true
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
		case lir.Recursive:
			rel(n.Anchor)
			rel(n.Step)
		case lir.RecursiveRef:
			// The sanctioned self-edge: a recursive binding's step refers to
			// itself through recursive_ref, which is not an ordering
			// dependency — so the binding graph stays topologically sortable.
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

	fields := make([]lir.Field, len(n.Columns))
	for i, c := range n.Columns {
		fields[i] = lir.Field{
			Name: c.Name,
			Slot: b.nextSlot,
			Type: lir.Type{Kind: c.Kind, Nullable: c.Nullable},
		}
		b.nextSlot++
	}
	r := bound.NewRows(n.Scope, fields, vals)
	b.labels[n.Scope] = true
	b.scopes = append(b.scopes, scopeEntry{label: n.Scope, rel: r})
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

// bindRecursiveRef binds one occurrence of the enclosing recursive binding's
// frontier: fresh slots over the binding's provisional (anchor) output,
// exposed under the occurrence's own scope. Placement and linearity are
// enforced structurally by validateRecursive before binding begins; this only
// resolves the reference and rejects a recursive_ref that escaped a step.
func (b *binder) bindRecursiveRef(n lir.RecursiveRef) (*bound.RecursiveRef, error) {
	if n.Scope == "" {
		return nil, reject.Inputf("planner: recursive_ref of binding %q needs a scope label", n.Binding)
	}
	if b.labels[n.Scope] {
		return nil, reject.Inputf("planner: duplicate scope %q", n.Scope)
	}
	if n.Binding != b.recursing {
		return nil, reject.Inputf("planner: recursive_ref to %q is legal only inside that binding's step", n.Binding)
	}
	bnd, ok := b.bindings[n.Binding]
	if !ok || !bnd.Recursive {
		return nil, reject.Inputf("planner: recursive_ref names %q, which is not a recursive binding", n.Binding)
	}
	out := bnd.Out // the provisional anchor shape
	fields := make([]lir.Field, len(out.Fields))
	canon := make([]lir.SlotID, len(out.Fields))
	for i, f := range out.Fields {
		fields[i] = lir.Field{Name: f.Name, Slot: b.nextSlot, Type: f.Type}
		canon[i] = f.Slot
		b.nextSlot++
	}
	r := bound.NewRecursiveRef(n.Binding, n.Scope, fields, canon)
	b.labels[n.Scope] = true
	b.scopes = append(b.scopes, scopeEntry{label: n.Scope, rel: r})
	return r, nil
}

// bindRecursiveBinding binds a recursively-defined binding. The anchor
// supplies the public shape; the step binds against that provisional shape
// through recursive_ref, then the two are reconciled — kinds must match,
// nullability is the join. Errors are name-free; the caller adds the binding
// name.
func (b *binder) bindRecursiveBinding(name string, rec lir.Recursive) (*bound.Binding, error) {
	if err := validateRecursive(name, rec); err != nil {
		return nil, err
	}

	anchor, err := b.bindRel(rec.Anchor)
	b.scopes = b.scopes[:0]
	if err != nil {
		return nil, err
	}
	if err := recursiveOutputUnique(anchor.Output()); err != nil {
		return nil, err
	}

	// Register provisionally so the step's recursive_ref resolves against the
	// anchor's shape before the reconciled shape exists.
	bnd := &bound.Binding{
		Name:         name,
		Root:         anchor,
		Out:          anchor.Output(),
		Recursive:    true,
		Accumulation: rec.Accumulation,
	}
	b.bindings[name] = bnd

	// The step's output nullability can depend on the frontier's — a column
	// derived from another that widens across iterations — so the public
	// signature is a fixpoint, not one reconcile. Bind the step against the
	// current signature, join anchor and step nullability, and re-bind against
	// the widened signature until it stops changing. Re-binding from the same
	// slot and label marks is deterministic, so the final pass's slots are the
	// ones the planner sees; kinds never change and nullability only widens
	// over a finite lattice, so this terminates in at most one pass per column.
	prev := b.recursing
	slotMark := b.nextSlot
	labelMark := maps.Clone(b.labels)
	var step bound.Relation
	for {
		b.nextSlot = slotMark
		b.labels = maps.Clone(labelMark)
		b.scopes = b.scopes[:0]

		b.recursing = name
		s, err := b.bindRel(rec.Step)
		b.recursing = prev
		if err != nil {
			return nil, err
		}
		widened, err := reconcileRecursive(anchor.Output(), s.Output())
		if err != nil {
			return nil, err
		}
		step = s
		stable := rowTypeEqual(widened, bnd.Out)
		bnd.Out = widened
		if stable {
			break
		}
	}
	b.scopes = b.scopes[:0]

	bnd.Step = step
	bnd.PlanSensitive = bound.PlanSensitive(anchor) || bound.PlanSensitive(step)
	return bnd, nil
}

// rowTypeEqual reports whether two row types have the same columns with the
// same kinds and nullability — the fixpoint-stability test for the recursive
// signature.
func rowTypeEqual(a, b lir.RowType) bool {
	if len(a.Fields) != len(b.Fields) {
		return false
	}
	for i := range a.Fields {
		x, y := a.Fields[i], b.Fields[i]
		if x.Name != y.Name || x.Type.Kind != y.Type.Kind || x.Type.Nullable != y.Type.Nullable {
			return false
		}
	}
	return true
}

// recursiveOutputUnique rejects a recursive anchor whose output has colliding
// column names — the anchor's shape is the binding's public contract.
func recursiveOutputUnique(out lir.RowType) error {
	seen := map[string]bool{}
	for _, f := range out.Fields {
		if seen[f.Name] {
			return reject.Inputf("planner: recursive anchor output has duplicate column %q — project it to a unique set of columns", f.Name)
		}
		seen[f.Name] = true
	}
	return nil
}

// reconcileRecursive checks the step's output against the anchor's: the same
// columns, each with the same kind, taking the nullability join so a column
// any branch can null is nullable in the binding's public shape.
func reconcileRecursive(anchor, step lir.RowType) (lir.RowType, error) {
	if len(anchor.Fields) != len(step.Fields) {
		return lir.RowType{}, reject.Inputf("planner: recursive anchor produces %d columns but the step produces %d — anchor and step must produce the same columns", len(anchor.Fields), len(step.Fields))
	}
	out := make([]lir.Field, len(anchor.Fields))
	for i, af := range anchor.Fields {
		sf, ok := step.Lookup(af.Name)
		if !ok {
			return lir.RowType{}, reject.Inputf("planner: recursive step is missing anchor column %q — anchor and step must produce the same columns", af.Name)
		}
		if sf.Type.Kind != af.Type.Kind {
			return lir.RowType{}, reject.Inputf("planner: recursive column %q is %s in the anchor but %s in the step — the kinds must match", af.Name, af.Type.Kind, sf.Type.Kind)
		}
		f := af
		f.Type.Nullable = af.Type.Nullable || sf.Type.Nullable
		out[i] = f
	}
	return lir.RowType{Fields: out}, nil
}

// validateRecursive enforces a recursive binding's structural well-formedness
// on the unbound trees: the anchor is self-reference-free, and the step
// contains exactly one recursive_ref to this binding in a monotone position.
// Errors are name-free; the caller adds the binding name.
func validateRecursive(name string, rec lir.Recursive) error {
	if err := checkRecursiveAnchor(name, rec.Anchor); err != nil {
		return err
	}
	count := 0
	if err := checkRecursiveStep(name, rec.Step, false, &count); err != nil {
		return err
	}
	switch {
	case count == 0:
		return reject.Inputf("planner: recursive step contains no recursive_ref — the step must reference the binding to recurse")
	case count > 1:
		return reject.Inputf("planner: recursive step contains %d recursive_refs — v0 allows exactly one (linear recursion)", count)
	}
	return nil
}

// checkRecursiveAnchor rejects any recursive_ref (the anchor is the base case)
// and any ordinary ref back to the binding under definition.
func checkRecursiveAnchor(name string, r lir.Relation) error {
	var walk func(lir.Relation) error
	var expr func(lir.Expr) error
	walk = func(r lir.Relation) error {
		switch n := r.(type) {
		case lir.RecursiveRef:
			return reject.Inputf("planner: recursive anchor contains a recursive_ref — the anchor is the base case and must not recurse")
		case lir.Ref:
			if n.Binding == name {
				return reject.Inputf("planner: recursive anchor references the binding through an ordinary ref — the base case cannot observe it")
			}
		case lir.Recursive:
			return reject.Inputf("planner: a recursive relation is only valid as a binding body")
		case lir.Filter:
			if err := walk(n.Input); err != nil {
				return err
			}
			return expr(n.Pred)
		case lir.Project:
			if err := walk(n.Input); err != nil {
				return err
			}
			for _, f := range n.Fields {
				if err := expr(f.Expr); err != nil {
					return err
				}
			}
		case lir.Join:
			if err := walk(n.Left); err != nil {
				return err
			}
			if err := walk(n.Right); err != nil {
				return err
			}
			return expr(n.On)
		case lir.Aggregate:
			if err := walk(n.Input); err != nil {
				return err
			}
			for _, g := range n.Groups {
				if err := expr(g.Expr); err != nil {
					return err
				}
			}
			for _, t := range n.Terms {
				if err := expr(t.Arg); err != nil {
					return err
				}
			}
		case lir.Order:
			if err := walk(n.Input); err != nil {
				return err
			}
			for _, t := range n.Terms {
				if err := expr(t.Expr); err != nil {
					return err
				}
			}
		case lir.Slice:
			return walk(n.Input)
		}
		return nil
	}
	expr = func(e lir.Expr) error {
		switch x := e.(type) {
		case lir.Unary:
			return expr(x.X)
		case lir.Binary:
			if err := expr(x.L); err != nil {
				return err
			}
			return expr(x.R)
		case lir.Cast:
			return expr(x.X)
		case lir.Exists:
			return walk(x.Rel)
		case lir.First:
			return walk(x.Rel)
		case lir.Scalar:
			return walk(x.Rel)
		case lir.Array:
			return walk(x.Rel)
		}
		return nil
	}
	return walk(r)
}

// checkRecursiveStep walks the step: recursive_ref must target this binding,
// appear in a monotone position (forbidden under an aggregate, slice, the
// nullable side of a left join, or a crossing), and there must be no ordinary
// ref back to this binding. It counts recursive_ref occurrences through count.
func checkRecursiveStep(name string, r lir.Relation, forbidden bool, count *int) error {
	switch n := r.(type) {
	case lir.RecursiveRef:
		if n.Binding != name {
			return reject.Inputf("planner: recursive_ref names a different binding %q — v0 does not allow mutual recursion", n.Binding)
		}
		if forbidden {
			return reject.Inputf("planner: recursive_ref appears in a non-monotone position (under an aggregate, slice, the nullable side of a left join, or a crossing) — the step must be monotone in the frontier")
		}
		*count++
		return nil
	case lir.Ref:
		if n.Binding == name {
			return reject.Inputf("planner: the step observes the binding through an ordinary ref — use recursive_ref for the frontier; the completed value is only observable outside")
		}
		return nil
	case lir.Recursive:
		return reject.Inputf("planner: a recursive relation is only valid as a binding body")
	case lir.Filter:
		if err := checkRecursiveStep(name, n.Input, forbidden, count); err != nil {
			return err
		}
		return checkRecursiveStepExpr(name, n.Pred)
	case lir.Project:
		if err := checkRecursiveStep(name, n.Input, forbidden, count); err != nil {
			return err
		}
		for _, f := range n.Fields {
			if err := checkRecursiveStepExpr(name, f.Expr); err != nil {
				return err
			}
		}
		return nil
	case lir.Join:
		if err := checkRecursiveStep(name, n.Left, forbidden, count); err != nil {
			return err
		}
		if err := checkRecursiveStep(name, n.Right, forbidden || n.Kind == lir.LeftJoin, count); err != nil {
			return err
		}
		return checkRecursiveStepExpr(name, n.On)
	case lir.Aggregate:
		if err := checkRecursiveStep(name, n.Input, true, count); err != nil {
			return err
		}
		for _, g := range n.Groups {
			if err := checkRecursiveStepExpr(name, g.Expr); err != nil {
				return err
			}
		}
		for _, t := range n.Terms {
			if err := checkRecursiveStepExpr(name, t.Arg); err != nil {
				return err
			}
		}
		return nil
	case lir.Order:
		if err := checkRecursiveStep(name, n.Input, forbidden, count); err != nil {
			return err
		}
		for _, t := range n.Terms {
			if err := checkRecursiveStepExpr(name, t.Expr); err != nil {
				return err
			}
		}
		return nil
	case lir.Slice:
		return checkRecursiveStep(name, n.Input, true, count)
	}
	return nil
}

// checkRecursiveStepExpr walks a step expression: a recursive_ref inside a
// crossing is a non-monotone use, so a crossing is always forbidden context.
func checkRecursiveStepExpr(name string, e lir.Expr) error {
	switch x := e.(type) {
	case lir.Unary:
		return checkRecursiveStepExpr(name, x.X)
	case lir.Binary:
		if err := checkRecursiveStepExpr(name, x.L); err != nil {
			return err
		}
		return checkRecursiveStepExpr(name, x.R)
	case lir.Cast:
		return checkRecursiveStepExpr(name, x.X)
	case lir.Exists:
		return checkRecursiveCrossing(name, x.Rel)
	case lir.First:
		return checkRecursiveCrossing(name, x.Rel)
	case lir.Scalar:
		return checkRecursiveCrossing(name, x.Rel)
	case lir.Array:
		return checkRecursiveCrossing(name, x.Rel)
	}
	return nil
}

func checkRecursiveCrossing(name string, r lir.Relation) error {
	var ignored int
	return checkRecursiveStep(name, r, true, &ignored)
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

// -
// uniqueness-aware cardinality
// -

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

// -
// order tie-breaker
// -

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
	case *bound.Distinct:
		// After distinct, the complete output row is unique, so all columns
		// together form a key — an order over them all is total.
		return n.Output().Fields
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
