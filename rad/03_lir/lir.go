// Package lir is Rad's logical intermediate representation (RADLIR): the
// structured, semantic query model that every frontend (Go structs today;
// SQL, GraphQL, ORMs later) lowers into, and that the planner consumes.
//
// This package is deliberately sacred. It expresses pure relational
// semantics — relations, expressions, projections, filters, joins, limits,
// and eventually mutations, aggregates, transactions, and parameters — and
// nothing else. Nothing here may know about storage (SlateDB, keys,
// encodings), physical access paths (indexes), or execution strategy (hash
// vs merge vs nested-loop joins). Those concerns belong to the planner
// (04_planner) and executor (05_exec), which consume this package.
//
// It may import 00_types for the shared value vocabulary and, ideally,
// nothing else — it currently honors that.
//
// During execution rows use alias-qualified column names, e.g. "u.id".
//
// TODO: IndexScan predates the planner and names a physical access path; it
// should migrate out of the IR once the planner does access-path selection
// from Filter predicates.
package lir

type Query struct {
	Root RelNode
}

type RelNode interface {
	relNode()
}

// Scan reads every row of a table in primary key order.
type Scan struct {
	Table string
	Alias string
}

// IndexScan reads rows whose indexed values match Prefix (leading columns of
// the index; empty scans the whole index).
type IndexScan struct {
	Table  string
	Alias  string
	Index  string
	Prefix Row
}

type Filter struct {
	Input RelNode
	Expr  Expr
}

type Project struct {
	Input   RelNode
	Columns []Projection
}

type Join struct {
	Left  RelNode
	Right RelNode
	Type  JoinType
	On    Expr
}

type Limit struct {
	Input RelNode
	N     int
}

func (Scan) relNode()      {}
func (IndexScan) relNode() {}
func (Filter) relNode()    {}
func (Project) relNode()   {}
func (Join) relNode()      {}
func (Limit) relNode()     {}

type JoinType string

const (
	InnerJoin JoinType = "inner"
)

type Expr interface {
	expr()
}

type Eq struct {
	Left  Expr
	Right Expr
}

// CmpOp is a comparison operator for Cmp.
type CmpOp string

const (
	OpNe  CmpOp = "!="
	OpLt  CmpOp = "<"
	OpLte CmpOp = "<="
	OpGt  CmpOp = ">"
	OpGte CmpOp = ">="
)

// Cmp compares two values with Op. Comparisons involving NULL are false.
type Cmp struct {
	Op    CmpOp
	Left  Expr
	Right Expr
}

type And struct {
	Exprs []Expr
}

type Or struct {
	Exprs []Expr
}

type Not struct {
	Expr Expr
}

// IsNull is true when Expr evaluates to NULL — the only way to match NULLs,
// since Eq/Cmp on NULL are always false.
type IsNull struct {
	Expr Expr
}

// ColRef references a column of an aliased input, e.g. {Alias: "u", Column: "id"}.
type ColRef struct {
	Alias  string
	Column string
}

type Literal struct {
	Value Value
}

func (Eq) expr()      {}
func (Cmp) expr()     {}
func (And) expr()     {}
func (Or) expr()      {}
func (Not) expr()     {}
func (IsNull) expr()  {}
func (ColRef) expr()  {}
func (Literal) expr() {}

type Projection struct {
	Expr Expr
	As   string
}
