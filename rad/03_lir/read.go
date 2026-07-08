package lir

// Read is the shaped-read form of the IR: one root relation with filters,
// ordering, pagination, and nested relationship includes. It is what the
// generated client and the JSON API speak, and what applications mean by
// "get/list" — the planner chooses access paths (PK lookup, index scan,
// full scan) from the filter; nothing here names an index.
//
// Expressions inside a Read reference the enclosing relation's own columns
// with a bare ColRef (empty Alias).
type Read struct {
	Table   string
	Filter  Expr // optional
	OrderBy []OrderTerm
	Offset  int
	Limit   int // 0 = unlimited
	Include []Include
}

// OrderTerm sorts by one column. NULLs sort first ascending, last
// descending.
type OrderTerm struct {
	Column string
	Desc   bool
}

// Direction names which side of a foreign key an Include traverses.
type Direction string

const (
	// ToParent follows a foreign key on this relation to the single row it
	// references (an object in the result; null when the FK is NULL).
	ToParent Direction = "parent"
	// ToChildren follows a foreign key on another table that references
	// this relation (an array in the result).
	ToChildren Direction = "children"
)

// Include embeds a related relation in each result record under the name
// As. The relation is identified by the foreign key's schema name — the
// generated client supplies it; humans use the ORM's derived names.
type Include struct {
	FK  string // foreign key name, e.g. "tasks_board_id_fk"
	Dir Direction
	As  string // output field name

	// Children-only refinements.
	Filter  Expr
	OrderBy []OrderTerm
	Limit   int // 0 = unlimited
	Include []Include
}
