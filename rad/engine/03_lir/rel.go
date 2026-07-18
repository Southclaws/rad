package lir

// Unbound relations. Operators consume relations and produce relations;
// the output of every node is usable as an ordinary input (relational
// closure). Scan always binds a scope; Project and Aggregate — the nodes
// that establish new row types — bind one when later operators reference
// their outputs by name. Correlation needs no node: a sub-relation that
// references a scope it does not define is correlated, as a property the
// binder derives.

// Relation is the sealed unbound relation interface.
type Relation interface{ rel() }

// Scan introduces a table and binds a scope label, unique per query.
type Scan struct {
	Table string
	Scope string
}

// RowsCol declares one column of a constant relation. Type and nullability
// are explicit, never inferred from the cells — a relation's schema must
// not depend on its data, and a bare JSON number cannot choose between
// int64 and float64.
type RowsCol struct {
	Name     string
	Kind     Kind
	Nullable bool
}

// Rows introduces a finite constant relation — the second leaf beside Scan.
// Values holds raw cell scalars, validated and decoded against the declared
// column types under the same rules as scalar literals; each row's arity
// must equal Columns. The relation is a bag of exactly len(Values) rows;
// its contents are deterministic, but document order is not a logical
// order.
type Rows struct {
	Scope   string
	Columns []RowsCol
	Values  [][]any
}

// Filter keeps rows where Pred evaluates to TRUE — not UNKNOWN. This is the
// load-bearing three-valued-logic rule.
type Filter struct {
	Input Relation
	Pred  Expr
}

// ProjField is one output attribute of a projection.
type ProjField struct {
	As   string
	Expr Expr
}

// Project establishes a new row type: the spread scopes' columns first (all
// columns of each listed scope, in declaration order), then the computed
// fields. Name collisions — explicit×explicit, explicit×spread,
// spread×spread — are rejected at bind time.
type Project struct {
	Input  Relation
	Scope  string // optional output scope; required if outputs are referenced by name
	Spread []string
	Fields []ProjField
}

// JoinKind enumerates supported join behavior.
type JoinKind string

const (
	InnerJoin JoinKind = "inner"
	LeftJoin  JoinKind = "left"
)

// Join combines two relations on a boolean condition. The logical node
// never names an execution strategy.
type Join struct {
	Left, Right Relation
	Kind        JoinKind
	On          Expr
}

// AggFn enumerates the aggregate folds.
type AggFn string

const (
	AggCount AggFn = "count"
	AggSum   AggFn = "sum"
	AggAvg   AggFn = "avg"
	AggMin   AggFn = "min"
	AggMax   AggFn = "max"
)

// AggTerm is one fold: count (Arg nil = count rows, else count non-NULL),
// or sum/avg/min/max over Arg. As names the output attribute.
type AggTerm struct {
	Fn  AggFn
	Arg Expr
	As  string
}

// GroupTerm is one grouping expression. As names the output attribute; when
// empty and Expr is a bare column reference, the binder defaults it to the
// column name.
type GroupTerm struct {
	As   string
	Expr Expr
}

// Aggregate folds Input: one output row per distinct group, or — with no
// groups — exactly one row (the global fold; count of nothing is 0, every
// other fold of nothing is NULL). There is no separate "fold mode":
// grouping is the same node, and GROUP BY is this node with groups.
type Aggregate struct {
	Input  Relation
	Scope  string // optional output scope, as on Project
	Groups []GroupTerm
	Terms  []AggTerm
}

// OrderTerm is one ordering term. NULLs sort first ascending, last
// descending.
type OrderTerm struct {
	Expr Expr
	Desc bool
}

// Order gives the relation a logical ordering. Scan encounter order is
// physical and never observable; when the output carries a known unique
// key the binder appends it as a deterministic tie-breaker.
type Order struct {
	Input Relation
	Terms []OrderTerm
}

// Slice keeps Limit rows after skipping Offset. A nil Limit is unlimited;
// an explicit 0 keeps no rows. A positive offset requires an ordered input;
// an unordered limit is allowed only until an observable boundary imposes its
// own ordering requirement.
type Slice struct {
	Input  Relation
	Offset int
	Limit  *int
}

// Ref is one occurrence of a named binding: a fresh variable ranging over
// the binding's committed value, exactly as a scan is a fresh variable over
// a table's snapshot. The occurrence exposes the binding's output columns
// under Scope; the binding's interior scopes are not visible through it.
type Ref struct {
	Binding string
	Scope   string
}

// RecursiveRef is one occurrence of the enclosing recursive binding's
// frontier: the rows the previous iteration produced, exposed under a fresh
// scope. Legal only inside that binding's step, where it observes the
// frontier — not the accumulated result, which an ordinary Ref observes from
// outside the binding.
type RecursiveRef struct {
	Binding string
	Scope   string
}

// RecursiveAccumulationMode controls how a recursive step's rows enter the
// fixpoint result and the next frontier: every generated row (`all`), or only
// rows not already admitted, by canonical full-row identity (`new`). It is the
// recursive analogue of — but a separate concept from — the unary Distinct
// operator.
type RecursiveAccumulationMode string

const (
	AccumulateAll RecursiveAccumulationMode = "all"
	AccumulateNew RecursiveAccumulationMode = "new"
)

// Recursive is a recursively-defined relation, valid only as a binding body.
// Anchor is the base case (no RecursiveRef); Step is the inductive case and
// contains exactly one RecursiveRef to the binding. Accumulation is the
// admission policy for generated rows.
type Recursive struct {
	Anchor       Relation
	Step         Relation
	Accumulation RecursiveAccumulationMode
}

// Distinct removes duplicate complete rows from Input by canonical full-row
// identity (NULL == NULL, type- and order-significant, unlike three-valued
// predicate equality). It preserves the input's row type and imposes no
// ordering — a bag becomes the corresponding set of complete rows.
type Distinct struct {
	Input Relation
}

func (Scan) rel()         {}
func (Rows) rel()         {}
func (Filter) rel()       {}
func (Project) rel()      {}
func (Join) rel()         {}
func (Aggregate) rel()    {}
func (Order) rel()        {}
func (Slice) rel()        {}
func (Ref) rel()          {}
func (RecursiveRef) rel() {}
func (Recursive) rel()    {}
func (Distinct) rel()     {}

// RootCard states how the root relation materialises on the wire.
type RootCard string

const (
	CardMany       RootCard = "many"        // array of objects
	CardFirst      RootCard = "first"       // object or null
	CardExactlyOne RootCard = "exactly_one" // object, error otherwise
	CardScalar     RootCard = "scalar"      // single value or null; root arity 1, at most one row
)

// Query is a root relation plus its materialisation mode, and the named
// bindings its Ref occurrences observe. Each binding denotes one
// statement-local committed relational value.
type Query struct {
	Root     Relation
	Card     RootCard
	Bindings map[string]Relation
}
