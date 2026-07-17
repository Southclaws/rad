package planner

// Adversarial recursion tests against *independent* oracles. The engine and the
// refexec oracle run near-identical semi-naive fixpoints over a shared
// canonical row identity, so a conceptual bug in that algorithm would sit in
// both and the differential would stay green. These tests instead pin the
// engine to textbook computations written from scratch here — a plain-map BFS
// for admit-new reachability (a set), and a topological path-count for
// admit-all reachability (a bag) — over many random graphs. A divergence is a
// real engine bug, not two copies of one mistake agreeing.

import (
	"fmt"
	"math/rand"
	"sort"
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/tests/harness"
)

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

// reachableSet is the independent set oracle: every node reachable from roots
// over the directed edges, including the roots, by breadth-first search.
func reachableSet(edges [][2]string, roots []string) []string {
	adj := map[string][]string{}
	for _, ed := range edges {
		adj[ed[0]] = append(adj[ed[0]], ed[1])
	}
	seen := map[string]bool{}
	var queue []string
	for _, r := range roots {
		if !seen[r] {
			seen[r] = true
			queue = append(queue, r)
		}
	}
	for len(queue) > 0 {
		cur := queue[0]
		queue = queue[1:]
		for _, nx := range adj[cur] {
			if !seen[nx] {
				seen[nx] = true
				queue = append(queue, nx)
			}
		}
	}
	out := make([]string, 0, len(seen))
	for k := range seen {
		out = append(out, k)
	}
	sort.Strings(out)
	return out
}

// pathCounts is the independent bag oracle for admit-all over an acyclic graph:
// a node's multiplicity is the number of distinct directed paths reaching it
// (each root is a length-zero path). Computed in node-index order, which is a
// topological order because edges point strictly forward (i < j).
func pathCounts(n int, edges [][2]string, roots []string) map[string]int {
	idx := func(node string) int { var i int; fmt.Sscanf(node, "n%d", &i); return i }
	isRoot := map[string]bool{}
	for _, r := range roots {
		isRoot[r] = true
	}
	into := map[int][]int{} // dst index -> src indices
	for _, ed := range edges {
		into[idx(ed[1])] = append(into[idx(ed[1])], idx(ed[0]))
	}
	pc := make([]int, n)
	for j := 0; j < n; j++ {
		node := fmt.Sprintf("n%d", j)
		if isRoot[node] {
			pc[j] = 1
		}
		for _, i := range into[j] {
			pc[j] += pc[i]
		}
	}
	out := map[string]int{}
	for j := 0; j < n; j++ {
		if pc[j] > 0 {
			out[fmt.Sprintf("n%d", j)] = pc[j]
		}
	}
	return out
}

// idRows renders an ordered `{id: ...}` list as the harness's canonical JSON.
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

