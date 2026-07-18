package generative

// Recursive-query synthesis: a random directed graph plus a correct-by-
// construction recursive query over it, for the three-way differential. Think
// like a compiler team, not a SQL one — generate graphs and traversals, then
// hold the executor to the reference interpreter.
//
// Termination is guaranteed by construction so every case checks a *result*,
// never the iteration cap (cycles and caps are covered by hand-written tests):
//
//   - An acyclic graph (edges only point forward in node order) terminates
//     under either accumulation mode with any recursive state, so those cases
//     carry the rich state shapes.
//   - An arbitrary graph may contain cycles, so those cases use `accumulation:
//     new` over id-only rows, which closes on the finite node set.

import (
	"fmt"

	"pgregory.net/rapid"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// GraphCatalog is the fixed schema the recursive generator traverses: a
// directed edge relation whose `w` (weight) and nullable `note` feed recursive
// state. The index on `src` gives the planner an access path, so the forced-
// full-scan leg of the differential is a genuine alternative.
func GraphCatalog() *Catalog {
	return &Catalog{Tables: []Table{{
		Name: "edges",
		Columns: []Column{
			{Name: "src", Type: model.TypeText},
			{Name: "dst", Type: model.TypeText},
			{Name: "w", Type: model.TypeInt64},
			{Name: "note", Type: model.TypeText, Nullable: true},
		},
		PrimaryKey: []string{"src", "dst"},
		Indexes:    [][]string{{"src"}},
	}}}
}

// GraphEdge is one directed edge with its state-feeding attributes; a nil Note
// is a NULL.
type GraphEdge struct {
	Src, Dst string
	W        int64
	Note     *string
}

// Graph is a generated directed graph. Acyclic records how it was drawn: when
// true, every edge points strictly forward in node order, so the query may
// carry rich state under either accumulation mode.
type Graph struct {
	Nodes   []string
	Edges   []GraphEdge
	Acyclic bool
}

// GenGraph draws a random directed graph: 2..6 nodes and up to 2n distinct
// edges. An acyclic draw restricts edges to forward pairs (i < j); an
// unrestricted draw admits back-edges and self-loops, hence cycles.
func GenGraph(rt *rapid.T) Graph {
	n := rapid.IntRange(2, 6).Draw(rt, "graph_nodes")
	nodes := make([]string, n)
	for i := range nodes {
		nodes[i] = fmt.Sprintf("n%d", i)
	}

	acyclic := rapid.Bool().Draw(rt, "graph_acyclic")
	m := rapid.IntRange(0, 2*n).Draw(rt, "graph_edges")
	seen := map[[2]int]bool{}
	var edges []GraphEdge
	for range m {
		i := rapid.IntRange(0, n-1).Draw(rt, "edge_src")
		j := rapid.IntRange(0, n-1).Draw(rt, "edge_dst")
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
		edges = append(edges, GraphEdge{
			Src:  nodes[i],
			Dst:  nodes[j],
			W:    int64(rapid.IntRange(0, 5).Draw(rt, "edge_w")),
			Note: genNote(rt),
		})
	}
	return Graph{Nodes: nodes, Edges: edges, Acyclic: acyclic}
}

// genNote draws an edge note: NULL a third of the time, else a small value.
func genNote(rt *rapid.T) *string {
	if rapid.IntRange(0, 2).Draw(rt, "edge_note_null") == 0 {
		return nil
	}
	s := rapid.SampledFrom([]string{"x", "y", ""}).Draw(rt, "edge_note")
	return &s
}

// GraphData renders a graph's edges as rows for the edges table, keyed by
// table name for the interpreter's scan and the engine's inserts.
func GraphData(g Graph) map[string][]lir.Row {
	rows := make([]lir.Row, len(g.Edges))
	for i, e := range g.Edges {
		note := lir.Null(model.TypeText)
		if e.Note != nil {
			note = lir.Text(*e.Note)
		}
		rows[i] = lir.Row{"src": lir.Text(e.Src), "dst": lir.Text(e.Dst), "w": lir.Int64(e.W), "note": note}
	}
	return map[string][]lir.Row{"edges": rows}
}

// stateCol is one recursive state column: its declared type, the anchor's
// initial cell (nil = NULL), and the step expression computing it from the
// frontier (scope "p") and the joined edge (scope "e").
type stateCol struct {
	name     string
	typ      model.Type
	nullable bool
	initial  any
	step     func() lir.Expr
}

var (
	// depth: a hop counter — parent.depth + 1.
	depthCol = stateCol{
		name: "depth", typ: model.TypeInt64, initial: int64(0),
		step: func() lir.Expr { return lir.Binary{Op: lir.OpAdd, L: qcol("p", "depth"), R: qlit(int64(1))} },
	}
	// cost: an accumulated edge weight — parent.cost + edge.w.
	costCol = stateCol{
		name: "cost", typ: model.TypeInt64, initial: int64(0),
		step: func() lir.Expr { return lir.Binary{Op: lir.OpAdd, L: qcol("p", "cost"), R: qcol("e", "w")} },
	}
	// tag / tag2: a nullable text carried from the reaching edge's note,
	// initialised NULL — exercises nullability reconciliation and NULL identity.
	tagCol  = stateCol{name: "tag", typ: model.TypeText, nullable: true, step: func() lir.Expr { return qcol("e", "note") }}
	tag2Col = stateCol{name: "tag2", typ: model.TypeText, nullable: true, step: func() lir.Expr { return qcol("e", "note") }}
	// flag: a boolean that flips each hop — not(parent.flag).
	flagCol = stateCol{
		name: "flag", typ: model.TypeBool, initial: true,
		step: func() lir.Expr { return lir.Unary{Op: lir.OpNot, X: qcol("p", "flag")} },
	}
	// num: a nullable int carried from the reaching edge's weight, initialised
	// NULL — a nullable int alongside a nullable text stresses the signature.
	numCol = stateCol{name: "num", typ: model.TypeInt64, nullable: true, step: func() lir.Expr { return qcol("e", "w") }}
)

// stateShapes are the recursive-state signatures the generator draws from (id
// is always present and implicit). Ranging from none through multiple nullable
// columns, they exercise recursive typing as much as traversal.
var stateShapes = [][]stateCol{
	{},
	{depthCol},
	{costCol},
	{tagCol},
	{depthCol, flagCol},
	{tagCol, numCol},
	{depthCol, costCol, tagCol, flagCol, tag2Col},
}

// GenRecursiveQuery synthesises a reachability query over g: a random anchor
// set, an accumulation mode, and a recursive-state signature. Cyclic graphs
// are constrained to id-only rows under `accumulation: new` so the fixpoint
// always closes; acyclic graphs carry any state under either mode. The query
// is well-formed by construction — one frontier reference in the step, a
// monotone join, an anchor free of self-reference.
func GenRecursiveQuery(rt *rapid.T, g Graph) lir.Query {
	nRoots := rapid.IntRange(1, min(3, len(g.Nodes))).Draw(rt, "root_count")
	roots := rapid.Permutation(g.Nodes).Draw(rt, "root_perm")[:nRoots]

	accumulation := lir.AccumulateNew
	shape := stateShapes[0]
	if g.Acyclic {
		if rapid.Bool().Draw(rt, "accumulate_all") {
			accumulation = lir.AccumulateAll
		}
		shape = rapid.SampledFrom(stateShapes).Draw(rt, "state_shape")
	}

	anchorCols := []lir.RowsCol{{Name: "id", Kind: lir.KindText}}
	for _, sc := range shape {
		anchorCols = append(anchorCols, lir.RowsCol{Name: sc.name, Kind: lir.Kind(string(sc.typ)), Nullable: sc.nullable})
	}
	anchorVals := make([][]any, len(roots))
	for i, r := range roots {
		row := []any{r}
		for _, sc := range shape {
			row = append(row, sc.initial)
		}
		anchorVals[i] = row
	}
	anchor := lir.Rows{Scope: "a", Columns: anchorCols, Values: anchorVals}

	stepFields := []lir.ProjField{{As: "id", Expr: qcol("e", "dst")}}
	for _, sc := range shape {
		stepFields = append(stepFields, lir.ProjField{As: sc.name, Expr: sc.step()})
	}
	step := lir.Project{
		Input: lir.Join{
			Left:  qscan("edges", "e"),
			Right: lir.RecursiveRef{Binding: "rec", Scope: "p"},
			Kind:  lir.InnerJoin,
			On:    qeq(qcol("e", "src"), qcol("p", "id")),
		},
		Scope:  "s",
		Fields: stepFields,
	}

	return lir.Query{
		Card:     lir.CardMany,
		Bindings: map[string]lir.Relation{"rec": lir.Recursive{Anchor: anchor, Step: step, Accumulation: accumulation}},
		Root: lir.Order{
			Input: lir.Ref{Binding: "rec", Scope: "r"},
			Terms: []lir.OrderTerm{{Expr: qlit(true)}},
		},
	}
}
