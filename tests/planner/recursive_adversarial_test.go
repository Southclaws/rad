package planner

// Adversarial recursion tests against the independent models in tests/oracle.
// The engine and refexec run near-identical semi-naive fixpoints, so a
// conceptual bug in that algorithm would sit in both and the differential would
// stay green. These tests instead pin the engine to oracle.FixpointNew /
// FixpointAll — a plain state-machine model that shares no code with either
// engine or refexec — over many random graphs, carrying id-only, depth, and
// nullable-tag state. A divergence is a real engine bug, not two copies of one
// mistake agreeing.

import (
	"fmt"
	"math/rand"
	"sort"
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/tests/harness"
	"github.com/Southclaws/rad/tests/oracle"
)

// adjacency indexes plain edges by source, the transition the oracle steps.
func adjacency(edges [][2]string) map[string][]string {
	adj := map[string][]string{}
	for _, ed := range edges {
		adj[ed[0]] = append(adj[ed[0]], ed[1])
	}
	return adj
}

// depthState is a reachability state carrying hop count from the anchor.
type depthState struct {
	ID    string
	Depth int
}

// tagState is a reachability state carrying a nullable tag; Null distinguishes
// a NULL tag from the empty string, so the struct's own value equality is the
// NULL == NULL identity admit-new needs.
type tagState struct {
	ID   string
	Tag  string
	Null bool
}

// notedOut is one outgoing edge with its nullable note.
type notedOut struct {
	dst  string
	note *string
}

func notedAdjacency(edges []notedEdge) map[string][]notedOut {
	adj := map[string][]notedOut{}
	for _, ed := range edges {
		adj[ed.src] = append(adj[ed.src], notedOut{ed.dst, ed.note})
	}
	return adj
}

// idRows renders an ordered `{id: ...}` list as the harness's canonical JSON;
// duplicates render as repeated rows (an admit-all bag ordered by id).
func idRows(ids []string) string {
	var b strings.Builder
	b.WriteByte('[')
	for i, id := range ids {
		if i > 0 {
			b.WriteByte(',')
		}
		fmt.Fprintf(&b, `{"id":%q}`, id)
	}
	b.WriteByte(']')
	return b.String()
}

func renderDepth(rows []depthState) string {
	var b strings.Builder
	b.WriteByte('[')
	for i, r := range rows {
		if i > 0 {
			b.WriteByte(',')
		}
		fmt.Fprintf(&b, `{"id":%q,"depth":%d}`, r.ID, r.Depth)
	}
	b.WriteByte(']')
	return b.String()
}

func renderTag(rows []tagState) string {
	var b strings.Builder
	b.WriteByte('[')
	for i, r := range rows {
		if i > 0 {
			b.WriteByte(',')
		}
		if r.Null {
			fmt.Fprintf(&b, `{"id":%q,"tag":null}`, r.ID)
		} else {
			fmt.Fprintf(&b, `{"id":%q,"tag":%q}`, r.ID, r.Tag)
		}
	}
	b.WriteByte(']')
	return b.String()
}

// sortTag orders by (id, tag) with NULL first, matching an order over a
// nullable column.
func sortTag(rows []tagState) {
	sort.Slice(rows, func(i, j int) bool {
		if rows[i].ID != rows[j].ID {
			return rows[i].ID < rows[j].ID
		}
		if rows[i].Null != rows[j].Null {
			return rows[i].Null
		}
		return rows[i].Tag < rows[j].Tag
	})
}

// reachAllDistinct is admit-all reachability with a unary distinct above the
// completed relation — exercising both new features at once. Its result must
// equal admit-new reachability (the set), since distinct collapses the all-bag
// by the same canonical identity admit-new uses.
func reachAllDistinct(roots ...string) lirwire.Query {
	cells := make([][]lirwire.Cell, len(roots))
	for i, r := range roots {
		cells[i] = []lirwire.Cell{mustValue(r)}
	}
	return qb(map[string]lirwire.Node{
		"anchor": lirwire.Rows("a", cols("id", "text"), cells),
		"escan":  lirwire.Scan("edges", "e"),
		"front":  lirwire.RecursiveRef("reach", "p"),
		"ej": lirwire.Join("escan", "front", "inner",
			lirwire.Binary("eq", lirwire.Col("e", "src"), lirwire.Col("p", "id"))),
		"step": lirwire.Project("ej", "s", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("e", "dst")},
		}),
		"ref":  lirwire.Ref("reach", "r"),
		"dist": lirwire.Distinct("ref"),
		"ord":  lirwire.Order("dist", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}}),
	}, map[string]lirwire.Binding{
		"reach": lirwire.Recursive("anchor", "step", "all"),
	}, "ord", "many")
}