// bagRows renders each id repeated by its multiplicity, ascending by id — the
// order the reachability query imposes, with equal ids adjacent.
func bagRows(mult map[string]int) string {
	ids := make([]string, 0, len(mult))
	for id := range mult {
		ids = append(ids, id)
	}
	sort.Strings(ids)
	var out []string
	for _, id := range ids {
		for k := 0; k < mult[id]; k++ {
			out = append(out, id)
		}
	}
	return idRows(out)
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

// depthBagJSON is the independent bag oracle for admit-all reachability that
// carries a depth column: enumerate every directed path from a root by DFS
// (finite on a forward-edge DAG) and emit (endpoint, path length). Ordered by
// (id, depth) to match the query's order.
func depthBagJSON(edges [][2]string, roots []string) string {
	adj := map[string][]string{}
	for _, ed := range edges {
		adj[ed[0]] = append(adj[ed[0]], ed[1])
	}
	type row struct {
		id    string
		depth int
	}
	var rows []row
	var dfs func(node string, d int)
	dfs = func(node string, d int) {
		rows = append(rows, row{node, d})
		for _, nx := range adj[node] {
			dfs(nx, d+1)
		}
	}
	for _, r := range roots {
		dfs(r, 0)
	}
	sort.Slice(rows, func(i, j int) bool {
		if rows[i].id != rows[j].id {
			return rows[i].id < rows[j].id
		}
		return rows[i].depth < rows[j].depth
	})
	var b strings.Builder
	b.WriteByte('[')
	for i, r := range rows {
		if i > 0 {
			b.WriteByte(',')
		}
		fmt.Fprintf(&b, `{"id":%q,"depth":%d}`, r.id, r.depth)
	}
	b.WriteByte(']')
	return b.String()
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

// TestRecDepthAllVsPathEnum: admit-all reachability carrying a derived depth
// must reproduce, row for row, the (endpoint, length) of every path an
// independent DFS enumerates — pinning multiplicity and carried state together.
func TestRecDepthAllVsPathEnum(t *testing.T) {
	t.Parallel()
	rng := rand.New(rand.NewSource(0xC2B2AE35))
	for iter := 0; iter < 80; iter++ {
		n := 3 + rng.Intn(4) // 3..6 nodes keeps path counts small
		edges := randGraph(rng, n, true)
		roots := randRoots(rng, n)
		want := depthBagJSON(edges, roots)
		t.Run(fmt.Sprintf("g%02d", iter), func(t *testing.T) {
			t.Logf("edges=%v roots=%v", edges, roots)
			d := graphDB(t, edges...)
			d.Query(depthAll(roots...)).Equals(want)
		})
	}
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

// TestRecCountReachableVsBFS: counting the admit-new reachable set must equal
// the size of the independent BFS reachable set — recursion into an aggregate.
func TestRecCountReachableVsBFS(t *testing.T) {
	t.Parallel()
	rng := rand.New(rand.NewSource(0x27D4EB2F))
	for iter := 0; iter < 80; iter++ {
		n := 3 + rng.Intn(5)
		edges := randGraph(rng, n, false)
		roots := randRoots(rng, n)
		want := fmt.Sprintf(`[{"n":%d}]`, len(reachableSet(edges, roots)))
		t.Run(fmt.Sprintf("g%02d", iter), func(t *testing.T) {
			t.Logf("edges=%v roots=%v", edges, roots)
			d := graphDB(t, edges...)
			d.Query(countReach(roots...)).Equals(want)
		})
	}
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

// tagStateSetJSON is the independent oracle for admit-new reachability carrying
// a nullable tag: breadth-first search over the (node, tag) state space, tag
// being the reaching edge's note (NULL at a root), deduped with NULL == NULL.
// Ordered by (id, tag), NULL first.
func tagStateSetJSON(edges []notedEdge, roots []string) string {
	type outEdge struct {
		dst  string
		note *string
	}
	adj := map[string][]outEdge{}
	for _, ed := range edges {
		adj[ed.src] = append(adj[ed.src], outEdge{ed.dst, ed.note})
	}
	type row struct {
		id  string
		tag *string
	}
	key := func(id string, tag *string) string {
		if tag == nil {
			return id + "\x00N"
		}
		return id + "\x00V" + *tag
	}
	seen := map[string]bool{}
	var queue, all []row
	add := func(id string, tag *string) {
		k := key(id, tag)
		if !seen[k] {
			seen[k] = true
			queue = append(queue, row{id, tag})
			all = append(all, row{id, tag})
		}
	}
	for _, r := range roots {
		add(r, nil)
	}
	for len(queue) > 0 {
		cur := queue[0]
		queue = queue[1:]
		for _, oe := range adj[cur.id] {
			add(oe.dst, oe.note)
		}
	}
	less := func(a, b *string) bool {
		if a == nil {
			return b != nil
		}
		if b == nil {
			return false
		}
		return *a < *b
	}
	sort.Slice(all, func(i, j int) bool {
		if all[i].id != all[j].id {
			return all[i].id < all[j].id
		}
		return less(all[i].tag, all[j].tag)
	})
	var b strings.Builder
	b.WriteByte('[')
	for i, r := range all {
		if i > 0 {
			b.WriteByte(',')
		}
		if r.tag == nil {
			fmt.Fprintf(&b, `{"id":%q,"tag":null}`, r.id)
		} else {
			fmt.Fprintf(&b, `{"id":%q,"tag":%q}`, r.id, *r.tag)
		}
	}
	b.WriteByte(']')
	return b.String()
}

// tagBagJSON is the independent oracle for admit-all reachability carrying a
// nullable tag: DFS every path, tagging each node with the note of the edge
// that reached it (NULL at a root). Ordered by (id, tag) with NULL first, to
// match the query's order over a nullable column.
func tagBagJSON(edges []notedEdge, roots []string) string {
	type outEdge struct {
		dst  string
		note *string
	}
	adj := map[string][]outEdge{}
	for _, ed := range edges {
		adj[ed.src] = append(adj[ed.src], outEdge{ed.dst, ed.note})
	}
	type row struct {
		id  string
		tag *string
	}
	var rows []row
	var dfs func(node string, tag *string)
	dfs = func(node string, tag *string) {
		rows = append(rows, row{node, tag})
		for _, oe := range adj[node] {
			dfs(oe.dst, oe.note)
		}
	}
	for _, r := range roots {
		dfs(r, nil)
	}
	// NULL sorts before any string; otherwise lexicographic.
	less := func(a, b *string) bool {
		if a == nil {
			return b != nil
		}
		if b == nil {
			return false
		}
		return *a < *b
	}
	sort.Slice(rows, func(i, j int) bool {
		if rows[i].id != rows[j].id {
			return rows[i].id < rows[j].id
		}
		return less(rows[i].tag, rows[j].tag)
	})
	var b strings.Builder
	b.WriteByte('[')
	for i, r := range rows {
		if i > 0 {
			b.WriteByte(',')
		}
		if r.tag == nil {
			fmt.Fprintf(&b, `{"id":%q,"tag":null}`, r.id)
		} else {
			fmt.Fprintf(&b, `{"id":%q,"tag":%q}`, r.id, *r.tag)
		}
	}
	b.WriteByte(']')
	return b.String()
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

// TestRecNullableTagAllVsPathEnum: a nullable tag initialised to a typed NULL
// and carried from each reaching edge's nullable note must reproduce, row for
// row, what an independent DFS predicts — pinning the nullability fixpoint,
// typed-NULL projection, and NULL ordering together.
func TestRecNullableTagAllVsPathEnum(t *testing.T) {
	t.Parallel()
	rng := rand.New(rand.NewSource(0x14057B7E))
	for iter := 0; iter < 80; iter++ {
		n := 3 + rng.Intn(4)
		edges := notedGraph(rng, n, true)
		roots := randRoots(rng, n)
		want := tagBagJSON(edges, roots)
		t.Run(fmt.Sprintf("g%02d", iter), func(t *testing.T) {
			t.Logf("edges=%v roots=%v", edges, roots)
			d := harnessNotedDB(t, edges)
			d.Query(tagReach("all", roots...)).Equals(want)
		})
	}
}

// TestRecNullableTagNewVsStateBFS: admit-new reachability carrying a nullable
// tag over random *cyclic* graphs must equal an independent breadth-first
// search of the (node, tag) state space — the fixpoint's termination and the
// NULL == NULL dedup both ride on the shared canonical identity, so this is the
// sharpest check of that identity inside a real recursive loop.
func TestRecNullableTagNewVsStateBFS(t *testing.T) {
	t.Parallel()
	rng := rand.New(rand.NewSource(0x3C6EF372))
	for iter := 0; iter < 80; iter++ {
		n := 3 + rng.Intn(4)
		edges := notedGraph(rng, n, false)
		roots := randRoots(rng, n)
		want := tagStateSetJSON(edges, roots)
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

// TestRecReachabilityNewVsBFS: admit-new reachability over random cyclic graphs
// must equal an independent breadth-first reachable set.
func TestRecReachabilityNewVsBFS(t *testing.T) {
	t.Parallel()
	rng := rand.New(rand.NewSource(0x5DEECE66D))
	for iter := 0; iter < 80; iter++ {
		n := 3 + rng.Intn(5) // 3..7 nodes → single-digit ids sort numerically
		edges := randGraph(rng, n, false)
		roots := randRoots(rng, n)
		want := idRows(reachableSet(edges, roots))
		t.Run(fmt.Sprintf("g%02d", iter), func(t *testing.T) {
			t.Logf("edges=%v roots=%v", edges, roots)
			d := graphDB(t, edges...)
			d.Query(reachN("new", roots...)).Equals(want)
		})
	}
}

// TestRecReachabilityAllVsPathCount: admit-all reachability over random acyclic
// graphs must reproduce the exact bag an independent path-count predicts, and
// distinct over that bag must collapse to the reachable set.
func TestRecReachabilityAllVsPathCount(t *testing.T) {
	t.Parallel()
	rng := rand.New(rand.NewSource(0xB5297A4D))
	for iter := 0; iter < 80; iter++ {
		n := 3 + rng.Intn(5)
		edges := randGraph(rng, n, true)
		roots := randRoots(rng, n)
		mult := pathCounts(n, edges, roots)
		wantBag := bagRows(mult)
		wantSet := idRows(reachableSet(edges, roots))
		t.Run(fmt.Sprintf("g%02d", iter), func(t *testing.T) {
			t.Logf("edges=%v roots=%v", edges, roots)
			d := graphDB(t, edges...)
			d.Query(reachN("all", roots...)).Equals(wantBag)
			d.Query(reachAllDistinct(roots...)).Equals(wantSet)
		})
	}
}
