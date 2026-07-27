package planner

// Recursion by shape, not by example. Trees are one family; the semantics
// worth pinning live in the others — graph reachability (bag vs. set),
// diamonds, DAGs, cycles, self-loops, ancestors, multiple/empty anchors,
// recursive state (depth/cost/carried columns), NULL identity, recursive
// joins, and executor stress (fan-out, deep chains). Each runs through the
// real client→server→bind→plan→execute path.

import (
	"fmt"
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/tests/harness"
)

// e is one directed edge, for readable graph literals.
func e(src, dst string) [2]string { return [2]string{src, dst} }

// graphDB builds a directed edge table (src → dst, composite key) and seeds
// it. An index on src lets the recursive step's join use an index probe.
func graphDB(t *testing.T, edges ...[2]string) *harness.DB {
	d := harness.New(t)
	d.Table("edges", harness.Text("src"), harness.Text("dst")).
		PK("src", "dst").
		Index("edges_src_idx", "src").
		Create()
	if len(edges) > 0 {
		rows := make([]harness.Row, len(edges))
		for i, ed := range edges {
			rows[i] = harness.Row{"src": ed[0], "dst": ed[1]}
		}
		d.Insert("edges", rows...)
	}
	return d
}

// reachN builds "every node reachable from the given roots over edges
// (src → dst), including the roots", under the given union mode, ordered by
// id. Zero roots is the empty-anchor case.
func reachN(union string, roots ...string) lirwire.Query {
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
		"ord": lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}}),
	}, map[string]lirwire.Binding{
		"reach": lirwire.Recursive("anchor", "step", union),
	}, "ord", "many")
}

// -
// graph reachability: bag vs set, transitive closure
// -

func TestRecTransitiveClosure(t *testing.T) {
	t.Parallel()
	d := graphDB(t, e("A", "B"), e("B", "C"), e("C", "D"))
	d.Query(reachN("new", "A")).Equals(`[{"id":"A"},{"id":"B"},{"id":"C"},{"id":"D"}]`)
}

// The canonical bag-vs-set test: D has two derivations (via B and via C).
func TestRecDiamondDistinct(t *testing.T) {
	t.Parallel()
	d := graphDB(t, e("A", "B"), e("A", "C"), e("B", "D"), e("C", "D"))
	d.Query(reachN("new", "A")).Equals(`[{"id":"A"},{"id":"B"},{"id":"C"},{"id":"D"}]`)
}

func TestRecDiamondAll(t *testing.T) {
	t.Parallel()
	d := graphDB(t, e("A", "B"), e("A", "C"), e("B", "D"), e("C", "D"))
	// D appears twice — once per derivation — under union all.
	d.Query(reachN("all", "A")).Equals(`[{"id":"A"},{"id":"B"},{"id":"C"},{"id":"D"},{"id":"D"}]`)
}

func TestRecDAG(t *testing.T) {
	t.Parallel()
	d := graphDB(t,
		e("A", "B"), e("A", "C"), e("B", "D"), e("C", "D"),
		e("C", "E"), e("D", "F"), e("E", "F"))
	d.Query(reachN("new", "A")).Equals(
		`[{"id":"A"},{"id":"B"},{"id":"C"},{"id":"D"},{"id":"E"},{"id":"F"}]`)
}

// -
// cycles and self-loops: termination
// -

func TestRecTwoCycleDistinct(t *testing.T) {
	t.Parallel()
	d := graphDB(t, e("A", "B"), e("B", "A"))
	// distinct dedups A on the way back, so the fixpoint closes.
	d.Query(reachN("new", "A")).Equals(`[{"id":"A"},{"id":"B"}]`)
}

func TestRecTwoCycleAllCap(t *testing.T) {
	t.Parallel()
	d := graphDB(t, e("A", "B"), e("B", "A"))
	// union all over a cycle never terminates; the iteration cap fails it.
	d.Query(reachN("all", "A")).ExpectCode("execution_failed")
}

func TestRecSelfLoopDistinct(t *testing.T) {
	t.Parallel()
	d := graphDB(t, e("A", "A"))
	d.Query(reachN("new", "A")).Equals(`[{"id":"A"}]`)
}

func TestRecSelfLoopAllCap(t *testing.T) {
	t.Parallel()
	d := graphDB(t, e("A", "A"))
	d.Query(reachN("all", "A")).ExpectCode("execution_failed")
}

// -
// anchors: empty, missing, multiple; connected components
// -

func TestRecEmptyAnchor(t *testing.T) {
	t.Parallel()
	d := graphDB(t, e("A", "B"))
	// No roots: the anchor is empty, the step never runs, the result is empty.
	d.Query(reachN("new")).Empty()
}

