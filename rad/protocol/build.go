package protocol

// Constructors for building query trees. Generated clients and the runtime
// assemble graphs through these so emitted code stays small and readable.

// Col references a column of a scope.
func Col(scope, column string) *Expr {
	return &Expr{Kind: "col", Scope: scope, Column: column}
}

// Lit is a literal value; the server types it by the column it meets.
func Lit(v any) *Expr { return &Expr{Kind: "lit", Value: v} }

func binary(op string, l, r *Expr) *Expr {
	return &Expr{Kind: "binary", Op: op, Left: l, Right: r}
}

func Eq(l, r *Expr) *Expr  { return binary("eq", l, r) }
func Ne(l, r *Expr) *Expr  { return binary("ne", l, r) }
func Lt(l, r *Expr) *Expr  { return binary("lt", l, r) }
func Lte(l, r *Expr) *Expr { return binary("lte", l, r) }
func Gt(l, r *Expr) *Expr  { return binary("gt", l, r) }
func Gte(l, r *Expr) *Expr { return binary("gte", l, r) }
func Or(l, r *Expr) *Expr  { return binary("or", l, r) }

func Add(l, r *Expr) *Expr { return binary("add", l, r) }
func Sub(l, r *Expr) *Expr { return binary("sub", l, r) }
func Mul(l, r *Expr) *Expr { return binary("mul", l, r) }
func Div(l, r *Expr) *Expr { return binary("div", l, r) }

func Not(e *Expr) *Expr       { return &Expr{Kind: "unary", Op: "not", Expr: e} }
func Neg(e *Expr) *Expr       { return &Expr{Kind: "unary", Op: "negate", Expr: e} }
func IsNull(e *Expr) *Expr    { return &Expr{Kind: "unary", Op: "is_null", Expr: e} }
func IsNotNull(e *Expr) *Expr { return &Expr{Kind: "unary", Op: "is_not_null", Expr: e} }

// CastTo converts to a scalar kind: "int64", "float64", "text", "bool".
func CastTo(e *Expr, kind string) *Expr { return &Expr{Kind: "cast", Expr: e, To: kind} }

// AndAll left-folds predicates into a binary and-chain: nil for none, the
// predicate itself for one.
func AndAll(exprs []*Expr) *Expr {
	var out *Expr
	for _, e := range exprs {
		if out == nil {
			out = e
		} else {
			out = binary("and", out, e)
		}
	}
	return out
}

// Crossings: the doors from a relation node into an expression.

func Exists(node string) *Expr  { return &Expr{Kind: "exists", Node: node} }
func FirstOf(node string) *Expr { return &Expr{Kind: "first", Node: node} }
func ScalarOf(node string) *Expr {
	return &Expr{Kind: "scalar", Node: node}
}
func ArrayOf(node string) *Expr { return &Expr{Kind: "array", Node: node} }
