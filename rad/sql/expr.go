package sql

import (
	"fmt"
	"strconv"
	"strings"
	"time"

	"github.com/pgplex/pgparser/nodes"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

var boolType = exprType{scalar: lirwire.ScalarTypeBool}

// lowerExpr lowers one scalar expression. want is a coercion hint consumed
// only by flexible leaves (literals, parameters, NULL): it never overrides
// the type of a column or computed expression.
func (c *cc) lowerExpr(e *env, n nodes.Node, want *exprType) (lirwire.Expr, exprType, error) {
	if e.agg != nil {
		if col, ok := e.agg.byFP[fingerprint(e.agg.env, n)]; ok {
			return lirwire.Col(e.agg.label, col.name), col.typ, nil
		}
	}
	switch v := n.(type) {
	case *nodes.ColumnRef:
		return c.lowerColumnRef(e, v)
	case *nodes.A_Const:
		return c.lowerConst(v, want)
	case *nodes.ParamRef:
		return c.lowerParam(v, want)
	case *nodes.A_Expr:
		return c.lowerAExpr(e, v)
	case *nodes.BoolExpr:
		return c.lowerBoolExpr(e, v)
	case *nodes.NullTest:
		arg, _, err := c.lowerExpr(e, v.Arg, nil)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		op := "is_null"
		if v.Nulltesttype == nodes.IS_NOT_NULL {
			op = "is_not_null"
		}
		return lirwire.Unary(op, arg), boolType, nil
	case *nodes.BooleanTest:
		return c.lowerBooleanTest(e, v)
	case *nodes.TypeCast:
		return c.lowerTypeCast(e, v)
	case *nodes.FuncCall:
		return c.lowerFuncCall(e, v)
	case *nodes.SQLValueFunction:
		return c.lowerSQLValueFunction(v)
	case *nodes.SubLink:
		return c.lowerSubLink(e, v)
	case *nodes.List:
		return lirwire.Expr{}, exprType{}, unsupportedf("row expression")
	case *nodes.CaseExpr:
		return c.lowerCase(e, v)
	case *nodes.CoalesceExpr:
		return c.lowerCoalesce(e, v)
	case *nodes.MinMaxExpr:
		return lirwire.Expr{}, exprType{}, unsupportedf("GREATEST/LEAST")
	case *nodes.A_Indirection:
		return lirwire.Expr{}, exprType{}, unsupportedf("subscript/field indirection")
	case *nodes.RowExpr:
		return lirwire.Expr{}, exprType{}, unsupportedf("ROW expression")
	}
	return lirwire.Expr{}, exprType{}, unsupportedf("expression %T", n)
}

func (c *cc) lowerColumnRef(e *env, ref *nodes.ColumnRef) (lirwire.Expr, exprType, error) {
	alias, col, star, err := splitColumnRef(ref)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	if star {
		return lirwire.Expr{}, exprType{}, fmt.Errorf("star reference outside SELECT list")
	}
	scope, cd, err := e.lookup(alias, col)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	return lirwire.Col(scope.label, cd.name), cd.typ, nil
}

// splitColumnRef decomposes a ColumnRef into (alias, column, isStar).
// Three-part names keep the last two components; the schema qualifier is
// dropped (everything lives in one namespace).
func splitColumnRef(ref *nodes.ColumnRef) (string, string, bool, error) {
	items := ref.Fields.Items
	if len(items) == 0 {
		return "", "", false, fmt.Errorf("empty column reference")
	}
	if len(items) > 3 {
		items = items[len(items)-3:]
	}
	parts := make([]string, 0, len(items))
	star := false
	for i, it := range items {
		switch f := it.(type) {
		case *nodes.String:
			parts = append(parts, f.Str)
		case *nodes.A_Star:
			if i != len(items)-1 {
				return "", "", false, fmt.Errorf("misplaced * in column reference")
			}
			star = true
		default:
			return "", "", false, unsupportedf("column reference component %T", it)
		}
	}
	switch {
	case star && len(parts) == 0:
		return "", "", true, nil
	case star:
		return parts[len(parts)-1], "", true, nil
	case len(parts) == 1:
		return "", parts[0], false, nil
	default:
		return parts[len(parts)-2], parts[len(parts)-1], false, nil
	}
}

func (c *cc) lowerConst(a *nodes.A_Const, want *exprType) (lirwire.Expr, exprType, error) {
	if a.Isnull {
		if want == nil {
			return lirwire.Expr{}, exprType{}, fmt.Errorf("NULL literal requires a typed context")
		}
		t := exprType{scalar: want.scalar, format: want.format, nullable: true}
		return lirwire.Lit(lirwire.Null(want.scalar)), t, nil
	}
	switch v := a.Val.(type) {
	case *nodes.Integer:
		if want != nil && want.scalar == lirwire.ScalarTypeFloat64 {
			return lirwire.Lit(lirwire.Float64(float64(v.Ival))), exprType{scalar: lirwire.ScalarTypeFloat64}, nil
		}
		return lirwire.Lit(lirwire.Int64(v.Ival)), exprType{scalar: lirwire.ScalarTypeInt64}, nil
	case *nodes.Float:
		if want != nil && want.scalar == lirwire.ScalarTypeInt64 {
			if i, err := strconv.ParseInt(v.Fval, 10, 64); err == nil {
				return lirwire.Lit(lirwire.Int64(i)), exprType{scalar: lirwire.ScalarTypeInt64}, nil
			}
		}
		f, err := strconv.ParseFloat(v.Fval, 64)
		if err != nil {
			return lirwire.Expr{}, exprType{}, fmt.Errorf("bad numeric literal %q", v.Fval)
		}
		return lirwire.Lit(lirwire.Float64(f)), exprType{scalar: lirwire.ScalarTypeFloat64}, nil
	case *nodes.String:
		return c.lowerStringLiteral(v.Str, want)
	case *nodes.Boolean:
		return lirwire.Lit(lirwire.Bool(v.Boolval)), boolType, nil
	}
	return lirwire.Expr{}, exprType{}, unsupportedf("literal %T", a.Val)
}

// lowerStringLiteral coerces a string literal into the wanted scalar the way
// Postgres treats unknown-type literals: timestamps parse to microseconds,
// numerics and bools parse from text, everything else stays text.
func (c *cc) lowerStringLiteral(s string, want *exprType) (lirwire.Expr, exprType, error) {
	if want == nil {
		return lirwire.Lit(lirwire.Text(s)), exprType{scalar: lirwire.ScalarTypeText}, nil
	}
	switch want.scalar {
	case lirwire.ScalarTypeText:
		return lirwire.Lit(lirwire.Text(s)), exprType{scalar: lirwire.ScalarTypeText, format: want.format}, nil
	case lirwire.ScalarTypeInt64:
		if IsTimeFormat(want.format) {
			us, err := ParseTimestamp(s)
			if err != nil {
				return lirwire.Expr{}, exprType{}, err
			}
			return lirwire.Lit(lirwire.Int64(us)), exprType{scalar: lirwire.ScalarTypeInt64, format: want.format}, nil
		}
		i, err := strconv.ParseInt(strings.TrimSpace(s), 10, 64)
		if err != nil {
			return lirwire.Expr{}, exprType{}, fmt.Errorf("invalid integer literal %q", s)
		}
		return lirwire.Lit(lirwire.Int64(i)), exprType{scalar: lirwire.ScalarTypeInt64}, nil
	case lirwire.ScalarTypeFloat64:
		f, err := strconv.ParseFloat(strings.TrimSpace(s), 64)
		if err != nil {
			return lirwire.Expr{}, exprType{}, fmt.Errorf("invalid numeric literal %q", s)
		}
		return lirwire.Lit(lirwire.Float64(f)), exprType{scalar: lirwire.ScalarTypeFloat64}, nil
	case lirwire.ScalarTypeBool:
		switch strings.ToLower(strings.TrimSpace(s)) {
		case "t", "true", "yes", "on", "1":
			return lirwire.Lit(lirwire.Bool(true)), boolType, nil
		case "f", "false", "no", "off", "0":
			return lirwire.Lit(lirwire.Bool(false)), boolType, nil
		}
		return lirwire.Expr{}, exprType{}, fmt.Errorf("invalid boolean literal %q", s)
	}
	return lirwire.Lit(lirwire.Text(s)), exprType{scalar: lirwire.ScalarTypeText}, nil
}

func (c *cc) lowerParam(p *nodes.ParamRef, want *exprType) (lirwire.Expr, exprType, error) {
	if p.Number <= 0 {
		return lirwire.Expr{}, exprType{}, fmt.Errorf("parameter reference without a number")
	}
	if want != nil {
		if err := c.params.infer(p.Number, *want); err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
	} else {
		c.params.grow(p.Number)
	}
	pt := c.params.typeOf(p.Number)
	t := exprType{scalar: pt.Scalar, format: pt.Format, nullable: true}
	if c.args == nil {
		return lirwire.Lit(lirwire.Null(pt.Scalar)), t, nil
	}
	if p.Number > len(c.args) {
		return lirwire.Expr{}, exprType{}, fmt.Errorf("parameter $%d has no bound value", p.Number)
	}
	return lirwire.Lit(c.args[p.Number-1]), t, nil
}

// flexible reports whether a node's type is dictated by context rather than
// structure, so the other operand of a comparison should lower first.
func flexible(n nodes.Node) bool {
	switch n.(type) {
	case *nodes.A_Const, *nodes.ParamRef:
		return true
	}
	return false
}

var binaryOps = map[string]string{
	"=": "eq", "<>": "ne", "!=": "ne",
	"<": "lt", "<=": "lte", ">": "gt", ">=": "gte",
	"+": "add", "-": "sub", "*": "mul", "/": "div",
}

func isComparison(op string) bool {
	switch op {
	case "eq", "ne", "lt", "lte", "gt", "gte":
		return true
	}
	return false
}

func (c *cc) lowerAExpr(e *env, a *nodes.A_Expr) (lirwire.Expr, exprType, error) {
	switch a.Kind {
	case nodes.AEXPR_OP:
		return c.lowerOpExpr(e, a)
	case nodes.AEXPR_IN:
		return c.lowerInList(e, a)
	case nodes.AEXPR_BETWEEN, nodes.AEXPR_NOT_BETWEEN:
		return c.lowerBetween(e, a)
	case nodes.AEXPR_LIKE:
		return c.lowerLike(e, a)
	case nodes.AEXPR_ILIKE:
		return c.lowerLike(e, a)
	case nodes.AEXPR_SIMILAR:
		return lirwire.Expr{}, exprType{}, unsupportedf("SIMILAR TO")
	case nodes.AEXPR_DISTINCT, nodes.AEXPR_NOT_DISTINCT:
		return lirwire.Expr{}, exprType{}, unsupportedf("IS DISTINCT FROM")
	case nodes.AEXPR_NULLIF:
		return c.lowerNullIf(e, a)
	}
	return lirwire.Expr{}, exprType{}, unsupportedf("operator expression kind %d", a.Kind)
}

func operatorName(list *nodes.List) (string, error) {
	if list == nil || len(list.Items) == 0 {
		return "", fmt.Errorf("operator without a name")
	}
	s, ok := list.Items[len(list.Items)-1].(*nodes.String)
	if !ok {
		return "", fmt.Errorf("unexpected operator name node %T", list.Items[len(list.Items)-1])
	}
	return s.Str, nil
}

func (c *cc) lowerOpExpr(e *env, a *nodes.A_Expr) (lirwire.Expr, exprType, error) {
	name, err := operatorName(a.Name)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	if a.Lexpr == nil {
		if name != "-" {
			return lirwire.Expr{}, exprType{}, unsupportedf("unary operator %q", name)
		}
		arg, at, err := c.lowerExpr(e, a.Rexpr, nil)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		return lirwire.Unary("negate", arg), at, nil
	}
	op, ok := binaryOps[name]
	if !ok {
		return lirwire.Expr{}, exprType{}, unsupportedf("operator %q", name)
	}
	le, lt, re, rt, err := c.lowerOperands(e, a.Lexpr, a.Rexpr)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	if isComparison(op) {
		le, lt, re, rt, err = alignComparison(le, lt, re, rt)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		return lirwire.Binary(op, le, re), exprType{scalar: lirwire.ScalarTypeBool, nullable: lt.nullable || rt.nullable}, nil
	}
	// Arithmetic: both sides numeric; the engine widens int64 to float64.
	if !numeric(lt.scalar) || !numeric(rt.scalar) {
		return lirwire.Expr{}, exprType{}, unsupportedf("operator %q on %s and %s", name, lt.scalar, rt.scalar)
	}
	out := lirwire.ScalarTypeInt64
	if lt.scalar == lirwire.ScalarTypeFloat64 || rt.scalar == lirwire.ScalarTypeFloat64 {
		out = lirwire.ScalarTypeFloat64
	}
	format := lt.format
	if format == "" {
		format = rt.format
	}
	return lirwire.Binary(op, le, re), exprType{scalar: out, format: format, nullable: lt.nullable || rt.nullable}, nil
}

// lowerOperands lowers a binary expression's sides, feeding the concrete
// side's type to a flexible side as its coercion hint.
func (c *cc) lowerOperands(e *env, l, r nodes.Node) (lirwire.Expr, exprType, lirwire.Expr, exprType, error) {
	switch {
	case flexible(l) && !flexible(r):
		re, rt, err := c.lowerExpr(e, r, nil)
		if err != nil {
			return lirwire.Expr{}, exprType{}, lirwire.Expr{}, exprType{}, err
		}
		le, lt, err := c.lowerExpr(e, l, &rt)
		return le, lt, re, rt, err
	case !flexible(l) && flexible(r):
		le, lt, err := c.lowerExpr(e, l, nil)
		if err != nil {
			return lirwire.Expr{}, exprType{}, lirwire.Expr{}, exprType{}, err
		}
		re, rt, err := c.lowerExpr(e, r, &lt)
		return le, lt, re, rt, err
	default:
		le, lt, err := c.lowerExpr(e, l, nil)
		if err != nil {
			return lirwire.Expr{}, exprType{}, lirwire.Expr{}, exprType{}, err
		}
		re, rt, err := c.lowerExpr(e, r, &lt)
		return le, lt, re, rt, err
	}
}

func numeric(s lirwire.ScalarType) bool {
	return s == lirwire.ScalarTypeInt64 || s == lirwire.ScalarTypeFloat64
}

// alignComparison makes both sides of a comparison the same scalar kind,
// inserting the engine's only implicit-cast-free widening (int64→float64)
// where Postgres would compare numerics directly.
func alignComparison(le lirwire.Expr, lt exprType, re lirwire.Expr, rt exprType) (lirwire.Expr, exprType, lirwire.Expr, exprType, error) {
	if lt.scalar == rt.scalar {
		return le, lt, re, rt, nil
	}
	if lt.scalar == lirwire.ScalarTypeInt64 && rt.scalar == lirwire.ScalarTypeFloat64 {
		le = lirwire.Cast(le, lirwire.ScalarTypeFloat64)
		lt.scalar = lirwire.ScalarTypeFloat64
		return le, lt, re, rt, nil
	}
	if lt.scalar == lirwire.ScalarTypeFloat64 && rt.scalar == lirwire.ScalarTypeInt64 {
		re = lirwire.Cast(re, lirwire.ScalarTypeFloat64)
		rt.scalar = lirwire.ScalarTypeFloat64
		return le, lt, re, rt, nil
	}
	return le, lt, re, rt, unsupportedf("comparison between %s and %s", lt.scalar, rt.scalar)
}

// lowerInList lowers `x [NOT] IN (a, b, ...)` to a fold of equalities. The
// list elements coerce to x's type.
func (c *cc) lowerInList(e *env, a *nodes.A_Expr) (lirwire.Expr, exprType, error) {
	name, err := operatorName(a.Name)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	list, ok := a.Rexpr.(*nodes.List)
	if !ok {
		return lirwire.Expr{}, exprType{}, unsupportedf("IN over %T", a.Rexpr)
	}
	le, lt, err := c.lowerExpr(e, a.Lexpr, nil)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	var preds []*lirwire.Expr
	for _, item := range list.Items {
		re, rt, err := c.lowerExpr(e, item, &lt)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		el, _, er, _, err := alignComparison(le, lt, re, rt)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		eq := lirwire.Binary("eq", el, er)
		preds = append(preds, &eq)
	}
	if len(preds) == 0 {
		return lirwire.Lit(lirwire.Bool(false)), boolType, nil
	}
	out := *preds[0]
	for _, p := range preds[1:] {
		out = lirwire.Binary("or", out, *p)
	}
	if name == "<>" {
		out = lirwire.Unary("not", out)
	}
	return out, exprType{scalar: lirwire.ScalarTypeBool, nullable: lt.nullable}, nil
}

func (c *cc) lowerBetween(e *env, a *nodes.A_Expr) (lirwire.Expr, exprType, error) {
	list, ok := a.Rexpr.(*nodes.List)
	if !ok || len(list.Items) != 2 {
		return lirwire.Expr{}, exprType{}, fmt.Errorf("malformed BETWEEN")
	}
	le, lt, err := c.lowerExpr(e, a.Lexpr, nil)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	lo, lot, err := c.lowerExpr(e, list.Items[0], &lt)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	hi, hit, err := c.lowerExpr(e, list.Items[1], &lt)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	l1, _, lo1, _, err := alignComparison(le, lt, lo, lot)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	l2, _, hi1, _, err := alignComparison(le, lt, hi, hit)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	out := lirwire.Binary("and",
		lirwire.Binary("gte", l1, lo1),
		lirwire.Binary("lte", l2, hi1),
	)
	if a.Kind == nodes.AEXPR_NOT_BETWEEN {
		out = lirwire.Unary("not", out)
	}
	return out, exprType{scalar: lirwire.ScalarTypeBool, nullable: lt.nullable}, nil
}

func (c *cc) lowerBoolExpr(e *env, b *nodes.BoolExpr) (lirwire.Expr, exprType, error) {
	if b.Boolop == nodes.NOT_EXPR {
		if len(b.Args.Items) != 1 {
			return lirwire.Expr{}, exprType{}, fmt.Errorf("malformed NOT")
		}
		arg, at, err := c.lowerExpr(e, b.Args.Items[0], &boolType)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		return lirwire.Unary("not", arg), exprType{scalar: lirwire.ScalarTypeBool, nullable: at.nullable}, nil
	}
	op := "and"
	if b.Boolop == nodes.OR_EXPR {
		op = "or"
	}
	var out lirwire.Expr
	nullable := false
	for i, item := range b.Args.Items {
		arg, at, err := c.lowerExpr(e, item, &boolType)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		nullable = nullable || at.nullable
		if i == 0 {
			out = arg
			continue
		}
		out = lirwire.Binary(op, out, arg)
	}
	return out, exprType{scalar: lirwire.ScalarTypeBool, nullable: nullable}, nil
}

// lowerBooleanTest lowers IS [NOT] TRUE/FALSE/UNKNOWN as total predicates
// (never UNKNOWN), matching SQL semantics.
func (c *cc) lowerBooleanTest(e *env, b *nodes.BooleanTest) (lirwire.Expr, exprType, error) {
	arg, _, err := c.lowerExpr(e, b.Arg, &boolType)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	notNull := lirwire.Unary("is_not_null", arg)
	isTrue := lirwire.Binary("and", notNull, arg)
	isFalse := lirwire.Binary("and", notNull, lirwire.Unary("not", arg))
	var out lirwire.Expr
	switch b.Booltesttype {
	case nodes.IS_TRUE:
		out = isTrue
	case nodes.IS_NOT_TRUE:
		out = lirwire.Unary("not", isTrue)
	case nodes.IS_FALSE:
		out = isFalse
	case nodes.IS_NOT_FALSE:
		out = lirwire.Unary("not", isFalse)
	case nodes.IS_UNKNOWN:
		out = lirwire.Unary("is_null", arg)
	case nodes.IS_NOT_UNKNOWN:
		out = lirwire.Unary("is_not_null", arg)
	default:
		return lirwire.Expr{}, exprType{}, unsupportedf("boolean test %d", b.Booltesttype)
	}
	return out, boolType, nil
}

func (c *cc) lowerTypeCast(e *env, tc *nodes.TypeCast) (lirwire.Expr, exprType, error) {
	scalar, format, err := typeNameOf(tc.TypeName)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	target := exprType{scalar: scalar, format: format}
	if flexible(tc.Arg) {
		expr, t, err := c.lowerExpr(e, tc.Arg, &target)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		t.format = format
		return expr, t, nil
	}
	expr, t, err := c.lowerExpr(e, tc.Arg, nil)
	if err != nil {
		return lirwire.Expr{}, exprType{}, err
	}
	switch {
	case t.scalar == scalar:
		t.format = format
		return expr, t, nil
	case numeric(t.scalar) && numeric(scalar):
		t.scalar = scalar
		t.format = format
		return lirwire.Cast(expr, scalar), t, nil
	}
	return lirwire.Expr{}, exprType{}, unsupportedf("cast from %s to %s", t.scalar, scalar)
}

// typeNameOf resolves a grammar TypeName to the rad scalar + format pair.
func typeNameOf(tn *nodes.TypeName) (lirwire.ScalarType, string, error) {
	if tn == nil || tn.Names == nil || len(tn.Names.Items) == 0 {
		return "", "", fmt.Errorf("missing type name")
	}
	if tn.ArrayBounds != nil && len(tn.ArrayBounds.Items) > 0 {
		return "", "", unsupportedf("array type")
	}
	last, ok := tn.Names.Items[len(tn.Names.Items)-1].(*nodes.String)
	if !ok {
		return "", "", fmt.Errorf("unexpected type name node")
	}
	return pgTypeName(last.Str)
}

func funcName(list *nodes.List) string {
	if list == nil || len(list.Items) == 0 {
		return ""
	}
	if s, ok := list.Items[len(list.Items)-1].(*nodes.String); ok {
		return strings.ToLower(s.Str)
	}
	return ""
}

var aggFuncs = map[string]string{
	"count": "count", "sum": "sum", "avg": "avg", "min": "min", "max": "max",
}

// lowerFuncCall handles the few scalar functions the frontend evaluates
// itself. Aggregate calls never reach here: inside an aggregating SELECT
// they are rewritten to aggregate-output columns via the aggScope.
func (c *cc) lowerFuncCall(e *env, fc *nodes.FuncCall) (lirwire.Expr, exprType, error) {
	name := funcName(fc.Funcname)
	switch name {
	case "now", "transaction_timestamp", "statement_timestamp", "clock_timestamp":
		return c.nowLiteral(), exprType{scalar: lirwire.ScalarTypeInt64, format: FormatTimestampTZ}, nil
	}
	if _, ok := aggFuncs[name]; ok {
		return lirwire.Expr{}, exprType{}, fmt.Errorf("aggregate %s() not allowed here", name)
	}
	return lirwire.Expr{}, exprType{}, unsupportedf("function %s()", name)
}

// nowLiteral inlines the statement's wall-clock time. The frontend, not the
// engine, evaluates volatile time functions; one SQL statement therefore
// sees one timestamp, matching Postgres statement semantics closely enough
// for a translation layer.
func (c *cc) nowLiteral() lirwire.Expr {
	return lirwire.Lit(lirwire.Int64(time.Now().UTC().UnixMicro()))
}

func (c *cc) lowerSQLValueFunction(v *nodes.SQLValueFunction) (lirwire.Expr, exprType, error) {
	switch v.Op {
	case nodes.SVFOP_CURRENT_TIMESTAMP, nodes.SVFOP_CURRENT_TIMESTAMP_N,
		nodes.SVFOP_LOCALTIMESTAMP, nodes.SVFOP_LOCALTIMESTAMP_N:
		return c.nowLiteral(), exprType{scalar: lirwire.ScalarTypeInt64, format: FormatTimestampTZ}, nil
	case nodes.SVFOP_CURRENT_DATE:
		day := time.Now().UTC().Truncate(24 * time.Hour).UnixMicro()
		return lirwire.Lit(lirwire.Int64(day)), exprType{scalar: lirwire.ScalarTypeInt64, format: FormatDate}, nil
	}
	return lirwire.Expr{}, exprType{}, unsupportedf("SQL value function %d", v.Op)
}

func (c *cc) lowerSubLink(e *env, sl *nodes.SubLink) (lirwire.Expr, exprType, error) {
	sub, ok := sl.Subselect.(*nodes.SelectStmt)
	if !ok {
		return lirwire.Expr{}, exprType{}, unsupportedf("subquery %T", sl.Subselect)
	}
	switch nodes.SubLinkType(sl.SubLinkType) {
	case nodes.EXISTS_SUBLINK:
		out, err := c.lowerSelect(&env{parent: e}, sub, modeSub)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		return lirwire.Exists(out.root), boolType, nil
	case nodes.ANY_SUBLINK:
		if sl.OperName != nil {
			if name, err := operatorName(sl.OperName); err != nil || name != "=" {
				return lirwire.Expr{}, exprType{}, unsupportedf("ANY with operator")
			}
		}
		out, err := c.lowerSelect(&env{parent: e}, sub, modeSub)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		if len(out.cols) != 1 {
			return lirwire.Expr{}, exprType{}, fmt.Errorf("IN subquery must return one column")
		}
		test, tt, err := c.lowerExpr(e, sl.Testexpr, &out.cols[0].typ)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		subCol := lirwire.Col(out.scope, out.cols[0].name)
		sc, _, tc2, _, err := alignComparison(subCol, out.cols[0].typ, test, tt)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		filtered := c.add(lirwire.Filter(out.root, lirwire.Binary("eq", sc, tc2)))
		return lirwire.Exists(filtered), boolType, nil
	case nodes.EXPR_SUBLINK:
		out, err := c.lowerSelect(&env{parent: e}, sub, modeScalar)
		if err != nil {
			return lirwire.Expr{}, exprType{}, err
		}
		if len(out.cols) != 1 {
			return lirwire.Expr{}, exprType{}, fmt.Errorf("scalar subquery must return one column")
		}
		t := out.cols[0].typ
		t.nullable = true
		return lirwire.Scalar(out.root), t, nil
	}
	return lirwire.Expr{}, exprType{}, unsupportedf("subquery link type %d", sl.SubLinkType)
}

// fingerprint produces a structural identity for matching post-aggregate
// expressions against GROUP BY keys and collected aggregate calls. Column
// references normalize through the environment so qualified and unqualified
// spellings of the same column match. Unknown node kinds never match.
func fingerprint(e *env, n nodes.Node) string {
	switch v := n.(type) {
	case *nodes.ColumnRef:
		alias, col, star, err := splitColumnRef(v)
		if err != nil || star {
			return "!"
		}
		scope, cd, err := e.lookup(alias, col)
		if err != nil {
			return "!"
		}
		return "c:" + scope.label + "." + cd.name
	case *nodes.A_Const:
		if v.Isnull {
			return "l:null"
		}
		switch val := v.Val.(type) {
		case *nodes.Integer:
			return "l:i:" + strconv.FormatInt(val.Ival, 10)
		case *nodes.Float:
			return "l:f:" + val.Fval
		case *nodes.String:
			return "l:s:" + val.Str
		case *nodes.Boolean:
			return "l:b:" + strconv.FormatBool(val.Boolval)
		}
		return "!"
	case *nodes.ParamRef:
		return "p:" + strconv.Itoa(v.Number)
	case *nodes.FuncCall:
		parts := []string{"f:" + funcName(v.Funcname)}
		if v.AggStar {
			parts = append(parts, "*")
		}
		if v.AggDistinct {
			parts = append(parts, "distinct")
		}
		if v.Args != nil {
			for _, a := range v.Args.Items {
				parts = append(parts, fingerprint(e, a))
			}
		}
		return strings.Join(parts, ",")
	case *nodes.A_Expr:
		name, err := operatorName(v.Name)
		if err != nil {
			return "!"
		}
		l, r := "", ""
		if v.Lexpr != nil {
			l = fingerprint(e, v.Lexpr)
		}
		if v.Rexpr != nil {
			r = fingerprint(e, v.Rexpr)
		}
		return fmt.Sprintf("op:%d:%s(%s,%s)", v.Kind, name, l, r)
	case *nodes.TypeCast:
		scalar, format, err := typeNameOf(v.TypeName)
		if err != nil {
			return "!"
		}
		return "cast:" + string(scalar) + ":" + format + ":" + fingerprint(e, v.Arg)
	case *nodes.NullTest:
		return fmt.Sprintf("nt:%d:%s", v.Nulltesttype, fingerprint(e, v.Arg))
	}
	return "!"
}
