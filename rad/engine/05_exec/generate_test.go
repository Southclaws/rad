package exec

// Typed, schema-aware LIR generation feeding the differential oracle
// (step 4/5 of the oracle ADR). For each seed we generate a random catalog,
// random data, and a bind-valid query correct-by-construction, then run the
// same query three ways — the engine's chosen plan, the same query forced to
// full scans, and the naive reference interpreter — and require they agree.
//
// The generator is typed: it tracks each relation's output schema as it builds
// and only emits legal children (typed literals, unique output names, an order
// where one is required, join sides that can't see each other). It never emits
// arbitrary JSON hoping to discover validation errors — a bind failure here is
// a generator bug, surfaced as a hard failure with the reproducing seed.
//
// Results compare as multisets: tie order is path-dependent (the binder appends
// a unique-key tie-breaker the interpreter does not) and is covered by the
// conformance fixtures and path-independence instead. What this catches is the
// relational layer — row selection, join matching/padding, grouping/folds,
// projection shaping — independent of physical plan choice.

import (
	"context"
	"encoding/json"
	"fmt"
	"math/rand"
	"os"
	"strconv"
	"testing"

	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// generatedSeeds is how many random cases the differential explores. Opening a
// fresh store per seed dominates the cost, so the subtests run in parallel; the
// count is env-tunable for a soak run (RAD_GEN_SEEDS=100000 overnight), with a
// modest default so `go test ./...` stays quick.
func generatedSeeds() int {
	if s := os.Getenv("RAD_GEN_SEEDS"); s != "" {
		if n, err := strconv.Atoi(s); err == nil && n > 0 {
			return n
		}
	}
	return 200
}

func TestGeneratedDifferential(t *testing.T) {
	t.Parallel()
	for i := 0; i < generatedSeeds(); i++ {
		seed := int64(i)
		t.Run(fmt.Sprint(seed), func(t *testing.T) {
			t.Parallel()
			rng := rand.New(rand.NewSource(seed))
			spec := genCatalogSpec(rng)
			eng, ctx := buildGenEngine(t, rng, spec)

			g := &gen{rng: rng, cat: spec}
			q := g.genQuery()

			chosen, errC := eng.Execute(ctx, q)
			oracle, errO := interpQuery(ctx, eng, q)
			forced, errF := executeFullScan(ctx, eng, q)

			// The differential contract: all three either succeed or fail
			// together. A bind failure (all three fail identically at bind) is a
			// generator bug; a split — one path errors, another doesn't — is a
			// real engine divergence.
			if (errC != nil) != (errO != nil) || (errC != nil) != (errF != nil) {
				t.Fatalf("error split: chosen=%v oracle=%v forced=%v\nseed %d query:\n%s",
					errC, errO, errF, seed, litQuery(q))
			}
			if errC != nil {
				t.Fatalf("all paths errored (generator likely emitted invalid LIR): %v\nseed %d query:\n%s",
					errC, seed, litQuery(q))
			}

			ms := multiset(chosen)
			if o := multiset(oracle); !sameMultiset(ms, o) {
				t.Fatalf("engine ≠ oracle\n engine: %v\n oracle: %v\nseed %d query:\n%s",
					ms, o, seed, litQuery(q))
			}
			if f := multiset(forced); !sameMultiset(ms, f) {
				t.Fatalf("chosen plan ≠ full-scan plan (path-dependent result)\n chosen: %v\n forced: %v\nseed %d query:\n%s",
					ms, f, seed, litQuery(q))
			}
		})
	}
}

// ── the catalog spec ────────────────────────────────────────────────────────

type genColumn struct {
	name     string
	typ      catalog.Type
	nullable bool
}

type genTable struct {
	name     string
	cols     []genColumn // cols[0] is always the "id" text PK
	indexOn  string      // "" or a column name to index (non-unique)
	fkCol    string      // "" or the name of a nullable text FK column (also in cols)
	fkParent string      // parent table name for fkCol
}

type genCatalog struct{ tables []genTable }

var scalarTypes = []catalog.Type{catalog.TypeText, catalog.TypeInt64, catalog.TypeFloat64, catalog.TypeBool}

func (t genTable) col(name string) (genColumn, bool) {
	for _, c := range t.cols {
		if c.name == name {
			return c, true
		}
	}
	return genColumn{}, false
}

func genCatalogSpec(rng *rand.Rand) *genCatalog {
	n := 2 + rng.Intn(3) // 2..4 tables
	cat := &genCatalog{}
	for i := 0; i < n; i++ {
		tbl := genTable{name: fmt.Sprintf("t%d", i)}
		tbl.cols = append(tbl.cols, genColumn{name: "id", typ: catalog.TypeText})
		extra := 1 + rng.Intn(3) // 1..3 value columns
		for j := 0; j < extra; j++ {
			tbl.cols = append(tbl.cols, genColumn{
				name:     fmt.Sprintf("c%d", j),
				typ:      scalarTypes[rng.Intn(len(scalarTypes))],
				nullable: rng.Intn(2) == 0,
			})
		}
		// A nullable FK to an earlier table lets joins/grouping find matches.
		if i > 0 && rng.Intn(2) == 0 {
			parent := cat.tables[rng.Intn(i)]
			tbl.fkCol = "fk"
			tbl.fkParent = parent.name
			tbl.cols = append(tbl.cols, genColumn{name: "fk", typ: catalog.TypeText, nullable: true})
		}
		// A non-unique secondary index gives the planner a range-scan option
		// (exercises path-independence) without risking insert rejection.
		if extra > 0 && rng.Intn(2) == 0 {
			tbl.indexOn = tbl.cols[1+rng.Intn(extra)].name
		}
		cat.tables = append(cat.tables, tbl)
	}
	return cat
}

// ── building a real engine from the spec ────────────────────────────────────

func buildGenEngine(t *testing.T, rng *rand.Rand, spec *genCatalog) (*Engine, context.Context) {
	t.Helper()
	ctx := context.Background()
	store, err := kvslate.Open("gen-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })
	cat := catalog.New(store)

	for _, tbl := range spec.tables {
		def := catalog.TableDef{Name: tbl.name, PrimaryKey: []string{"id"}}
		for _, c := range tbl.cols {
			def.Columns = append(def.Columns, catalog.ColumnDef{Name: c.name, Type: c.typ, Nullable: c.nullable})
		}
		if tbl.indexOn != "" {
			def.Indexes = []catalog.IndexDef{{Name: tbl.name + "_" + tbl.indexOn + "_idx", Columns: []string{tbl.indexOn}}}
		}
		if tbl.fkCol != "" {
			def.ForeignKeys = []catalog.ForeignKeyDef{{
				Name: tbl.name + "_fk", Columns: []string{tbl.fkCol},
				RefTable: tbl.fkParent, RefColumns: []string{"id"}}}
		}
		if _, err := cat.CreateTable(ctx, def); err != nil {
			t.Fatalf("create table %q: %v", tbl.name, err)
		}
	}

	eng := New(store, cat)
	ids := map[string][]string{} // table -> inserted ids, for FK refs
	for _, tbl := range spec.tables {
		rows := rng.Intn(6) // 0..5 rows, sometimes empty
		for i := 0; i < rows; i++ {
			row := lir.Row{}
			for _, c := range tbl.cols {
				switch {
				case c.name == "id":
					row["id"] = lir.Text(fmt.Sprintf("%s_%d", tbl.name, i))
				case c.name == tbl.fkCol:
					parents := ids[tbl.fkParent]
					if len(parents) == 0 || rng.Intn(3) == 0 {
						row[c.name] = lir.Null(catalog.TypeText)
					} else {
						row[c.name] = lir.Text(parents[rng.Intn(len(parents))])
					}
				default:
					row[c.name] = genValue(rng, c)
				}
			}
			if err := eng.Insert(ctx, tbl.name, row); err != nil {
				t.Fatalf("insert into %q: %v", tbl.name, err)
			}
			ids[tbl.name] = append(ids[tbl.name], fmt.Sprintf("%s_%d", tbl.name, i))
		}
	}
	return eng, ctx
}

func genValue(rng *rand.Rand, c genColumn) lir.Value {
	if c.nullable && rng.Intn(10) < 3 {
		return lir.Null(c.typ)
	}
	switch c.typ {
	case catalog.TypeText:
		return lir.Text([]string{"a", "b", "c", ""}[rng.Intn(4)])
	case catalog.TypeInt64:
		return lir.Int64([]int64{-2, -1, 0, 1, 2, 100}[rng.Intn(6)])
	case catalog.TypeFloat64:
		return lir.Float64([]float64{-1.5, 0, 1.5, 2.5}[rng.Intn(4)])
	default:
		return lir.Bool(rng.Intn(2) == 0)
	}
}

// ── query generation (correct-by-construction) ──────────────────────────────

// genScope is one visible output scope: a label plus its typed columns.
type genScope struct {
	name string
	cols []genColumn
}

type gen struct {
	rng    *rand.Rand
	cat    *genCatalog
	scopeN int
	fieldN int
}

func (g *gen) fresh() string     { g.scopeN++; return fmt.Sprintf("s%d", g.scopeN) }
func (g *gen) field() string     { g.fieldN++; return fmt.Sprintf("f%d", g.fieldN) }
func (g *gen) chance(n int) bool { return g.rng.Intn(n) == 0 }

func (g *gen) genQuery() lir.Query {
	rel, scopes := g.genRel(3)
	// Flatten every visible scope into one uniquely-named output so the root
	// object never has colliding attribute names.
	var fields []lir.ProjField
	for _, s := range scopes {
		for _, c := range s.cols {
			fields = append(fields, lir.ProjField{As: g.field(), Expr: qcol(s.name, c.name)})
		}
	}
	// Optionally attach correlated crossings (includes / correlated folds) to
	// the output. Their sub-relations reference the visible scopes, so this
	// exercises correlation — and because the reference interpreter evaluates
	// crossings per-row nested while the engine batches them, the differential
	// is also the batched-≡-nested check.
	for k := 0; k < g.rng.Intn(3); k++ {
		fields = append(fields, g.genCrossingField(scopes))
	}
	flat := lir.Project{Input: rel, Scope: g.fresh(), Fields: fields}
	return many(flat)
}

// genCrossingField builds one output field whose value is a crossing over a
// sub-relation correlated with the visible outer scopes: exists (bool), a
// correlated count (scalar), the first matching row (object|null), or all
// matching rows (array, ordered by the child's unique id so engine and
// interpreter agree on element order).
func (g *gen) genCrossingField(outer []genScope) lir.ProjField {
	sub, subScope := g.genCorrelatedSub(outer)
	switch g.rng.Intn(4) {
	case 0:
		return lir.ProjField{As: g.field(), Expr: lir.Exists{Rel: sub}}
	case 1:
		agg := lir.Aggregate{Input: sub, Terms: []lir.AggTerm{{Fn: lir.AggCount, As: "n"}}}
		return lir.ProjField{As: g.field(), Expr: lir.Scalar{Rel: agg}}
	case 2:
		ord := lir.Order{Input: sub, Terms: []lir.OrderTerm{{Expr: qcol(subScope, "id")}}}
		return lir.ProjField{As: g.field(), Expr: lir.First{Rel: ord}}
	default:
		ord := lir.Order{Input: sub, Terms: []lir.OrderTerm{{Expr: qcol(subScope, "id")}}}
		return lir.ProjField{As: g.field(), Expr: lir.Array{Rel: ord}}
	}
}

// genCorrelatedSub builds a filtered scan correlated with an outer scope: a
// fresh scan whose filter compares one of its columns to an outer column of
// the same type. Every table has a text "id", and every outer scope has a text
// column, so a correlation is always available. Equality biases toward the
// key-correlated (batched) path; a range makes it general correlation.
func (g *gen) genCorrelatedSub(outer []genScope) (lir.Relation, string) {
	tbl := g.cat.tables[g.rng.Intn(len(g.cat.tables))]
	scope := g.fresh()
	sub := []genScope{{name: scope, cols: tbl.cols}}

	var corr lir.Expr = qlit(true)
	for _, typ := range shuffle(g.rng, scalarTypes) {
		sc, ss, sok := g.pickCol(sub, []catalog.Type{typ})
		oc, os, ook := g.pickCol(outer, []catalog.Type{typ})
		if sok && ook {
			op := lir.OpEq
			if typ != catalog.TypeBool && g.rng.Intn(3) == 0 {
				op = []lir.BinaryOp{lir.OpLt, lir.OpGt}[g.rng.Intn(2)]
			}
			corr = lir.Binary{Op: op, L: qcol(ss, sc.name), R: qcol(os, oc.name)}
			break
		}
	}
	return lir.Filter{Input: lir.Scan{Table: tbl.name, Scope: scope}, Pred: corr}, scope
}

func (g *gen) genRel(fuel int) (lir.Relation, []genScope) {
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

func (g *gen) genScan() (lir.Relation, []genScope) {
	tbl := g.cat.tables[g.rng.Intn(len(g.cat.tables))]
	scope := g.fresh()
	return lir.Scan{Table: tbl.name, Scope: scope}, []genScope{{name: scope, cols: tbl.cols}}
}

func (g *gen) genFilter(fuel int) (lir.Relation, []genScope) {
	child, scopes := g.genRel(fuel - 1)
	return lir.Filter{Input: child, Pred: g.genPred(scopes)}, scopes
}

func (g *gen) genOrder(fuel int) (lir.Relation, []genScope) {
	child, scopes := g.genRel(fuel - 1)
	var terms []lir.OrderTerm
	if c, s, ok := g.pickCol(scopes, orderable); ok {
		terms = append(terms, lir.OrderTerm{Expr: qcol(s, c.name), Desc: g.rng.Intn(2) == 0})
	} else {
		terms = append(terms, lir.OrderTerm{Expr: qlit(true)})
	}
	return lir.Order{Input: child, Terms: terms}, scopes
}

func (g *gen) genProject(fuel int) (lir.Relation, []genScope) {
	child, scopes := g.genRel(fuel - 1)
	scope := g.fresh()
	var fields []lir.ProjField
	var cols []genColumn
	// Re-expose a random non-empty subset of visible columns under new names.
	for _, s := range scopes {
		for _, c := range s.cols {
			if g.rng.Intn(2) == 0 {
				name := g.field()
				fields = append(fields, lir.ProjField{As: name, Expr: qcol(s.name, c.name)})
				cols = append(cols, genColumn{name: name, typ: c.typ, nullable: c.nullable})
			}
		}
	}
	if len(fields) == 0 { // projection needs at least one field
		c, s, _ := g.anyCol(scopes)
		name := g.field()
		fields = append(fields, lir.ProjField{As: name, Expr: qcol(s, c.name)})
		cols = append(cols, genColumn{name: name, typ: c.typ, nullable: c.nullable})
	}
	// Occasionally add a computed int64 field to exercise projection of a
	// non-column expression.
	if c, s, ok := g.pickCol(scopes, []catalog.Type{catalog.TypeInt64}); ok && g.chance(2) {
		name := g.field()
		expr := lir.Binary{Op: lir.OpAdd, L: qcol(s, c.name), R: qlit(1)}
		fields = append(fields, lir.ProjField{As: name, Expr: expr})
		cols = append(cols, genColumn{name: name, typ: catalog.TypeInt64, nullable: c.nullable})
	}
	return lir.Project{Input: child, Scope: scope, Fields: fields}, []genScope{{name: scope, cols: cols}}
}

func (g *gen) genJoin(fuel int) (lir.Relation, []genScope) {
	left, ls := g.genRel(fuel - 1)
	right, rs := g.genRel(fuel - 1)
	on := g.genJoinOn(ls, rs)
	kind := lir.InnerJoin
	if g.rng.Intn(2) == 0 {
		kind = lir.LeftJoin
	}
	return lir.Join{Left: left, Right: right, Kind: kind, On: on}, append(append([]genScope{}, ls...), rs...)
}

func (g *gen) genAggregate(fuel int) (lir.Relation, []genScope) {
	child, scopes := g.genRel(fuel - 1)
	scope := g.fresh()
	var groups []lir.GroupTerm
	var cols []genColumn
	// 0..2 group keys over comparable columns.
	for k := 0; k < g.rng.Intn(3); k++ {
		if c, s, ok := g.pickCol(scopes, orderable); ok {
			name := g.field()
			groups = append(groups, lir.GroupTerm{Expr: qcol(s, c.name), As: name})
			cols = append(cols, genColumn{name: name, typ: c.typ, nullable: c.nullable})
		}
	}
	var terms []lir.AggTerm
	countName := g.field()
	terms = append(terms, lir.AggTerm{Fn: lir.AggCount, As: countName})
	cols = append(cols, genColumn{name: countName, typ: catalog.TypeInt64})
	// An optional numeric fold.
	if c, s, ok := g.pickCol(scopes, []catalog.Type{catalog.TypeInt64, catalog.TypeFloat64}); ok && g.chance(2) {
		fn := []lir.AggFn{lir.AggSum, lir.AggMin, lir.AggMax, lir.AggAvg}[g.rng.Intn(4)]
		name := g.field()
		terms = append(terms, lir.AggTerm{Fn: fn, Arg: qcol(s, c.name), As: name})
		typ := c.typ
		if fn == lir.AggAvg {
			typ = catalog.TypeFloat64
		}
		cols = append(cols, genColumn{name: name, typ: typ, nullable: true})
	}
	return lir.Aggregate{Input: child, Scope: scope, Groups: groups, Terms: terms}, []genScope{{name: scope, cols: cols}}
}

// ── expression / column helpers ─────────────────────────────────────────────

var orderable = []catalog.Type{catalog.TypeText, catalog.TypeInt64, catalog.TypeFloat64}

// genPred builds a boolean predicate anchored on a column (so the literal side
// is typed from context), optionally combined with a second via and/or.
func (g *gen) genPred(scopes []genScope) lir.Expr {
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

func (g *gen) genAtom(scopes []genScope) lir.Expr {
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
	col := qcol(s, c.name)
	if c.typ == catalog.TypeBool {
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
	return lir.Binary{Op: op, L: col, R: qlit(g.literal(c.typ))}
}

// genJoinOn joins on an equality between same-typed columns from each side, or
// a constant true (cross product) when the sides share no comparable type.
func (g *gen) genJoinOn(ls, rs []genScope) lir.Expr {
	for _, typ := range shuffle(g.rng, orderable) {
		lc, lsc, lok := g.pickCol(ls, []catalog.Type{typ})
		rc, rsc, rok := g.pickCol(rs, []catalog.Type{typ})
		if lok && rok {
			return qeq(qcol(lsc, lc.name), qcol(rsc, rc.name))
		}
	}
	return qlit(true)
}

// pickCol returns a random column (and its scope) whose type is in want.
func (g *gen) pickCol(scopes []genScope, want []catalog.Type) (genColumn, string, bool) {
	type hit struct {
		c genColumn
		s string
	}
	var hits []hit
	for _, s := range scopes {
		for _, c := range s.cols {
			for _, w := range want {
				if c.typ == w {
					hits = append(hits, hit{c, s.name})
				}
			}
		}
	}
	if len(hits) == 0 {
		return genColumn{}, "", false
	}
	h := hits[g.rng.Intn(len(hits))]
	return h.c, h.s, true
}

func (g *gen) anyCol(scopes []genScope) (genColumn, string, bool) {
	return g.pickCol(scopes, scalarTypes)
}

func (g *gen) literal(typ catalog.Type) any {
	switch typ {
	case catalog.TypeText:
		return []string{"a", "b", "c", ""}[g.rng.Intn(4)]
	case catalog.TypeInt64:
		return []int{-1, 0, 1, 2, 100}[g.rng.Intn(5)]
	case catalog.TypeFloat64:
		return []float64{-1.5, 0, 1.5, 2.5}[g.rng.Intn(4)]
	default:
		return g.rng.Intn(2) == 0
	}
}

func shuffle[T any](rng *rand.Rand, in []T) []T {
	out := append([]T{}, in...)
	rng.Shuffle(len(out), func(i, j int) { out[i], out[j] = out[j], out[i] })
	return out
}

// ── comparison ──────────────────────────────────────────────────────────────

// multiset renders an array-datum result as a count per canonical-JSON row, so
// results compare order-insensitively (tie order is covered elsewhere).
func multiset(d lir.Datum) map[string]int {
	m := map[string]int{}
	if d.Kind != lir.DatumArray {
		b, _ := json.Marshal(jsonish(d))
		m[string(b)]++
		return m
	}
	for _, e := range d.Elems {
		b, _ := json.Marshal(jsonish(e))
		m[string(b)]++
	}
	return m
}

func sameMultiset(a, b map[string]int) bool {
	if len(a) != len(b) {
		return false
	}
	for k, v := range a {
		if b[k] != v {
			return false
		}
	}
	return true
}

// litQuery renders a query for failure output; %#v is enough to reconstruct.
func litQuery(q lir.Query) string { return fmt.Sprintf("%#v", q) }
