package planner

// The query plan view over the wire: ?show-plan / ?dry-run on POST /execute,
// exercised through the real client -> server -> engine path. Confirms the plan
// rides the response as free-form JSON, dry-run skips execution, and the two
// knobs are orthogonal.

import (
	"context"
	"strings"
	"testing"

	radclient "github.com/Southclaws/rad/rad/client"
	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/tests/harness"
)

func TestQueryPlanView(t *testing.T) {
	d := harness.New(t)
	d.Table("items", harness.Text("id"), harness.Text("kind")).
		PK("id").Index("items_kind_idx", "kind").Create()
	d.Insert("items",
		harness.Row{"id": "i1", "kind": "a"},
		harness.Row{"id": "i2", "kind": "b"},
		harness.Row{"id": "i3", "kind": "a"})

	// A filter on the indexed column so the planner has a real access choice.
	prog, err := protocol.UnmarshalProgram([]byte(`{"statements":[{"name":"q","kind":"query","relation":{
		"nodes":{
			"t":{"kind":"scan","table":"items","scope":"t"},
			"f":{"kind":"filter","input":"t","predicate":{"kind":"binary","op":"eq",
				"left":{"kind":"col","scope":"t","column":"kind"},"right":{"kind":"lit","value":"a"}}},
			"o":{"kind":"order","input":"f","terms":[{"expr":{"kind":"col","scope":"t","column":"id"}}]}
		},
		"root":{"node":"o","cardinality":"many"}}}]}`))
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()

	// show-plan -> results AND the plan, which shows the index beat a scan.
	res, err := d.Client.Execute(ctx, prog, radclient.WithPlan())
	if err != nil {
		t.Fatal(err)
	}
	if res.Result == nil {
		t.Fatal("show-plan should still return results")
	}
	text := planText(t, res.Plan)
	for _, want := range []string{"IndexRangeScan", "items_kind_idx", "access:"} {
		if !strings.Contains(text, want) {
			t.Fatalf("plan text missing %q:\n%s", want, text)
		}
	}

	// dry-run + show-plan -> the plan, no execution.
	res, err = d.Client.Execute(ctx, prog, radclient.WithPlan(), radclient.DryRun())
	if err != nil {
		t.Fatal(err)
	}
	if res.Result != nil {
		t.Fatalf("dry-run must not execute; result = %v", res.Result)
	}
	if res.Plan == nil {
		t.Fatal("dry-run + show-plan should return the plan")
	}

	// dry-run alone -> empty success: no result, no plan.
	res, err = d.Client.Execute(ctx, prog, radclient.DryRun())
	if err != nil {
		t.Fatal(err)
	}
	if res.Result != nil || res.Plan != nil {
		t.Fatalf("dry-run alone should be an empty success; result=%v plan=%v", res.Result, res.Plan)
	}

	// no flags -> normal, no plan.
	res, err = d.Client.Execute(ctx, prog)
	if err != nil {
		t.Fatal(err)
	}
	if res.Plan != nil {
		t.Fatalf("plain execute should carry no plan; got %v", res.Plan)
	}
}

func planText(t *testing.T, plan any) string {
	t.Helper()
	m, ok := plan.(map[string]any)
	if !ok {
		t.Fatalf("plan is not an object: %T", plan)
	}
	stmts, ok := m["statements"].([]any)
	if !ok || len(stmts) == 0 {
		t.Fatalf("plan has no statements: %v", m)
	}
	return stmts[0].(map[string]any)["text"].(string)
}