func TestRecMissingRoot(t *testing.T) {
	t.Parallel()
	d := graphDB(t, e("A", "B"))
	// A root with no outgoing edges yields just itself.
	d.Query(reachN("new", "Z")).Equals(`[{"id":"Z"}]`)
}

func TestRecMultipleAnchorsGraph(t *testing.T) {
	t.Parallel()
	d := graphDB(t, e("A", "B"), e("X", "Y"))
	// The frontier starts as {A, X}, not one root then the other.
	d.Query(reachN("new", "A", "X")).Equals(
		`[{"id":"A"},{"id":"B"},{"id":"X"},{"id":"Y"}]`)
}

// An undirected graph is a directed one with both edges; reachability finds
// the whole connected component and terminates under distinct.
func TestRecConnectedComponents(t *testing.T) {
	t.Parallel()
	d := graphDB(t,
		e("alice", "bob"), e("bob", "alice"),
		e("alice", "cyn"), e("cyn", "alice"),
		e("dave", "eve"), e("eve", "dave")) // a separate component
	d.Query(reachN("new", "alice")).Equals(
		`[{"id":"alice"},{"id":"bob"},{"id":"cyn"}]`)
}

// -
// ancestors: the FK walked in reverse
// -

func TestRecAncestors(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Walk up the referral chain from c4: c4 → c2 → c1.
	d.Query(qb(map[string]lirwire.Node{
		"a_scan":   lirwire.Scan("customers", "ac"),
		"a_filter": lirwire.Filter("a_scan", eqText("ac", "id", "c4")),
		"a_proj": lirwire.Project("a_filter", "a", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("ac", "id")},
			{As: "referrer_id", Expr: lirwire.Col("ac", "referrer_id")},
		}),
		"anc":   lirwire.Scan("customers", "up"),
		"front": lirwire.RecursiveRef("chain", "fr"),
		"j": lirwire.Join("anc", "front", "inner",
			lirwire.Binary("eq", lirwire.Col("up", "id"), lirwire.Col("fr", "referrer_id"))),
		"step": lirwire.Project("j", "s", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("up", "id")},
			{As: "referrer_id", Expr: lirwire.Col("up", "referrer_id")},
		}),
		"ref": lirwire.Ref("chain", "r"),
		"ord": lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}}),
	}, map[string]lirwire.Binding{
		"chain": lirwire.Recursive("a_proj", "step", "new"),
	}, "ord", "many")).Equals(`[
		{"id":"c1","referrer_id":null},
		{"id":"c2","referrer_id":"c1"},
		{"id":"c4","referrer_id":"c2"}
	]`)
}

func TestRecMultipleAnchorsTree(t *testing.T) {
	t.Parallel()
	d := shop(t)
	// Anchors are the roots — customers with no referrer (c1, c5). Their
	// whole forest is everybody.
	d.Query(qb(map[string]lirwire.Node{
		"a_scan": lirwire.Scan("customers", "ac"),
		"a_filter": lirwire.Filter("a_scan",
			lirwire.Unary("is_null", lirwire.Col("ac", "referrer_id"))),
		"a_proj": lirwire.Project("a_filter", "a", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("ac", "id")},
		}),
		"sc":    lirwire.Scan("customers", "sc"),
		"front": lirwire.RecursiveRef("tree", "parent"),
		"j": lirwire.Join("sc", "front", "inner",
			lirwire.Binary("eq", lirwire.Col("sc", "referrer_id"), lirwire.Col("parent", "id"))),
		"step": lirwire.Project("j", "s", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("sc", "id")},
		}),
		"ref": lirwire.Ref("tree", "r"),
		"ord": lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}}),
	}, map[string]lirwire.Binding{
		"tree": lirwire.Recursive("a_proj", "step", "new"),
	}, "ord", "many")).Equals(`[
		{"id":"c1"},{"id":"c2"},{"id":"c3"},{"id":"c4"},{"id":"c5"}
	]`)
}

// -
// recursive state: cost, carried columns, wide signatures
// -

