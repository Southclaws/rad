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
	"math"
	"math/rand"
	"os"
	"sort"
	"strconv"
	"testing"

	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
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
			q := (&gen{rng: rng, cat: spec}).genQuery()

			chosen, oracle, forced, ok := runThreeWays(t, ctx, eng, q, seed)
			if !ok {
				return
			}
			// Unordered (bag) result: compare as a multiset — order is arbitrary.
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

// TestGeneratedDifferentialOrdered is the sequence-comparing sibling: it
// generates queries with a total output order (see genOrderedQuery) and
// compares the row *sequence* exactly, so an ordering bug (Sort, ordered-index
// pushdown) that a multiset comparison would miss shows up as a divergence.
func TestGeneratedDifferentialOrdered(t *testing.T) {
	t.Parallel()
	for i := 0; i < generatedSeeds(); i++ {
		seed := int64(i)
		t.Run(fmt.Sprint(seed), func(t *testing.T) {
			t.Parallel()
			rng := rand.New(rand.NewSource(seed))
			spec := genCatalogSpec(rng)
			eng, ctx := buildGenEngine(t, rng, spec)
			q := (&gen{rng: rng, cat: spec}).genOrderedQuery()

			chosen, oracle, forced, ok := runThreeWays(t, ctx, eng, q, seed)
			if !ok {
				return
			}
			cj := seqJSON(chosen)
			if oj := seqJSON(oracle); cj != oj {
				t.Fatalf("engine ≠ oracle (row sequence)\n engine: %s\n oracle: %s\nseed %d query:\n%s",
					cj, oj, seed, litQuery(q))
			}
			if fj := seqJSON(forced); cj != fj {
				t.Fatalf("chosen plan ≠ full-scan plan (ordering depends on access path)\n chosen: %s\n forced: %s\nseed %d query:\n%s",
					cj, fj, seed, litQuery(q))
			}
		})
	}
}

// runThreeWays executes q via the engine's chosen plan, the reference
// interpreter, and a forced full-scan plan, enforcing the all-fail contract:
// all three succeed or all fail together (a split is a real divergence). It
// returns compare=false when all three consistently errored — a legitimate
// runtime error (e.g. checked overflow), distinguished from the generator
// emitting un-bindable LIR by an explicit bind check.
func runThreeWays(t *testing.T, ctx context.Context, eng *Engine, q lir.Query, seed int64) (chosen, oracle, forced lir.Datum, compare bool) {
	t.Helper()
	chosen, errC := eng.Execute(ctx, q)
	oracle, errO := interpQuery(ctx, eng, q)
	forced, errF := executeFullScan(ctx, eng, q)
	if (errC != nil) != (errO != nil) || (errC != nil) != (errF != nil) {
		t.Fatalf("error split: chosen=%v oracle=%v forced=%v\nseed %d query:\n%s",
			errC, errO, errF, seed, litQuery(q))
	}
	if errC != nil {
		if _, berr := planner.Bind(ctx, eng.cat, q); berr != nil {
			t.Fatalf("generator emitted un-bindable LIR: %v\nseed %d query:\n%s",
				berr, seed, litQuery(q))
		}
		return lir.Datum{}, lir.Datum{}, lir.Datum{}, false
	}
	return chosen, oracle, forced, true
}

// seqJSON renders a datum as canonical JSON, order-sensitively (array order
// preserved), for exact row-sequence comparison.
func seqJSON(d lir.Datum) string {
	b, _ := json.Marshal(jsonish(d))
	return string(b)
}

