package lir_test

import (
	"fmt"

	lir "rad/rad/03_lir"
)

// A frontend expresses "the orders of user 1, id and total only" as a plain
// tree of IR nodes. Note what is absent: no index names, no join strategy,
// no keys — those are the planner's and executor's business.
func Example() {
	q := lir.Query{Root: lir.Project{
		Input: lir.Filter{
			Input: lir.Scan{Table: "orders", Alias: "o"},
			Expr: lir.Eq{
				Left:  lir.ColRef{Alias: "o", Column: "user_id"},
				Right: lir.Literal{Value: lir.Int64(1)},
			},
		},
		Columns: []lir.Projection{
			{Expr: lir.ColRef{Alias: "o", Column: "id"}, As: "order_id"},
			{Expr: lir.ColRef{Alias: "o", Column: "total"}, As: "total"},
		},
	}}

	project := q.Root.(lir.Project)
	filter := project.Input.(lir.Filter)
	fmt.Println("root:", fmt.Sprintf("%T", q.Root))
	fmt.Println("filter compares to:", filter.Expr.(lir.Eq).Right.(lir.Literal).Value)
	fmt.Println("output columns:", project.Columns[0].As+", "+project.Columns[1].As)
	// Output:
	// root: lir.Project
	// filter compares to: 1
	// output columns: order_id, total
}