func TestRecCost(t *testing.T) {
	t.Parallel()
	d := harness.New(t)
	d.Table("wedges", harness.Text("src"), harness.Text("dst"), harness.Int64("cost")).
		PK("src", "dst").Index("wedges_src_idx", "src").Create()
	d.Insert("wedges",
		harness.Row{"src": "A", "dst": "B", "cost": 5},
		harness.Row{"src": "B", "dst": "C", "cost": 3})
	// total_cost accumulates the edge weights along the path.
	d.Query(qb(map[string]lirwire.Node{
		"anchor": lirwire.Rows("a", cols("id", "text", "total", "int64"),
			[][]lirwire.Cell{{mustValue("A"), mustValue(0)}}),
		"escan": lirwire.Scan("wedges", "e"),
		"front": lirwire.RecursiveRef("paths", "p"),
		"j": lirwire.Join("escan", "front", "inner",
			lirwire.Binary("eq", lirwire.Col("e", "src"), lirwire.Col("p", "id"))),
		"step": lirwire.Project("j", "s", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("e", "dst")},
			{As: "total", Expr: lirwire.Binary("add", lirwire.Col("p", "total"), lirwire.Col("e", "cost"))},
		}),
		"ref": lirwire.Ref("paths", "r"),
		"ord": lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}}),
	}, map[string]lirwire.Binding{
		"paths": lirwire.Recursive("anchor", "step", "all"),
	}, "ord", "many")).Equals(`[
		{"id":"A","total":0},{"id":"B","total":5},{"id":"C","total":8}
	]`)
}

func TestRecCarriedColumns(t *testing.T) {
	t.Parallel()
	d := graphDB(t, e("A", "B"), e("B", "C"))
	// Three columns: a derived depth, and origin carried unchanged from the
	// anchor — recursive state is not just ids.
	d.Query(qb(map[string]lirwire.Node{
		"anchor": lirwire.Rows("a", cols("id", "text", "depth", "int64", "origin", "text"),
			[][]lirwire.Cell{{mustValue("A"), mustValue(0), mustValue("A")}}),
		"escan": lirwire.Scan("edges", "e"),
		"front": lirwire.RecursiveRef("walk", "p"),
		"j": lirwire.Join("escan", "front", "inner",
			lirwire.Binary("eq", lirwire.Col("e", "src"), lirwire.Col("p", "id"))),
		"step": lirwire.Project("j", "s", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("e", "dst")},
			{As: "depth", Expr: lirwire.Binary("add", lirwire.Col("p", "depth"), lirwire.Lit(lirwire.Int64(1)))},
			{As: "origin", Expr: lirwire.Col("p", "origin")},
		}),
		"ref": lirwire.Ref("walk", "r"),
		"ord": lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}}),
	}, map[string]lirwire.Binding{
		"walk": lirwire.Recursive("anchor", "step", "all"),
	}, "ord", "many")).Equals(`[
		{"id":"A","depth":0,"origin":"A"},
		{"id":"B","depth":1,"origin":"A"},
		{"id":"C","depth":2,"origin":"A"}
	]`)
}

// NULL identity under distinct: a NULL-valued carried column must make
// (D, NULL) equal to (D, NULL), so the two derivations of D collapse to one.
func TestRecNullDistinct(t *testing.T) {
	t.Parallel()
	// A diamond over edges carrying a nullable note column, seeded NULL. The
	// anchor's note is a real value; the step reads the (NULL) edge note, so
	// note widens to nullable (reconciliation), and the two derivations of
	// (D, NULL) collapse to one only if NULL == NULL under distinct.
	d := harness.New(t)
	d.Table("nedges", harness.Text("src"), harness.Text("dst"), harness.Null(harness.Text("note"))).
		PK("src", "dst").Index("nedges_src_idx", "src").Create()
	d.Insert("nedges",
		harness.Row{"src": "A", "dst": "B"},
		harness.Row{"src": "A", "dst": "C"},
		harness.Row{"src": "B", "dst": "D"},
		harness.Row{"src": "C", "dst": "D"}) // note omitted → NULL
	d.Query(qb(map[string]lirwire.Node{
		"anchor": lirwire.Rows("a", cols("id", "text", "note", "text"),
			[][]lirwire.Cell{{mustValue("A"), mustValue("root")}}),
		"escan": lirwire.Scan("nedges", "e"),
		"front": lirwire.RecursiveRef("reach", "p"),
		"j": lirwire.Join("escan", "front", "inner",
			lirwire.Binary("eq", lirwire.Col("e", "src"), lirwire.Col("p", "id"))),
		"step": lirwire.Project("j", "s", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("e", "dst")},
			{As: "note", Expr: lirwire.Col("e", "note")},
		}),
		"ref": lirwire.Ref("reach", "r"),
		"ord": lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}}),
	}, map[string]lirwire.Binding{
		"reach": lirwire.Recursive("anchor", "step", "new"),
	}, "ord", "many")).Equals(`[
		{"id":"A","note":"root"},{"id":"B","note":null},{"id":"C","note":null},{"id":"D","note":null}
	]`)
}

