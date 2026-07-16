package generative

import (
	"fmt"
	"math"
	"math/rand"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// orderable are the scalar types that carry a usable ordering for ORDER BY and
// group keys (bool is excluded from generated orderings, though Value.Compare
// does totally order it).
var orderable = []catalog.Type{catalog.TypeText, catalog.TypeInt64, catalog.TypeFloat64}

// genScope is one visible output scope: a label plus its typed columns.
type genScope struct {
	name string
	cols []Column
}

// genBinding is a declared query binding: a name and the output columns its
// body exposes, re-surfaced under each referencing scope.
type genBinding struct {
	name string
	cols []Column
}

// Generator synthesises correct-by-construction queries over a fixed catalog.
// It carries counters that hand out globally unique scope, field, and binding
// names so no synthesised relation collides with another.
type Generator struct {
	rng      *rand.Rand
	cat      *Catalog
	scopeN   int
	fieldN   int
	bindingN int
}

// NewGenerator builds a query generator over spec, drawing randomness from rng.
func NewGenerator(rng *rand.Rand, spec *Catalog) *Generator {
	return &Generator{rng: rng, cat: spec}
}

func (g *Generator) fresh() string     { g.scopeN++; return fmt.Sprintf("s%d", g.scopeN) }
func (g *Generator) field() string     { g.fieldN++; return fmt.Sprintf("f%d", g.fieldN) }
func (g *Generator) binding() string   { g.bindingN++; return fmt.Sprintf("b%d", g.bindingN) }
func (g *Generator) chance(n int) bool { return g.rng.Intn(n) == 0 }

// genBody generates the shared core: a few closed bindings, a relation tree,
// and a ref-join for each binding so every declared binding is referenced.
func (g *Generator) genBody() (lir.Relation, []genScope, map[string]lir.Relation) {
	// A binding's body is self-contained (its own scans, no outer refs) and
	// flattened to a unique-named output, since a ref exposes it under one scope
	// and the binding output must not collide.
	bindings := map[string]lir.Relation{}
	var binds []genBinding
	for k := 0; k < g.rng.Intn(3); k++ { // 0..2 bindings
		body, bscopes := g.genRel(2)
		flat, out := g.flattenScopes(body, bscopes)
		name := g.binding()
		bindings[name] = flat
		binds = append(binds, genBinding{name: name, cols: out.cols})
	}

	rel, scopes := g.genRel(3)

	// Every declared binding must be referenced at least once (bind-validity),
	// so join a fresh ref for each into the tree — sometimes twice, to exercise
	// commit-once with multiple occurrences (which drives the engine's
	// materialise-vs-replay choice; the interpreter commits once either way).
	for _, b := range binds {
		occ := 1
		if g.rng.Intn(2) == 0 {
			occ = 2
		}
		for i := 0; i < occ; i++ {
			rs := g.fresh()
			refScope := genScope{name: rs, cols: b.cols}
			kind := lir.InnerJoin
			if g.rng.Intn(2) == 0 {
				kind = lir.LeftJoin
			}
			rel = lir.Join{
				Left:  rel,
				Right: lir.Ref{Binding: b.name, Scope: rs},
				Kind:  kind,
				On:    g.genJoinOn(scopes, []genScope{refScope}),
			}
			scopes = append(scopes, refScope)
		}
	}
	return rel, scopes, bindings
}

// Query builds an unordered (bag) query for a multiset differential: it may
// carry correlated crossings in the output, and its result order is arbitrary.
func (g *Generator) Query() lir.Query {
	rel, scopes, bindings := g.genBody()

	// Flatten every visible scope into one uniquely-named output so the root
	// object never has colliding attribute names.
	var fields []lir.ProjField
	for _, s := range scopes {
		for _, c := range s.cols {
			fields = append(fields, lir.ProjField{As: g.field(), Expr: qcol(s.name, c.Name)})
		}
	}
	// Optionally attach correlated crossings (includes / correlated folds) to
	// the output. Their sub-relations reference the visible scopes, so this
	// exercises correlation — and because a reference interpreter evaluates
	// crossings per-row nested while the engine batches them, the differential
	// is also the batched-≡-nested check.
	for k := 0; k < g.rng.Intn(3); k++ {
		fields = append(fields, g.genCrossingField(scopes))
	}
	flat := lir.Project{Input: rel, Scope: g.fresh(), Fields: fields}

	q := many(flat)
	if len(bindings) > 0 {
		q.Bindings = bindings
	}
	return q
}

// OrderedQuery builds a query whose result is a deterministic sequence: it
// projects every (scalar) output column and orders by all of them (bool
// included — Value.Compare totally orders every scalar type). No crossing
// outputs, so every output column is a comparable scalar. Ordering by all
// output columns means the only ties are between genuinely identical rows,
// which render the same in any order — so the sequence is well-defined
// regardless of the tie-break the engine appends (and the interpreter doesn't).
// That is what lets a differential compare row *sequences*, catching ordering
// bugs a multiset comparison can't see.
func (g *Generator) OrderedQuery() lir.Query {
	rel, scopes, bindings := g.genBody()

	ps := g.fresh()
	var fields []lir.ProjField
	var order []lir.OrderTerm
	for _, s := range scopes {
		for _, c := range s.cols {
			name := g.field()
			fields = append(fields, lir.ProjField{As: name, Expr: qcol(s.name, c.Name)})
			order = append(order, lir.OrderTerm{Expr: qcol(ps, name), Desc: g.rng.Intn(2) == 0})
		}
	}
	// Every scope has at least one column, so fields and order are non-empty.
	flat := lir.Project{Input: rel, Scope: ps, Fields: fields}
	q := lir.Query{Card: lir.CardMany, Root: lir.Order{Input: flat, Terms: order}}
	if len(bindings) > 0 {
		q.Bindings = bindings
	}
	return q
}

// flattenScopes projects every column of every scope to a fresh unique name,
// returning the new single-scope relation and its output schema — the shape a
// binding body (and any observable boundary) needs so its columns don't
// collide when exposed under one scope.
func (g *Generator) flattenScopes(rel lir.Relation, scopes []genScope) (lir.Relation, genScope) {
	ps := g.fresh()
	var fields []lir.ProjField
	var cols []Column
	for _, s := range scopes {
		for _, c := range s.cols {
			name := g.field()
			fields = append(fields, lir.ProjField{As: name, Expr: qcol(s.name, c.Name)})
			cols = append(cols, Column{Name: name, Type: c.Type, Nullable: c.Nullable})
		}
	}
	return lir.Project{Input: rel, Scope: ps, Fields: fields}, genScope{name: ps, cols: cols}
}

// genCrossingField builds one output field whose value is a crossing over a
// sub-relation correlated with the visible outer scopes: exists (bool), a
// correlated count (scalar), the first matching row (object|null), or all
// matching rows (array, ordered by the child's unique id so engine and
// interpreter agree on element order).
func (g *Generator) genCrossingField(outer []genScope) lir.ProjField {
	sub, subScopes := g.genCorrelatedSub(outer)
	switch g.rng.Intn(4) {
	case 0:
		// exists renders a bool — the join body's columns are never shaped,
		// so a multi-scope (join) body is fine as-is.
		return lir.ProjField{As: g.field(), Expr: lir.Exists{Rel: sub}}
	case 1:
		// scalar over a count renders one value — likewise fine over a join.
		agg := lir.Aggregate{Input: sub, Terms: []lir.AggTerm{{Fn: lir.AggCount, As: "n"}}}
		return lir.ProjField{As: g.field(), Expr: lir.Scalar{Rel: agg}}
	case 2:
		return lir.ProjField{As: g.field(), Expr: lir.First{Rel: g.orderedSub(sub, subScopes)}}
	default:
		return lir.ProjField{As: g.field(), Expr: lir.Array{Rel: g.orderedSub(sub, subScopes)}}
	}
}

// orderedSub prepares a crossing body for `first`/`array`, which shape its
// rows into objects: it flattens every scope to a unique-named output (a join
// body would otherwise collide on shared names like "id") and orders by the
// projected id columns — a total unique key, so the selection is deterministic
// and engine/interpreter agree with no tie-break divergence.
func (g *Generator) orderedSub(sub lir.Relation, scopes []genScope) lir.Relation {
	ps := g.fresh()
	var fields []lir.ProjField
	var order []lir.OrderTerm
	for _, s := range scopes {
		for _, c := range s.cols {
			name := g.field()
			fields = append(fields, lir.ProjField{As: name, Expr: qcol(s.name, c.Name)})
			if c.Name == "id" {
				order = append(order, lir.OrderTerm{Expr: qcol(ps, name)})
			}
		}
	}
	return lir.Order{Input: lir.Project{Input: sub, Scope: ps, Fields: fields}, Terms: order}
}

// genCorrelatedSub builds a relation correlated with an outer scope: usually a
// filtered scan, but sometimes a filtered JOIN so a crossing's body can itself
// contain a join. The filter compares one of the sub's columns to an outer
// column of the same type; every table has a text "id" and every outer scope a
// text column, so a correlation is always available. Equality biases toward the
// key-correlated (batched) path; a range makes it general correlation.
func (g *Generator) genCorrelatedSub(outer []genScope) (lir.Relation, []genScope) {
	if g.rng.Intn(3) == 0 { // ~1/3: a correlated crossing over a join
		ta := g.cat.Tables[g.rng.Intn(len(g.cat.Tables))]
		tb := g.cat.Tables[g.rng.Intn(len(g.cat.Tables))]
		sa, sb := g.fresh(), g.fresh()
		scopes := []genScope{{name: sa, cols: ta.Columns}, {name: sb, cols: tb.Columns}}
		join := lir.Join{
			Left:  lir.Scan{Table: ta.Name, Scope: sa},
			Right: lir.Scan{Table: tb.Name, Scope: sb},
			Kind:  lir.InnerJoin,
			On:    g.genJoinOn(scopes[:1], scopes[1:]),
		}
		return lir.Filter{Input: join, Pred: g.correlate(scopes, outer)}, scopes
	}
	tbl := g.cat.Tables[g.rng.Intn(len(g.cat.Tables))]
	scope := g.fresh()
	sub := []genScope{{name: scope, cols: tbl.Columns}}
	return lir.Filter{Input: lir.Scan{Table: tbl.Name, Scope: scope}, Pred: g.correlate(sub, outer)}, sub
}

// correlate builds a predicate tying one of the sub's columns to an outer
// column of the same type — what makes the crossing correlated.
func (g *Generator) correlate(sub, outer []genScope) lir.Expr {
	for _, typ := range shuffle(g.rng, scalarTypes) {
		sc, ss, sok := g.pickCol(sub, []catalog.Type{typ})
		oc, os, ook := g.pickCol(outer, []catalog.Type{typ})
		if sok && ook {
			op := lir.OpEq
			if typ != catalog.TypeBool && g.rng.Intn(3) == 0 {
				op = []lir.BinaryOp{lir.OpLt, lir.OpGt}[g.rng.Intn(2)]
			}
			return lir.Binary{Op: op, L: qcol(ss, sc.Name), R: qcol(os, oc.Name)}
		}
	}
	return qlit(true)
}

func (g *Generator) genRel(fuel int) (lir.Relation, []genScope) {
	if fuel <= 0 || g.chance(3) {
		return g.genScan()
	}
	switch g.rng.Intn(5) {
	case 0:
		return g.genFilter(fuel)
	case 1:
		return g.genProject(fuel)
	case 2:
		return g.genOrder(fuel)
	case 3:
		return g.genJoin(fuel)
	default:
		return g.genAggregate(fuel)
	}
}

func (g *Generator) genScan() (lir.Relation, []genScope) {
	tbl := g.cat.Tables[g.rng.Intn(len(g.cat.Tables))]
	scope := g.fresh()
	return lir.Scan{Table: tbl.Name, Scope: scope}, []genScope{{name: scope, cols: tbl.Columns}}
}

func (g *Generator) genFilter(fuel int) (lir.Relation, []genScope) {
	child, scopes := g.genRel(fuel - 1)
	return lir.Filter{Input: child, Pred: g.genPred(scopes)}, scopes
}

func (g *Generator) genOrder(fuel int) (lir.Relation, []genScope) {
	child, scopes := g.genRel(fuel - 1)
	var terms []lir.OrderTerm
	if c, s, ok := g.pickCol(scopes, orderable); ok {
		terms = append(terms, lir.OrderTerm{Expr: qcol(s, c.Name), Desc: g.rng.Intn(2) == 0})
	} else {
		terms = append(terms, lir.OrderTerm{Expr: qlit(true)})
	}
	return lir.Order{Input: child, Terms: terms}, scopes
}

func (g *Generator) genProject(fuel int) (lir.Relation, []genScope) {
	child, scopes := g.genRel(fuel - 1)
	scope := g.fresh()
	var fields []lir.ProjField
	var cols []Column
	// Re-expose a random non-empty subset of visible columns under new names.
	for _, s := range scopes {
		for _, c := range s.cols {
			if g.rng.Intn(2) == 0 {
				name := g.field()
				fields = append(fields, lir.ProjField{As: name, Expr: qcol(s.name, c.Name)})
				cols = append(cols, Column{Name: name, Type: c.Type, Nullable: c.Nullable})
			}
		}
	}
	if len(fields) == 0 { // projection needs at least one field
		c, s, _ := g.anyCol(scopes)
		name := g.field()
		fields = append(fields, lir.ProjField{As: name, Expr: qcol(s, c.Name)})
		cols = append(cols, Column{Name: name, Type: c.Type, Nullable: c.Nullable})
	}
	// Occasionally add a computed int64 field to exercise projection of a
	// non-column expression.
	if c, s, ok := g.pickCol(scopes, []catalog.Type{catalog.TypeInt64}); ok && g.chance(2) {
		name := g.field()
		expr := lir.Binary{Op: lir.OpAdd, L: qcol(s, c.Name), R: qlit(1)}
		fields = append(fields, lir.ProjField{As: name, Expr: expr})
		cols = append(cols, Column{Name: name, Type: catalog.TypeInt64, Nullable: c.Nullable})
	}
	return lir.Project{Input: child, Scope: scope, Fields: fields}, []genScope{{name: scope, cols: cols}}
}

func (g *Generator) genJoin(fuel int) (lir.Relation, []genScope) {
	left, ls := g.genRel(fuel - 1)
	right, rs := g.genRel(fuel - 1)
	on := g.genJoinOn(ls, rs)
	kind := lir.InnerJoin
	if g.rng.Intn(2) == 0 {
		kind = lir.LeftJoin
	}
	return lir.Join{Left: left, Right: right, Kind: kind, On: on}, append(append([]genScope{}, ls...), rs...)
}

func (g *Generator) genAggregate(fuel int) (lir.Relation, []genScope) {
	child, scopes := g.genRel(fuel - 1)
	scope := g.fresh()
	var groups []lir.GroupTerm
	var cols []Column
	// 0..2 group keys over comparable columns.
	for k := 0; k < g.rng.Intn(3); k++ {
		if c, s, ok := g.pickCol(scopes, orderable); ok {
			name := g.field()
			groups = append(groups, lir.GroupTerm{Expr: qcol(s, c.Name), As: name})
			cols = append(cols, Column{Name: name, Type: c.Type, Nullable: c.Nullable})
		}
	}
	var terms []lir.AggTerm
	countName := g.field()
	terms = append(terms, lir.AggTerm{Fn: lir.AggCount, As: countName})
	cols = append(cols, Column{Name: countName, Type: catalog.TypeInt64})
	// An optional numeric fold.
	if c, s, ok := g.pickCol(scopes, []catalog.Type{catalog.TypeInt64, catalog.TypeFloat64}); ok && g.chance(2) {
		fn := []lir.AggFn{lir.AggSum, lir.AggMin, lir.AggMax, lir.AggAvg}[g.rng.Intn(4)]
		name := g.field()
		terms = append(terms, lir.AggTerm{Fn: fn, Arg: qcol(s, c.Name), As: name})
		typ := c.Type
		if fn == lir.AggAvg {
			typ = catalog.TypeFloat64
		}
		cols = append(cols, Column{Name: name, Type: typ, Nullable: true})
	}
	return lir.Aggregate{Input: child, Scope: scope, Groups: groups, Terms: terms}, []genScope{{name: scope, cols: cols}}
}

// genPred builds a boolean predicate anchored on a column (so the literal side
// is typed from context), optionally combined with a second via and/or.
func (g *Generator) genPred(scopes []genScope) lir.Expr {
	p := g.genAtom(scopes)
	if g.chance(3) {
		op := lir.OpAnd
		if g.rng.Intn(2) == 0 {
			op = lir.OpOr
		}
		return lir.Binary{Op: op, L: p, R: g.genAtom(scopes)}
	}
	return p
}

func (g *Generator) genAtom(scopes []genScope) lir.Expr {
	// Occasionally a correlated EXISTS — a crossing inside a filter predicate,
	// a distinct code path from a crossing in a projection field.
	if g.chance(4) {
		sub, _ := g.genCorrelatedSub(scopes)
		e := lir.Expr(lir.Exists{Rel: sub})
		if g.rng.Intn(2) == 0 {
			e = lir.Unary{Op: lir.OpNot, X: e}
		}
		return e
	}
	c, s, ok := g.anyCol(scopes)
	if !ok {
		return qlit(true)
	}
	col := qcol(s, c.Name)
	if c.Type == catalog.TypeBool {
		switch g.rng.Intn(3) {
		case 0:
			return col
		case 1:
			return lir.Unary{Op: lir.OpNot, X: col}
		default:
			return lir.Unary{Op: lir.OpIsNull, X: col}
		}
	}
	ops := []lir.BinaryOp{lir.OpEq, lir.OpNe, lir.OpLt, lir.OpLte, lir.OpGt, lir.OpGte}
	op := ops[g.rng.Intn(len(ops))]
	if g.chance(4) { // occasionally is_null instead of a comparison
		return lir.Unary{Op: lir.OpIsNull, X: col}
	}
	return lir.Binary{Op: op, L: col, R: qlit(g.literal(c.Type))}
}

// genJoinOn joins on an equality between same-typed columns from each side, or
// a constant true (cross product) when the sides share no comparable type.
func (g *Generator) genJoinOn(ls, rs []genScope) lir.Expr {
	for _, typ := range shuffle(g.rng, orderable) {
		lc, lsc, lok := g.pickCol(ls, []catalog.Type{typ})
		rc, rsc, rok := g.pickCol(rs, []catalog.Type{typ})
		if lok && rok {
			return qeq(qcol(lsc, lc.Name), qcol(rsc, rc.Name))
		}
	}
	return qlit(true)
}

// pickCol returns a random column (and its scope) whose type is in want.
func (g *Generator) pickCol(scopes []genScope, want []catalog.Type) (Column, string, bool) {
	type hit struct {
		c Column
		s string
	}
	var hits []hit
	for _, s := range scopes {
		for _, c := range s.cols {
			for _, w := range want {
				if c.Type == w {
					hits = append(hits, hit{c, s.name})
				}
			}
		}
	}
	if len(hits) == 0 {
		return Column{}, "", false
	}
	h := hits[g.rng.Intn(len(hits))]
	return h.c, h.s, true
}

func (g *Generator) anyCol(scopes []genScope) (Column, string, bool) {
	return g.pickCol(scopes, scalarTypes)
}

func (g *Generator) literal(typ catalog.Type) any {
	switch typ {
	case catalog.TypeText:
		return []string{"a", "b", "c", ""}[g.rng.Intn(4)]
	case catalog.TypeInt64:
		return []int{math.MinInt64, -1, 0, 1, 2, 100, math.MaxInt64}[g.rng.Intn(7)]
	case catalog.TypeFloat64:
		return []float64{-1.5, 0, 1.5, 2.5}[g.rng.Intn(4)]
	default:
		return g.rng.Intn(2) == 0
	}
}