// TestGeneratorCoverage audits the generator's reach: it generates many
// queries (pure generation, no engine) and tallies which constructs and
// compositions actually appear, so we don't fool ourselves with a suite that
// technically supports a construct but almost never reaches it. It prints the
// full distribution and fails if any construct the generator is supposed to
// emit drops below a floor (a regression guard on the generator itself).
func TestGeneratorCoverage(t *testing.T) {
	const n = 2000
	counts := map[string]int{}
	for i := 0; i < n; i++ {
		rng := rand.New(rand.NewSource(int64(i)))
		spec := genCatalogSpec(rng)
		g := &gen{rng: rng, cat: spec}
		for f := range queryFeatures(g.genQuery()) {
			counts[f]++
		}
	}

	keys := make([]string, 0, len(counts))
	for k := range counts {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	t.Logf("generator feature coverage over %d seeds:", n)
	for _, k := range keys {
		t.Logf("  %-22s %5d", k, counts[k])
	}

	// Constructs the generator is meant to emit must appear regularly. Known
	// gaps (not yet generated) are asserted absent below so the list stays
	// honest — flip them to `mustHit` as they land.
	mustHit := []string{
		"scan", "filter", "project", "order",
		"join_inner", "join_left", "aggregate", "group_by", "global_aggregate",
		"exists", "first", "scalar", "array",
		"arithmetic", "is_null", "and_or", "not",
		"crossing", "correlated_aggregate", "crossing_over_join", "ref_binding",
	}
	const floor = 15
	for _, f := range mustHit {
		if counts[f] < floor {
			t.Errorf("under-explored: %q generated %d/%d times, want ≥ %d", f, counts[f], n, floor)
		}
	}
	// Documented gaps — constructs and compositions the generator does not emit
	// yet. Kept as explicit zero-assertions so the audit fails loudly the day
	// one starts appearing (promote it into mustHit) or a gap is deliberately
	// closed. The audit itself surfaced `crossing_over_join`: a correlated
	// crossing's body is always a filtered scan (or a global aggregate over
	// one), never a join, so deep compositions like a correlated array over a
	// join are unreached until the crossing sub-relation generator is enriched.
	for _, f := range []string{
		"rows", "cast", "slice", "nested_crossing",
	} {
		if counts[f] != 0 {
			t.Errorf("gap %q now appears (%d) — promote it into mustHit", f, counts[f])
		}
	}
}

// queryFeatures walks an unbound query and records which constructs and
// compositions it contains — the structure-aware coverage signal.
func queryFeatures(q lir.Query) map[string]bool {
	f := map[string]bool{}
	walkRelFeat(q.Root, f, false)
	for _, b := range q.Bindings {
		walkRelFeat(b, f, false)
	}
	return f
}

func walkRelFeat(r lir.Relation, f map[string]bool, inCross bool) {
	switch n := r.(type) {
	case lir.Scan:
		f["scan"] = true
	case lir.Rows:
		f["rows"] = true
	case lir.Ref:
		f["ref_binding"] = true
	case lir.Filter:
		f["filter"] = true
		walkExprFeat(n.Pred, f, inCross)
		walkRelFeat(n.Input, f, inCross)
	case lir.Project:
		f["project"] = true
		for _, fld := range n.Fields {
			walkExprFeat(fld.Expr, f, inCross)
		}
		walkRelFeat(n.Input, f, inCross)
	case lir.Join:
		if n.Kind == lir.LeftJoin {
			f["join_left"] = true
		} else {
			f["join_inner"] = true
		}
		walkExprFeat(n.On, f, inCross)
		walkRelFeat(n.Left, f, inCross)
		walkRelFeat(n.Right, f, inCross)
	case lir.Order:
		f["order"] = true
		for _, t := range n.Terms {
			walkExprFeat(t.Expr, f, inCross)
		}
		walkRelFeat(n.Input, f, inCross)
	case lir.Slice:
		f["slice"] = true
		walkRelFeat(n.Input, f, inCross)
	case lir.Aggregate:
		f["aggregate"] = true
		if len(n.Groups) == 0 {
			f["global_aggregate"] = true
		} else {
			f["group_by"] = true
		}
		for _, g := range n.Groups {
			walkExprFeat(g.Expr, f, inCross)
		}
		for _, t := range n.Terms {
			if t.Arg != nil {
				walkExprFeat(t.Arg, f, inCross)
			}
		}
		walkRelFeat(n.Input, f, inCross)
	}
}

func walkExprFeat(e lir.Expr, f map[string]bool, inCross bool) {
	switch x := e.(type) {
	case lir.Unary:
		switch x.Op {
		case lir.OpIsNull, lir.OpIsNotNull:
			f["is_null"] = true
		case lir.OpNot:
			f["not"] = true
		case lir.OpNegate:
			f["arithmetic"] = true
		}
		walkExprFeat(x.X, f, inCross)
	case lir.Binary:
		switch x.Op {
		case lir.OpAdd, lir.OpSub, lir.OpMul, lir.OpDiv:
			f["arithmetic"] = true
		case lir.OpAnd, lir.OpOr:
			f["and_or"] = true
		}
		walkExprFeat(x.L, f, inCross)
		walkExprFeat(x.R, f, inCross)
	case lir.Cast:
		f["cast"] = true
		walkExprFeat(x.X, f, inCross)
	case lir.Exists:
		crossFeat(x.Rel, f, "exists", inCross)
	case lir.First:
		crossFeat(x.Rel, f, "first", inCross)
	case lir.Scalar:
		crossFeat(x.Rel, f, "scalar", inCross)
	case lir.Array:
		crossFeat(x.Rel, f, "array", inCross)
	}
}

func crossFeat(rel lir.Relation, f map[string]bool, kind string, inCross bool) {
	f["crossing"] = true
	f[kind] = true
	if inCross {
		f["nested_crossing"] = true
	}
	if relContains(rel, func(r lir.Relation) bool { _, ok := r.(lir.Aggregate); return ok }) {
		f["correlated_aggregate"] = true
	}
	if relContains(rel, func(r lir.Relation) bool { _, ok := r.(lir.Join); return ok }) {
		f["crossing_over_join"] = true
	}
	walkRelFeat(rel, f, true)
}

// relContains reports whether any relation node in the tree (not descending
// into crossing sub-expressions) satisfies pred.
func relContains(r lir.Relation, pred func(lir.Relation) bool) bool {
	if pred(r) {
		return true
	}
	switch n := r.(type) {
	case lir.Filter:
		return relContains(n.Input, pred)
	case lir.Project:
		return relContains(n.Input, pred)
	case lir.Order:
		return relContains(n.Input, pred)
	case lir.Slice:
		return relContains(n.Input, pred)
	case lir.Aggregate:
		return relContains(n.Input, pred)
	case lir.Join:
		return relContains(n.Left, pred) || relContains(n.Right, pred)
	}
	return false
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
		return lir.Int64([]int64{math.MinInt64, -2, -1, 0, 1, 2, 100, math.MaxInt64}[rng.Intn(8)])
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
	rng      *rand.Rand
	cat      *genCatalog
	scopeN   int
	fieldN   int
	bindingN int
}

func (g *gen) fresh() string     { g.scopeN++; return fmt.Sprintf("s%d", g.scopeN) }
func (g *gen) field() string     { g.fieldN++; return fmt.Sprintf("f%d", g.fieldN) }
func (g *gen) binding() string   { g.bindingN++; return fmt.Sprintf("b%d", g.bindingN) }
func (g *gen) chance(n int) bool { return g.rng.Intn(n) == 0 }

type genBinding struct {
	name string
	cols []genColumn // the binding's output columns, re-exposed under each ref's scope
}

// genBody generates the shared core: a few closed bindings, a relation tree,
// and a ref-join for each binding so every declared binding is referenced.
func (g *gen) genBody() (lir.Relation, []genScope, map[string]lir.Relation) {
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

// genQuery builds an unordered (bag) query for the multiset differential: it may
// carry correlated crossings in the output, and its result order is arbitrary.
func (g *gen) genQuery() lir.Query {
	rel, scopes, bindings := g.genBody()

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

	q := many(flat)
	if len(bindings) > 0 {
		q.Bindings = bindings
	}
	return q
}

// genOrderedQuery builds a query whose result is a deterministic sequence: it
// projects every (scalar) output column and orders by all of them (bool
// included — Value.Compare totally orders every scalar type). No crossing
// outputs, so every output column is a comparable scalar. Ordering by all
// output columns means the only ties are between genuinely identical rows,
// which render the same in any order — so the sequence is well-defined
// regardless of the tie-break the engine appends (and the interpreter doesn't).
// That is what lets the differential compare row *sequences*, catching ordering
// bugs the multiset mode can't see.
func (g *gen) genOrderedQuery() lir.Query {
	rel, scopes, bindings := g.genBody()

	ps := g.fresh()
	var fields []lir.ProjField
	var order []lir.OrderTerm
	for _, s := range scopes {
		for _, c := range s.cols {
			name := g.field()
			fields = append(fields, lir.ProjField{As: name, Expr: qcol(s.name, c.name)})
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
func (g *gen) flattenScopes(rel lir.Relation, scopes []genScope) (lir.Relation, genScope) {
	ps := g.fresh()
	var fields []lir.ProjField
	var cols []genColumn
	for _, s := range scopes {
		for _, c := range s.cols {
			name := g.field()
			fields = append(fields, lir.ProjField{As: name, Expr: qcol(s.name, c.name)})
			cols = append(cols, genColumn{name: name, typ: c.typ, nullable: c.nullable})
		}
	}
	return lir.Project{Input: rel, Scope: ps, Fields: fields}, genScope{name: ps, cols: cols}
}

// genCrossingField builds one output field whose value is a crossing over a
// sub-relation correlated with the visible outer scopes: exists (bool), a
// correlated count (scalar), the first matching row (object|null), or all
// matching rows (array, ordered by the child's unique id so engine and
// interpreter agree on element order).
func (g *gen) genCrossingField(outer []genScope) lir.ProjField {
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
func (g *gen) orderedSub(sub lir.Relation, scopes []genScope) lir.Relation {
	ps := g.fresh()
	var fields []lir.ProjField
	var order []lir.OrderTerm
	for _, s := range scopes {
		for _, c := range s.cols {
			name := g.field()
			fields = append(fields, lir.ProjField{As: name, Expr: qcol(s.name, c.name)})
			if c.name == "id" {
				order = append(order, lir.OrderTerm{Expr: qcol(ps, name)})
			}
		}
	}
	return lir.Order{Input: lir.Project{Input: sub, Scope: ps, Fields: fields}, Terms: order}
}

// genCorrelatedSub builds a relation correlated with an outer scope: usually a
// filtered scan, but sometimes a filtered JOIN so a crossing's body can itself
// contain a join (the deep-composition path the coverage audit found unreached).
// The filter compares one of the sub's columns to an outer column of the same
// type; every table has a text "id" and every outer scope a text column, so a
// correlation is always available. Equality biases toward the key-correlated
// (batched) path; a range makes it general correlation.
func (g *gen) genCorrelatedSub(outer []genScope) (lir.Relation, []genScope) {
	if g.rng.Intn(3) == 0 { // ~1/3: a correlated crossing over a join
		ta := g.cat.tables[g.rng.Intn(len(g.cat.tables))]
		tb := g.cat.tables[g.rng.Intn(len(g.cat.tables))]
		sa, sb := g.fresh(), g.fresh()
		scopes := []genScope{{name: sa, cols: ta.cols}, {name: sb, cols: tb.cols}}
		join := lir.Join{
			Left:  lir.Scan{Table: ta.name, Scope: sa},
			Right: lir.Scan{Table: tb.name, Scope: sb},
			Kind:  lir.InnerJoin,
			On:    g.genJoinOn(scopes[:1], scopes[1:]),
		}
		return lir.Filter{Input: join, Pred: g.correlate(scopes, outer)}, scopes
	}
	tbl := g.cat.tables[g.rng.Intn(len(g.cat.tables))]
	scope := g.fresh()
	sub := []genScope{{name: scope, cols: tbl.cols}}
	return lir.Filter{Input: lir.Scan{Table: tbl.name, Scope: scope}, Pred: g.correlate(sub, outer)}, sub
}

// correlate builds a predicate tying one of the sub's columns to an outer
// column of the same type — what makes the crossing correlated.
func (g *gen) correlate(sub, outer []genScope) lir.Expr {
	for _, typ := range shuffle(g.rng, scalarTypes) {
		sc, ss, sok := g.pickCol(sub, []catalog.Type{typ})
		oc, os, ook := g.pickCol(outer, []catalog.Type{typ})
		if sok && ook {
			op := lir.OpEq
			if typ != catalog.TypeBool && g.rng.Intn(3) == 0 {
				op = []lir.BinaryOp{lir.OpLt, lir.OpGt}[g.rng.Intn(2)]
			}
			return lir.Binary{Op: op, L: qcol(ss, sc.name), R: qcol(os, oc.name)}
		}
	}
	return qlit(true)
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
		return []int{math.MinInt64, -1, 0, 1, 2, 100, math.MaxInt64}[g.rng.Intn(7)]
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