// A recursive state column initialised to NULL in the anchor: a projected
// typed-NULL literal (a NULL with no column context). The step fills it with a
// real value, so tag reconciles to nullable text.
func TestRecNullState(t *testing.T) {
	t.Parallel()
	d := shop(t)
	d.Query(qb(map[string]lirwire.Node{
		"a_scan":   lirwire.Scan("customers", "ac"),
		"a_filter": lirwire.Filter("a_scan", eqText("ac", "id", "c1")),
		"a_proj": lirwire.Project("a_filter", "a", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("ac", "id")},
			{As: "tag", Expr: lirwire.Lit(lirwire.Null(lirwire.ScalarTypeText))},
		}),
		"sc":    lirwire.Scan("customers", "sc"),
		"front": lirwire.RecursiveRef("tree", "parent"),
		"j": lirwire.Join("sc", "front", "inner",
			lirwire.Binary("eq", lirwire.Col("sc", "referrer_id"), lirwire.Col("parent", "id"))),
		"step": lirwire.Project("j", "s", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("sc", "id")},
			{As: "tag", Expr: lirwire.Col("sc", "name")},
		}),
		"ref": lirwire.Ref("tree", "r"),
		"ord": lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}}),
	}, map[string]lirwire.Binding{
		"tree": lirwire.Recursive("a_proj", "step", "all"),
	}, "ord", "many")).Equals(`[
		{"id":"c1","tag":null},{"id":"c2","tag":"Bob"},{"id":"c3","tag":"Cyn"},{"id":"c4","tag":"Dee"}
	]`)
}

// -
// recursive joins: the step joins the frontier and another table
// -

func TestRecStepJoinsTable(t *testing.T) {
	t.Parallel()
	d := harness.New(t)
	d.Table("edges", harness.Text("src"), harness.Text("dst")).
		PK("src", "dst").Index("edges_src_idx", "src").Create()
	d.Table("labels", harness.Text("id"), harness.Text("label")).Create()
	d.Insert("edges", harness.Row{"src": "A", "dst": "B"}, harness.Row{"src": "B", "dst": "C"})
	d.Insert("labels",
		harness.Row{"id": "A", "label": "alpha"},
		harness.Row{"id": "B", "label": "bravo"},
		harness.Row{"id": "C", "label": "charlie"})
	// The step joins the frontier to edges, then joins labels to enrich the
	// reached node — an ordinary join composed with the recursive reference.
	d.Query(qb(map[string]lirwire.Node{
		"a_scan":   lirwire.Scan("labels", "la"),
		"a_filter": lirwire.Filter("a_scan", eqText("la", "id", "A")),
		"a_proj": lirwire.Project("a_filter", "a", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("la", "id")},
			{As: "label", Expr: lirwire.Col("la", "label")},
		}),
		"escan": lirwire.Scan("edges", "e"),
		"front": lirwire.RecursiveRef("walk", "p"),
		"j1": lirwire.Join("escan", "front", "inner",
			lirwire.Binary("eq", lirwire.Col("e", "src"), lirwire.Col("p", "id"))),
		"lscan": lirwire.Scan("labels", "cl"),
		"j2": lirwire.Join("j1", "lscan", "inner",
			lirwire.Binary("eq", lirwire.Col("cl", "id"), lirwire.Col("e", "dst"))),
		"step": lirwire.Project("j2", "s", nil, []lirwire.Field{
			{As: "id", Expr: lirwire.Col("e", "dst")},
			{As: "label", Expr: lirwire.Col("cl", "label")},
		}),
		"ref": lirwire.Ref("walk", "r"),
		"ord": lirwire.Order("ref", []lirwire.OrderTerm{{Expr: lirwire.Col("r", "id")}}),
	}, map[string]lirwire.Binding{
		"walk": lirwire.Recursive("a_proj", "step", "new"),
	}, "ord", "many")).Equals(`[
		{"id":"A","label":"alpha"},{"id":"B","label":"bravo"},{"id":"C","label":"charlie"}
	]`)
}

// -
// executor stress: fan-out and deep chains
// -

func TestRecLargeFanout(t *testing.T) {
	t.Parallel()
	edges := make([][2]string, 0, 1000)
	for i := range 1000 {
		edges = append(edges, e("root", fmt.Sprintf("c%04d", i)))
	}
	d := graphDB(t, edges...)
	// root plus 1000 children, discovered in one step iteration.
	d.Query(reachN("new", "root")).Len(1001)
}

func TestRecDeepChain(t *testing.T) {
	t.Parallel()
	const n = 500
	edges := make([][2]string, 0, n)
	for i := range n {
		edges = append(edges, e(fmt.Sprintf("n%04d", i), fmt.Sprintf("n%04d", i+1)))
	}
	d := graphDB(t, edges...)
	// A chain n0 → … → n500 recurses 500 deep — well within the iteration
	// backstop, so a legitimately deep traversal is not mistaken for a runaway.
	d.Query(reachN("new", "n0000")).Len(n + 1)
}
