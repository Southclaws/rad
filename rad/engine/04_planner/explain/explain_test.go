package explain_test

// The query plan view: the JSON artifact + derived string that rides the
// transport as observability metadata. Exercises access-decision capture (why
// an access path was chosen), the pretty render, and JSON round-trip.

import (
	"encoding/json"
	"strings"
	"testing"

	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
	"github.com/Southclaws/rad/rad/engine/04_planner/explain"
)

func TestPlanViewAccessDecision(t *testing.T) {
	q := bind(t, lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"),
		band(beq(bcol("t", "board_id"), blit("b1")), beq(bcol("t", "status"), blit("open"))))})
	view := explain.NewPlanView(planner.PlanQuery(q))

	// The pretty render shows why the index beat a table scan.
	s := view.String()
	for _, want := range []string{"access:", "tasks_board_status_idx", "✓", "TableScan(0)"} {
		if !strings.Contains(s, want) {
			t.Fatalf("plan view missing %q:\n%s", want, s)
		}
	}

	// The JSON artifact round-trips and carries the structured candidates with
	// the winner marked.
	raw, err := json.Marshal(view)
	if err != nil {
		t.Fatal(err)
	}
	var got explain.PlanView
	if err := json.Unmarshal(raw, &got); err != nil {
		t.Fatalf("plan view JSON did not round-trip: %v", err)
	}
	idx := findOp(got.Root, "IndexRangeScan")
	if idx == nil {
		t.Fatalf("no IndexRangeScan in view:\n%s", raw)
	}
	var chosen string
	sawScan := false
	for _, c := range idx.Access {
		if c.Chosen {
			chosen = c.Method
		}
		if c.Method == "TableScan" {
			sawScan = true
		}
	}
	if !strings.Contains(chosen, "tasks_board_status_idx") {
		t.Fatalf("chosen access = %q, want the index; candidates=%+v", chosen, idx.Access)
	}
	if !sawScan {
		t.Fatalf("access candidates should include the rejected TableScan: %+v", idx.Access)
	}
}

// An unconstrained scan of an indexed table has no real access choice (every
// candidate scores 0, the scan wins), so the view renders no access line.
func TestPlanViewNoAccessNoise(t *testing.T) {
	q := bind(t, lir.Query{Card: lir.CardMany, Root: bscan("comments", "c")})
	if s := explain.NewPlanView(planner.PlanQuery(q)).String(); strings.Contains(s, "access:") {
		t.Fatalf("unconstrained scan should render no access decision:\n%s", s)
	}
}

func findOp(n *explain.PlanNodeView, op string) *explain.PlanNodeView {
	if n == nil {
		return nil
	}
	if n.Op == op {
		return n
	}
	for _, c := range n.Children {
		if f := findOp(c, op); f != nil {
			return f
		}
	}
	return nil
}