// countReach counts the admit-new reachable set with an aggregate above the
// recursive binding — recursion feeding a fold.
func countReach(roots ...string) lirwire.Query {
	cells := make([][]lirwire.Cell, len(roots))
	for i, r := range roots {
		cells[i] = []lirwire.Cell{mustValue(r)}
	}
	return qb(map[string]lirwire.Node{
		"anchor": lirwire.Rows("a", cols("id", "text"), cells),
		"escan":  lirwire.Scan("edges", "e"),
		"front":  lirwire.RecursiveRef("reach", "p"),
		"ej": lirwire.Join("escan", "front", "inner",
			lirwire.Binary("eq", lirwire.Col("e", "src"), lirwire.Col("p", "id"))),
		"step": lirwire.Project("ej", "s", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("e", "dst")},
		}),
		"ref": lirwire.Ref("reach", "r"),
		"agg": lirwire.Aggregate("ref", "g", nil, []lirwire.AggTerm{{Fn: "count", As: "n"}}),
		"ord": lirwire.Order("agg", []lirwire.OrderTerm{{Expr: lirwire.Col("g", "n")}}),
	}, map[string]lirwire.Binding{
		"reach": lirwire.Recursive("anchor", "step", "new"),
	}, "ord", "many")
}

// depthAll is admit-all reachability carrying depth = parent.depth + 1 from an
// anchor depth of 0, ordered by (id, depth).
func depthAll(roots ...string) lirwire.Query {
	cells := make([][]lirwire.Cell, len(roots))
	for i, r := range roots {
		cells[i] = []lirwire.Cell{mustValue(r), mustValue(0)}
	}
	return qb(map[string]lirwire.Node{
		"anchor": lirwire.Rows("a", cols("id", "text", "depth", "int64"), cells),
		"escan":  lirwire.Scan("edges", "e"),
		"front":  lirwire.RecursiveRef("reach", "p"),
		"ej": lirwire.Join("escan", "front", "inner",
			lirwire.Binary("eq", lirwire.Col("e", "src"), lirwire.Col("p", "id"))),
		"step": lirwire.Project("ej", "s", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("e", "dst")},
			{As: "depth", Expr: lirwire.Binary("add", lirwire.Col("p", "depth"), lirwire.Lit(lirwire.Int64(1)))},
		}),
		"ref": lirwire.Ref("reach", "r"),
		"ord": lirwire.Order("ref", []lirwire.OrderTerm{
			{Expr: lirwire.Col("r", "id")},
			{Expr: lirwire.Col("r", "depth")},
		}),
	}, map[string]lirwire.Binding{
		"reach": lirwire.Recursive("anchor", "step", "all"),
	}, "ord", "many")
}

// tagReach is reachability carrying tag = reaching edge's note, from a
// typed-NULL anchor tag, under the given accumulation mode, ordered by (id, tag).
func tagReach(mode string, roots ...string) lirwire.Query {
	cells := make([][]lirwire.Cell, len(roots))
	for i, r := range roots {
		cells[i] = []lirwire.Cell{mustValue(r), nil} // nil = typed NULL tag
	}
	anchorCols := []lirwire.RowsColumn{
		{Name: "id", Type: lirwire.ScalarTypeText},
		{Name: "tag", Type: lirwire.ScalarTypeText, Nullable: ptrBool(true)},
	}
	return qb(map[string]lirwire.Node{
		"anchor": lirwire.Rows("a", anchorCols, cells),
		"escan":  lirwire.Scan("nedges", "e"),
		"front":  lirwire.RecursiveRef("reach", "p"),
		"ej": lirwire.Join("escan", "front", "inner",
			lirwire.Binary("eq", lirwire.Col("e", "src"), lirwire.Col("p", "id"))),
		"step": lirwire.Project("ej", "s", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("e", "dst")},
			{As: "tag", Expr: lirwire.Col("e", "note")},
		}),
		"ref": lirwire.Ref("reach", "r"),
		"ord": lirwire.Order("ref", []lirwire.OrderTerm{
			{Expr: lirwire.Col("r", "id")},
			{Expr: lirwire.Col("r", "tag")},
		}),
	}, map[string]lirwire.Binding{
		"reach": lirwire.Recursive("anchor", "step", mode),
	}, "ord", "many")
}

