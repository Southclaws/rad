package exec

// The recursion iteration cap is a configurable execution safeguard, not a
// language limit: the same terminating query succeeds under the default and
// fails under a cap below its depth.

import (
	"context"
	"fmt"
	"testing"

	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// chainReach is "every node reachable from n0 over edges (src → dst)", union
// all, ordered by id — a recursive query whose depth equals the chain length.
func chainReach() lir.Query {
	return lir.Query{
		Card: lir.CardMany,
		Bindings: map[string]lir.Relation{
			"reach": lir.Recursive{
				Accumulation: lir.AccumulateAll,
				Anchor: lir.Rows{
					Scope:   "a",
					Columns: []lir.RowsCol{{Name: "id", Kind: lir.KindText}},
					Values:  [][]any{{"n0"}},
				},
				Step: lir.Project{
					Input: lir.Join{
						Left:  lir.Scan{Table: "edges", Scope: "e"},
						Right: lir.RecursiveRef{Binding: "reach", Scope: "p"},
						Kind:  lir.InnerJoin,
						On:    lir.Binary{Op: lir.OpEq, L: lir.Column{Scope: "e", Name: "src"}, R: lir.Column{Scope: "p", Name: "id"}},
					},
					Scope:  "s",
					Fields: []lir.ProjField{{As: "id", Expr: lir.Column{Scope: "e", Name: "dst"}}},
				},
			},
		},
		Root: lir.Order{
			Input: lir.Ref{Binding: "reach", Scope: "r"},
			Terms: []lir.OrderTerm{{Expr: lir.Column{Scope: "r", Name: "id"}}},
		},
	}
}

func TestRecursionCapConfigurable(t *testing.T) {
	ctx := context.Background()
	store, err := kvslate.Open("test-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })
	cat := catalog.New(store)
	if _, err := cat.CreateTable(ctx, model.TableDef{
		Name:       "edges",
		Columns:    []model.ColumnDef{{Name: "src", Type: model.TypeText}, {Name: "dst", Type: model.TypeText}},
		PrimaryKey: []string{"src", "dst"},
		Indexes:    []model.IndexDef{{Name: "edges_src_idx", Columns: []string{"src"}}},
	}); err != nil {
		t.Fatal(err)
	}

	// A chain n0 → n1 → … → n5: reachability recurses six rounds.
	def := New(store, cat)
	for i := range 5 {
		if err := def.Insert(ctx, "edges", lir.Row{
			"src": lir.Text(fmt.Sprintf("n%d", i)),
			"dst": lir.Text(fmt.Sprintf("n%d", i+1)),
		}); err != nil {
			t.Fatal(err)
		}
	}

	// Under the default cap the query terminates: six reachable nodes.
	d, err := def.Execute(ctx, chainReach())
	if err != nil {
		t.Fatalf("default engine: %v", err)
	}
	if len(d.Elems) != 6 {
		t.Fatalf("default engine: got %d rows, want 6", len(d.Elems))
	}

	// A cap below the chain's depth turns the same terminating query into a
	// recursion-limit failure — the limit is honored, not hard-coded.
	capped := New(store, cat, WithRecursionLimits(RecursionLimits{MaxIterations: 3}))
	_, err = capped.Execute(ctx, chainReach())
	if reason, ok := reject.ReasonOf(err); !ok || reason != reject.ReasonRecursionLimit {
		t.Fatalf("capped engine: err = %v (reason %q), want recursion_limit", err, reason)
	}
}
