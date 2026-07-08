// These tests document RADLIR (03): the value model and the shape of the
// logical query IR. Nothing here touches storage, the planner, or the
// executor — the IR is pure semantics, and these tests prove it stays
// self-contained.
package lir_test

import (
	"encoding/json"
	"testing"

	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
)

// The constructor helpers produce correctly-typed values.
func TestValueConstructors(t *testing.T) {
	cases := []struct {
		v    lir.Value
		typ  catalog.Type
		null bool
	}{
		{lir.Text("hi"), catalog.TypeText, false},
		{lir.Int64(42), catalog.TypeInt64, false},
		{lir.Float64(2.5), catalog.TypeFloat64, false},
		{lir.Null(catalog.TypeText), catalog.TypeText, true},
	}
	for _, c := range cases {
		if c.v.Type != c.typ || c.v.Null != c.null {
			t.Errorf("%+v: want type %s null=%v", c.v, c.typ, c.null)
		}
	}
}

// Equality is typed, and NULL follows SQL intuition: it never equals
// anything, including another NULL. (The executor turns Eq-on-NULL into a
// non-match for the same reason.)
func TestValueEqual(t *testing.T) {
	if !lir.Int64(1).Equal(lir.Int64(1)) {
		t.Error("1 != 1")
	}
	if lir.Int64(1).Equal(lir.Int64(2)) {
		t.Error("1 == 2")
	}
	if lir.Text("1").Equal(lir.Int64(1)) {
		t.Error("cross-type equality: text 1 == int64 1")
	}
	if lir.Null(catalog.TypeInt64).Equal(lir.Null(catalog.TypeInt64)) {
		t.Error("NULL == NULL")
	}
	if lir.Null(catalog.TypeInt64).Equal(lir.Int64(0)) {
		t.Error("NULL == 0")
	}
	if !lir.Float64(2.5).Equal(lir.Float64(2.5)) {
		t.Error("2.5 != 2.5")
	}
}

// String renders values for humans: quoted text, bare numbers, NULL.
func TestValueString(t *testing.T) {
	cases := map[string]lir.Value{
		`"hi"`: lir.Text("hi"),
		"42":   lir.Int64(42),
		"2.5":  lir.Float64(2.5),
		"NULL": lir.Null(catalog.TypeText),
	}
	for want, v := range cases {
		if got := v.String(); got != want {
			t.Errorf("%+v.String() = %q, want %q", v, got, want)
		}
	}
}

// Rows survive a JSON round trip with types intact. This matters beyond
// convenience: the executor persists rows as JSON-encoded lir.Row, so this
// round trip is the row storage format.
func TestRowJSONRoundTrip(t *testing.T) {
	row := lir.Row{
		"id":    lir.Int64(7),
		"name":  lir.Text("Ada"),
		"score": lir.Float64(99.5),
		"bio":   lir.Null(catalog.TypeText),
		"zero":  lir.Int64(0), // zero payloads must survive omitempty
	}

	raw, err := json.Marshal(row)
	if err != nil {
		t.Fatal(err)
	}
	var got lir.Row
	if err := json.Unmarshal(raw, &got); err != nil {
		t.Fatal(err)
	}

	if len(got) != len(row) {
		t.Fatalf("got %d columns, want %d", len(got), len(row))
	}
	for name, want := range row {
		g := got[name]
		if want.Null {
			if !g.Null || g.Type != want.Type {
				t.Errorf("%s: got %+v, want typed NULL", name, g)
			}
			continue
		}
		if !g.Equal(want) {
			t.Errorf("%s: got %+v, want %+v", name, g, want)
		}
	}
}

// A query is a plain immutable tree of nodes and expressions. This test
// pins the IR's shape: what a frontend builds and what the planner receives.
func TestQueryTreeShape(t *testing.T) {
	q := lir.Query{Root: lir.Limit{
		Input: lir.Project{
			Input: lir.Filter{
				Input: lir.Join{
					Left:  lir.Scan{Table: "users", Alias: "u"},
					Right: lir.Scan{Table: "orders", Alias: "o"},
					Type:  lir.InnerJoin,
					On: lir.Eq{
						Left:  lir.ColRef{Alias: "u", Column: "id"},
						Right: lir.ColRef{Alias: "o", Column: "user_id"},
					},
				},
				Expr: lir.And{Exprs: []lir.Expr{
					lir.Eq{Left: lir.ColRef{Alias: "u", Column: "name"}, Right: lir.Literal{Value: lir.Text("Ada")}},
				}},
			},
			Columns: []lir.Projection{
				{Expr: lir.ColRef{Alias: "o", Column: "id"}, As: "order_id"},
			},
		},
		N: 10,
	}}

	// Walk the tree back down, asserting each level.
	limit, ok := q.Root.(lir.Limit)
	if !ok || limit.N != 10 {
		t.Fatalf("root: %T", q.Root)
	}
	project, ok := limit.Input.(lir.Project)
	if !ok || len(project.Columns) != 1 || project.Columns[0].As != "order_id" {
		t.Fatalf("limit input: %T", limit.Input)
	}
	filter, ok := project.Input.(lir.Filter)
	if !ok {
		t.Fatalf("project input: %T", project.Input)
	}
	join, ok := filter.Input.(lir.Join)
	if !ok || join.Type != lir.InnerJoin {
		t.Fatalf("filter input: %T", filter.Input)
	}
	if scan, ok := join.Left.(lir.Scan); !ok || scan.Table != "users" || scan.Alias != "u" {
		t.Fatalf("join left: %#v", join.Left)
	}
	if on, ok := join.On.(lir.Eq); !ok {
		t.Fatalf("join on: %T", join.On)
	} else if _, ok := on.Left.(lir.ColRef); !ok {
		t.Fatalf("on left: %T", on.Left)
	}
}