// randGraph draws a random directed graph over n nodes. When acyclic, edges
// point strictly forward (i < j); otherwise self-loops and back-edges (hence
// cycles) are allowed.
func randGraph(rng *rand.Rand, n int, acyclic bool) [][2]string {
	seen := map[[2]int]bool{}
	var edges [][2]string
	m := rng.Intn(2*n + 1)
	for k := 0; k < m; k++ {
		i, j := rng.Intn(n), rng.Intn(n)
		if acyclic {
			if i == j {
				continue
			}
			if i > j {
				i, j = j, i
			}
		}
		if seen[[2]int{i, j}] {
			continue
		}
		seen[[2]int{i, j}] = true
		edges = append(edges, e(fmt.Sprintf("n%d", i), fmt.Sprintf("n%d", j)))
	}
	return edges
}

// randRoots picks 1..2 distinct root nodes.
func randRoots(rng *rand.Rand, n int) []string {
	perm := rng.Perm(n)
	nr := 1 + rng.Intn(2)
	roots := make([]string, 0, nr)
	for k := 0; k < nr && k < n; k++ {
		roots = append(roots, fmt.Sprintf("n%d", perm[k]))
	}
	return roots
}

// notedEdge is a directed edge whose note (nullable) becomes the reaching
// node's carried tag.
type notedEdge struct {
	src, dst string
	note     *string // nil = NULL
}

// notedGraph draws a random directed graph with a random nullable note per
// edge. When acyclic, edges point strictly forward; otherwise back-edges and
// self-loops (hence cycles) are allowed — admit-new over a (node, tag) state
// still terminates because that state space is finite.
func notedGraph(rng *rand.Rand, n int, acyclic bool) []notedEdge {
	seen := map[[2]int]bool{}
	var edges []notedEdge
	m := rng.Intn(2*n + 1)
	notes := []string{"x", "y", ""}
	for k := 0; k < m; k++ {
		i, j := rng.Intn(n), rng.Intn(n)
		if acyclic {
			if i == j {
				continue
			}
			if i > j {
				i, j = j, i
			}
		}
		if seen[[2]int{i, j}] {
			continue
		}
		seen[[2]int{i, j}] = true
		var note *string
		if rng.Intn(3) != 0 { // NULL a third of the time
			s := notes[rng.Intn(len(notes))]
			note = &s
		}
		edges = append(edges, notedEdge{fmt.Sprintf("n%d", i), fmt.Sprintf("n%d", j), note})
	}
	return edges
}

// harnessNotedDB seeds a directed edge table whose note column is nullable; a
// nil note is inserted as NULL (the key is omitted).
func harnessNotedDB(t *testing.T, edges []notedEdge) *harness.DB {
	d := harness.New(t)
	d.Table("nedges", harness.Text("src"), harness.Text("dst"), harness.Null(harness.Text("note"))).
		PK("src", "dst").Index("nedges_src_idx", "src").Create()
	if len(edges) == 0 {
		return d
	}
	rows := make([]harness.Row, len(edges))
	for i, ed := range edges {
		r := harness.Row{"src": ed.src, "dst": ed.dst}
		if ed.note != nil {
			r["note"] = *ed.note
		}
		rows[i] = r
	}
	d.Insert("nedges", rows...)
	return d
}

// TestRecReachabilityNewVsModel: admit-new reachability over random cyclic
// graphs must equal the model's breadth-first reachable set.
func TestRecReachabilityNewVsModel(t *testing.T) {
	t.Parallel()
	rng := rand.New(rand.NewSource(0x5DEECE66D))
	for iter := 0; iter < 80; iter++ {
		n := 3 + rng.Intn(5) // 3..7 nodes → single-digit ids sort numerically
		edges := randGraph(rng, n, false)
		roots := randRoots(rng, n)
		adj := adjacency(edges)
		reach := oracle.FixpointNew(roots, func(id string) []string { return adj[id] })
		sort.Strings(reach)
		want := idRows(reach)
		t.Run(fmt.Sprintf("g%02d", iter), func(t *testing.T) {
			t.Logf("edges=%v roots=%v", edges, roots)
			d := graphDB(t, edges...)
			d.Query(reachN("new", roots...)).Equals(want)
		})
	}
}

