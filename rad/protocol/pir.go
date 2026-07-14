package protocol

// PIR — the program intermediate representation. A program is an ordered list
// of named statements run as one atomic unit; each statement carries an LIR
// Query as its relational body. These are the ergonomic handwritten types;
// the Schemancer-generated transport (pirwire) is the strict tagged union,
// and pir.schema.yaml is the normative spec.

// StatementKind names the four statement kinds.
type StatementKind string

const (
	StmtQuery  StatementKind = "query"
	StmtCreate StatementKind = "create"
	StmtUpdate StatementKind = "update"
	StmtDelete StatementKind = "delete"
)

// Program is a complete execution program: statements in execution order plus
// an optional result selector. Result may be empty only for a single-statement
// program, whose sole statement is then the result.
type Program struct {
	Statements []Statement `json:"statements"`
	Result     string      `json:"result,omitempty"`
}

// Statement is one statement of a program. Kind selects the behaviour; Name is
// unique within the program and is the label under which the statement's result
// enters the program binding namespace. Table is set for mutation kinds only.
// Relation is the statement's LIR body.
type Statement struct {
	Name     string        `json:"name"`
	Kind     StatementKind `json:"kind"`
	Table    string        `json:"table,omitempty"`
	Relation Query         `json:"relation"`
}

// Query builds a single-statement read program: the common case, with no
// result selector needed.
func QueryProgram(rel Query) Program {
	return Program{Statements: []Statement{{Name: "result", Kind: StmtQuery, Relation: rel}}}
}

// Read constructs a query statement.
func Read(name string, rel Query) Statement {
	return Statement{Name: name, Kind: StmtQuery, Relation: rel}
}

// Create constructs a create statement inserting rel's rows into table.
func Create(name, table string, rel Query) Statement {
	return Statement{Name: name, Kind: StmtCreate, Table: table, Relation: rel}
}

// Update constructs an update statement: rel's schema identifies (by primary
// key) and assigns the target rows of table.
func Update(name, table string, rel Query) Statement {
	return Statement{Name: name, Kind: StmtUpdate, Table: table, Relation: rel}
}

// Delete constructs a delete statement: rel's primary-key rows identify the
// rows of table to remove.
func Delete(name, table string, rel Query) Statement {
	return Statement{Name: name, Kind: StmtDelete, Table: table, Relation: rel}
}