// TestRecReachabilityAllVsModel: admit-all reachability over random acyclic
// graphs must reproduce the model's path bag, and distinct over that bag must
// collapse to the reachable set.
func TestRecReachabilityAllVsModel(t *testing.T) {
	t.Parallel()
	rng := rand.New(rand.NewSource(0xB5297A4D))
	for iter := 0; iter < 80; iter++ {
		n := 3 + rng.Intn(5)
		edges := randGraph(rng, n, true)
		roots := randRoots(rng, n)
		adj := adjacency(edges)
		step := func(id string) []string { return adj[id] }

		bag := oracle.FixpointAll(roots, step)
		sort.Strings(bag)
		wantBag := idRows(bag)

		set := oracle.FixpointNew(roots, step)
		sort.Strings(set)
		wantSet := idRows(set)

		t.Run(fmt.Sprintf("g%02d", iter), func(t *testing.T) {
			t.Logf("edges=%v roots=%v", edges, roots)
			d := graphDB(t, edges...)
			d.Query(reachN("all", roots...)).Equals(wantBag)
			d.Query(reachAllDistinct(roots...)).Equals(wantSet)
		})
	}
}

// TestRecDepthAllVsModel: admit-all reachability carrying a derived depth must
// reproduce, row for row, the (endpoint, length) of every path the model
// enumerates — pinning multiplicity and carried state together.
func TestRecDepthAllVsModel(t *testing.T) {
	t.Parallel()
	rng := rand.New(rand.NewSource(0xC2B2AE35))
	for iter := 0; iter < 80; iter++ {
		n := 3 + rng.Intn(4) // 3..6 nodes keeps path counts small
		edges := randGraph(rng, n, true)
		roots := randRoots(rng, n)
		adj := adjacency(edges)
		anchors := make([]depthState, len(roots))
		for i, r := range roots {
			anchors[i] = depthState{ID: r, Depth: 0}
		}
		bag := oracle.FixpointAll(anchors, func(s depthState) []depthState {
			var out []depthState
			for _, dst := range adj[s.ID] {
				out = append(out, depthState{ID: dst, Depth: s.Depth + 1})
			}
			return out
		})
		sort.Slice(bag, func(i, j int) bool {
			if bag[i].ID != bag[j].ID {
				return bag[i].ID < bag[j].ID
			}
			return bag[i].Depth < bag[j].Depth
		})
		want := renderDepth(bag)
		t.Run(fmt.Sprintf("g%02d", iter), func(t *testing.T) {
			t.Logf("edges=%v roots=%v", edges, roots)
			d := graphDB(t, edges...)
			d.Query(depthAll(roots...)).Equals(want)
		})
	}
}

// TestRecCountReachableVsModel: counting the admit-new reachable set must equal
// the size of the model's reachable set — recursion into an aggregate.
func TestRecCountReachableVsModel(t *testing.T) {
	t.Parallel()
	rng := rand.New(rand.NewSource(0x27D4EB2F))
	for iter := 0; iter < 80; iter++ {
		n := 3 + rng.Intn(5)
		edges := randGraph(rng, n, false)
		roots := randRoots(rng, n)
		adj := adjacency(edges)
		reach := oracle.FixpointNew(roots, func(id string) []string { return adj[id] })
		want := fmt.Sprintf(`[{"n":%d}]`, len(reach))
		t.Run(fmt.Sprintf("g%02d", iter), func(t *testing.T) {
			t.Logf("edges=%v roots=%v", edges, roots)
			d := graphDB(t, edges...)
			d.Query(countReach(roots...)).Equals(want)
		})
	}
}

// tagStep builds the transition for a nullable-tag reachability: each edge
// carries its note as the reached state's tag.
func tagStep(adj map[string][]notedOut) func(tagState) []tagState {
	return func(s tagState) []tagState {
		var out []tagState
		for _, oe := range adj[s.ID] {
			t := tagState{ID: oe.dst}
			if oe.note == nil {
				t.Null = true
			} else {
				t.Tag = *oe.note
			}
			out = append(out, t)
		}
		return out
	}
}

// TestRecNullableTagAllVsModel: a nullable tag initialised to a typed NULL and
// carried from each reaching edge's nullable note must reproduce, row for row,
// what the model enumerates — pinning the nullability fixpoint, typed-NULL
// projection, and NULL ordering together.
func TestRecNullableTagAllVsModel(t *testing.T) {
	t.Parallel()
	rng := rand.New(rand.NewSource(0x14057B7E))
	for iter := 0; iter < 80; iter++ {
		n := 3 + rng.Intn(4)
		edges := notedGraph(rng, n, true)
		roots := randRoots(rng, n)
		anchors := make([]tagState, len(roots))
		for i, r := range roots {
			anchors[i] = tagState{ID: r, Null: true}
		}
		bag := oracle.FixpointAll(anchors, tagStep(notedAdjacency(edges)))
		sortTag(bag)
		want := renderTag(bag)
		t.Run(fmt.Sprintf("g%02d", iter), func(t *testing.T) {
			t.Logf("edges=%v roots=%v", edges, roots)
			d := harnessNotedDB(t, edges)
			d.Query(tagReach("all", roots...)).Equals(want)
		})
	}
}

// TestRecNullableTagNewVsModel: admit-new reachability carrying a nullable tag
// over random *cyclic* graphs must equal the model's breadth-first search of
// the (node, tag) state space — the fixpoint's termination and the NULL == NULL
// dedup both ride on canonical identity, so this is the sharpest check of that
// identity inside a real recursive loop.
func TestRecNullableTagNewVsModel(t *testing.T) {
	t.Parallel()
	rng := rand.New(rand.NewSource(0x3C6EF372))
	for iter := 0; iter < 80; iter++ {
		n := 3 + rng.Intn(4)
		edges := notedGraph(rng, n, false)
		roots := randRoots(rng, n)
		anchors := make([]tagState, len(roots))
		for i, r := range roots {
			anchors[i] = tagState{ID: r, Null: true}
		}
		set := oracle.FixpointNew(anchors, tagStep(notedAdjacency(edges)))
		sortTag(set)
		want := renderTag(set)
		t.Run(fmt.Sprintf("g%02d", iter), func(t *testing.T) {
			t.Logf("edges=%v roots=%v", edges, roots)
			d := harnessNotedDB(t, edges)
			d.Query(tagReach("new", roots...)).Equals(want)
		})
	}
}

// TestRecTwoBindingsCrossJoined: two independent recursive bindings in one
// query, cross-joined at the root. The frontier buffer is keyed by binding
// name, so the two fixpoints must not contaminate each other — reach1 = {A, B},
// reach2 = {X, Y}, product = four rows.
func TestRecTwoBindingsCrossJoined(t *testing.T) {
	t.Parallel()
	d := graphDB(t, e("A", "B"), e("X", "Y"))
	d.Query(qb(map[string]lirwire.Node{
		"anchor1": lirwire.Rows("a1", cols("id", "text"), [][]lirwire.Cell{{mustValue("A")}}),
		"escan1":  lirwire.Scan("edges", "e1"),
		"front1":  lirwire.RecursiveRef("reach1", "p1"),
		"ej1": lirwire.Join("escan1", "front1", "inner",
			lirwire.Binary("eq", lirwire.Col("e1", "src"), lirwire.Col("p1", "id"))),
		"step1": lirwire.Project("ej1", "s1", nil, []lirwire.Field{{As: "id", Expr: lirwire.Col("e1", "dst")}}),

		"anchor2": lirwire.Rows("a2", cols("id", "text"), [][]lirwire.Cell{{mustValue("X")}}),
		"escan2":  lirwire.Scan("edges", "e2"),
		"front2":  lirwire.RecursiveRef("reach2", "p2"),
		"ej2": lirwire.Join("escan2", "front2", "inner",
			lirwire.Binary("eq", lirwire.Col("e2", "src"), lirwire.Col("p2", "id"))),
		"step2": lirwire.Project("ej2", "s2", nil, []lirwire.Field{{As: "id", Expr: lirwire.Col("e2", "dst")}}),

		"r1":    lirwire.Ref("reach1", "x"),
		"r2":    lirwire.Ref("reach2", "y"),
		"cross": lirwire.Join("r1", "r2", "inner", lirwire.Lit(lirwire.Bool(true))),
		"proj": lirwire.Project("cross", "o", nil, []lirwire.Field{
			{As: "a", Expr: lirwire.Col("x", "id")},
			{As: "b", Expr: lirwire.Col("y", "id")},
		}),
		"ord": lirwire.Order("proj", []lirwire.OrderTerm{
			{Expr: lirwire.Col("o", "a")}, {Expr: lirwire.Col("o", "b")},
		}),
	}, map[string]lirwire.Binding{
		"reach1": lirwire.Recursive("anchor1", "step1", "new"),
		"reach2": lirwire.Recursive("anchor2", "step2", "new"),
	}, "ord", "many")).Equals(`[
		{"a":"A","b":"X"},{"a":"A","b":"Y"},{"a":"B","b":"X"},{"a":"B","b":"Y"}
	]`)
}
