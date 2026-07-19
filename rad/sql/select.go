package sql

import (
	"fmt"
	"strconv"

	"github.com/pgplex/pgparser/nodes"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

type selMode int

const (
	// modeRoot compiles a statement-level SELECT: the pipeline must end
	// deterministically (synthesized order for `many` roots).
	modeRoot selMode = iota
	// modeSub compiles a relation consumed structurally (FROM subselect,
	// CTE body, EXISTS/IN crossing): no ordering obligations of its own.
	modeSub
	// modeScalar compiles a scalar-crossing relation, which the engine
	// requires to be statically at most one row.
	modeScalar
)

// selOut is the compiled shape of one SELECT: the pipeline's root node, the
// output columns in order, and the scope label they are exposed under.
type selOut struct {
	root    string
	cols    []colDef
	scope   string
	one     bool
	ordered bool
	card    string
}

func (c *cc) lowerSelect(e *env, sel *nodes.SelectStmt, mode selMode) (selOut, error) {
	if sel.Op != nodes.SETOP_NONE {
		if sel.Op == nodes.SETOP_UNION {
			return c.lowerUnion(e, sel, mode)
		}
		return selOut{}, unsupportedf("INTERSECT/EXCEPT")
	}
	if sel.ValuesLists != nil {
		return selOut{}, unsupportedf("bare VALUES list")
	}
	if sel.LockingClause != nil && len(sel.LockingClause.Items) > 0 {
		// FOR UPDATE/SHARE: row locking has no meaning here — every program
		// is one serializable transaction. Accepted and ignored.
		_ = sel.LockingClause
	}
	if sel.WindowClause != nil && len(sel.WindowClause.Items) > 0 {
		return selOut{}, unsupportedf("window functions")
	}
	if sel.IntoClause != nil {
		return selOut{}, unsupportedf("SELECT INTO")
	}
	if sel.WithClause != nil {
		if err := c.registerCTEs(sel.WithClause); err != nil {
			return selOut{}, err
		}
	}

	// FROM
	local := &env{parent: e.parent}
	var cur string
	one := false
	if sel.FromClause == nil || len(sel.FromClause.Items) == 0 {
		one = true
		tr := "true"
		cur = c.add(lirwire.Rows(c.scope("one"), []lirwire.RowsColumn{{Name: "one", Type: lirwire.ScalarTypeBool}}, [][]lirwire.Cell{{&tr}}))
	} else {
		var err error
		cur, err = c.lowerFrom(local, sel.FromClause.Items)
		if err != nil {
			return selOut{}, err
		}
	}

	// WHERE
	if sel.WhereClause != nil {
		pred, _, err := c.lowerExpr(local, sel.WhereClause, &boolType)
		if err != nil {
			return selOut{}, err
		}
		cur = c.add(lirwire.Filter(cur, pred))
	}

	// GROUP BY / aggregates
	targets, err := resTargets(sel.TargetList)
	if err != nil {
		return selOut{}, err
	}
	aggCalls := collectAggs(sel.TargetList, sel.HavingClause, sel.SortClause)
	hasGroups := sel.GroupClause != nil && len(sel.GroupClause.Items) > 0
	exprEnv := local
	if hasGroups || len(aggCalls) > 0 {
		if sel.GroupDistinct {
			return selOut{}, unsupportedf("GROUP BY DISTINCT")
		}
		cur, exprEnv, err = c.lowerAggregate(local, cur, sel.GroupClause, aggCalls, targets)
		if err != nil {
			return selOut{}, err
		}
		one = !hasGroups
	}

	// HAVING
	if sel.HavingClause != nil {
		if exprEnv.agg == nil {
			return selOut{}, fmt.Errorf("HAVING without aggregation")
		}
		pred, _, err := c.lowerExpr(exprEnv, sel.HavingClause, &boolType)
		if err != nil {
			return selOut{}, err
		}
		cur = c.add(lirwire.Filter(cur, pred))
		one = false
	}

	// SELECT list
	fields, outCols, spreads, err := c.lowerTargets(exprEnv, targets)
	if err != nil {
		return selOut{}, err
	}

	distinct := sel.DistinctClause != nil && len(sel.DistinctClause.Items) > 0
	if distinct {
		if sel.DistinctClause.Items[0] != nil {
			return selOut{}, unsupportedf("DISTINCT ON")
		}
	}

	ordered := sel.SortClause != nil && len(sel.SortClause.Items) > 0
	limit, err := c.lowerLimitValue(sel.LimitCount)
	if err != nil {
		return selOut{}, err
	}
	offset, err := c.lowerLimitValue(sel.LimitOffset)
	if err != nil {
		return selOut{}, err
	}
	if limit != nil && *limit <= 1 {
		one = true
	}
	if sel.LimitOption == nodes.LIMIT_OPTION_WITH_TIES {
		return selOut{}, unsupportedf("FETCH FIRST WITH TIES")
	}

	outScope := c.scope("q")

	if !distinct {
		// order and slice sit below the final projection, over the FROM (or
		// aggregate) scopes.
		if ordered {
			terms, err := c.lowerSortClause(exprEnv, sel.SortClause, targets, nil, "")
			if err != nil {
				return selOut{}, err
			}
			cur = c.add(lirwire.Order(cur, terms))
		} else if mode == modeRoot && !one {
			terms := c.synthOrderTerms(exprEnv, outCols)
			if len(terms) == 0 {
				return selOut{}, unsupportedf("query with no orderable column")
			}
			cur = c.add(lirwire.Order(cur, terms))
			ordered = true
		}
		if limit != nil || (offset != nil && *offset > 0) {
			cur = c.add(sliceNode(cur, offset, limit))
		}
		cur = c.add(lirwire.Project(cur, outScope, spreads, fields))
	} else {
		cur = c.add(lirwire.Project(cur, outScope, spreads, fields))
		cur = c.add(lirwire.Distinct(cur))
		if ordered {
			terms, err := c.lowerSortClause(exprEnv, sel.SortClause, targets, outCols, outScope)
			if err != nil {
				return selOut{}, err
			}
			cur = c.add(lirwire.Order(cur, terms))
		} else if mode == modeRoot && !one {
			var terms []lirwire.OrderTerm
			for _, cd := range outCols {
				terms = append(terms, lirwire.OrderTerm{Expr: lirwire.Col(outScope, cd.name)})
			}
			if len(terms) == 0 {
				return selOut{}, unsupportedf("DISTINCT query with no orderable column")
			}
			cur = c.add(lirwire.Order(cur, terms))
			ordered = true
		}
		if limit != nil || (offset != nil && *offset > 0) {
			cur = c.add(sliceNode(cur, offset, limit))
		}
	}

	if mode == modeScalar && !one {
		return selOut{}, unsupportedf("scalar subquery not provably single-row (add LIMIT 1 or aggregate)")
	}

	card := "many"
	if one {
		card = "first"
	}
	return selOut{root: cur, cols: outCols, scope: outScope, one: one, ordered: ordered, card: card}, nil
}

func sliceNode(input string, offset, limit *int) lirwire.Node {
	off := 0
	if offset != nil {
		off = *offset
	}
	return lirwire.Slice(input, off, limit)
}

func resTargets(list *nodes.List) ([]*nodes.ResTarget, error) {
	if list == nil {
		return nil, fmt.Errorf("SELECT without a target list")
	}
	out := make([]*nodes.ResTarget, 0, len(list.Items))
	for _, item := range list.Items {
		rt, ok := item.(*nodes.ResTarget)
		if !ok {
			return nil, fmt.Errorf("unexpected target %T", item)
		}
		out = append(out, rt)
	}
	return out, nil
}

// lowerTargets lowers the SELECT list into project fields, expanding stars
// into scope spreads. Output columns are tracked in order with their
// SQL-visible names; LIR field names are sanitized and uniquified.
func (c *cc) lowerTargets(e *env, targets []*nodes.ResTarget) ([]lirwire.Field, []colDef, []string, error) {
	var fields []lirwire.Field
	var out []colDef
	var spreads []string
	seen := map[string]int{}
	uniquify := func(base string) string {
		base = sanitizeIdent(base)
		if base == "" {
			base = "column"
		}
		seen[base]++
		if n := seen[base]; n > 1 {
			return base + "_" + strconv.Itoa(n)
		}
		return base
	}
	for i, rt := range targets {
		if ref, ok := rt.Val.(*nodes.ColumnRef); ok {
			alias, _, star, err := splitColumnRef(ref)
			if err != nil {
				return nil, nil, nil, err
			}
			if star {
				scopes := e.scopes
				if alias != "" {
					var s *scopeDef
					for _, cand := range e.scopes {
						if cand.alias == alias {
							s = cand
							break
						}
					}
					if s == nil {
						return nil, nil, nil, fmt.Errorf("missing FROM-clause entry %q", alias)
					}
					scopes = []*scopeDef{s}
				}
				for _, s := range scopes {
					spreads = append(spreads, s.label)
					for _, cd := range s.cols {
						seen[cd.name]++
						out = append(out, cd)
					}
				}
				continue
			}
		}
		expr, t, err := c.lowerExpr(e, rt.Val, nil)
		if err != nil {
			return nil, nil, nil, err
		}
		name := rt.Name
		if name == "" {
			name = deriveName(rt.Val)
		}
		lirName := uniquify(name)
		fields = append(fields, lirwire.Field{As: lirName, Expr: expr})
		cd := colDef{name: lirName, typ: t}
		if name != lirName {
			cd.wire = name
		}
		out = append(out, cd)
		_ = i
	}
	return fields, out, spreads, nil
}

func deriveName(n nodes.Node) string {
	switch v := n.(type) {
	case *nodes.ColumnRef:
		_, col, _, err := splitColumnRef(v)
		if err == nil && col != "" {
			return col
		}
	case *nodes.FuncCall:
		if name := funcName(v.Funcname); name != "" {
			return name
		}
	case *nodes.TypeCast:
		return deriveName(v.Arg)
	case *nodes.SubLink:
		return "subquery"
	}
	return "column"
}

// lowerAggregate builds the aggregate node and the post-aggregate
// environment whose aggScope rewrites group-key and aggregate-call
// expressions to the node's outputs.
func (c *cc) lowerAggregate(e *env, input string, groupClause *nodes.List, aggCalls []*nodes.FuncCall, targets []*nodes.ResTarget) (string, *env, error) {
	label := c.scope("g")
	byFP := map[string]colDef{}
	var groups []lirwire.GroupTerm
	gi := 0
	used := map[string]bool{}
	// Group-key output names are internal to the aggregate node —
	// post-aggregate expressions resolve through fingerprints, never these
	// names — so colliding keys (GROUP BY a.id, b.id) uniquify freely.
	uniquify := func(base string) string {
		if !used[base] {
			used[base] = true
			return base
		}
		for n := 2; ; n++ {
			cand := base + "_" + strconv.Itoa(n)
			if !used[cand] {
				used[cand] = true
				return cand
			}
		}
	}
	if groupClause != nil {
		for _, item := range groupClause.Items {
			gexpr := item
			// GROUP BY <n> refers to the nth SELECT column.
			if ac, ok := item.(*nodes.A_Const); ok {
				iv, isInt := ac.Val.(*nodes.Integer)
				if isInt {
					idx := int(iv.Ival)
					if idx < 1 || idx > len(targets) {
						return "", nil, fmt.Errorf("GROUP BY position %d out of range", idx)
					}
					gexpr = targets[idx-1].Val
				}
			}
			expr, t, err := c.lowerExpr(e, gexpr, nil)
			if err != nil {
				return "", nil, err
			}
			name := ""
			if ref, ok := gexpr.(*nodes.ColumnRef); ok {
				_, col, star, err := splitColumnRef(ref)
				if err == nil && !star {
					name = col
				}
			}
			if name == "" {
				gi++
				name = "gk" + strconv.Itoa(gi)
			}
			name = uniquify(name)
			as := name
			groups = append(groups, lirwire.GroupTerm{As: &as, Expr: expr})
			byFP[fingerprint(e, gexpr)] = colDef{name: name, typ: t}
		}
	}
	var aggs []lirwire.AggTerm
	ai := 0
	for _, fc := range aggCalls {
		fp := fingerprint(e, fc)
		if _, dup := byFP[fp]; dup {
			continue
		}
		if fc.AggDistinct && !distinctRedundant(e, fc) {
			return "", nil, unsupportedf("aggregate DISTINCT over a non-unique expression")
		}
		if fc.AggFilter != nil || fc.AggOrder != nil || fc.Over != nil {
			return "", nil, unsupportedf("aggregate FILTER/ORDER/OVER")
		}
		fn := aggFuncs[funcName(fc.Funcname)]
		ai++
		as := uniquify("agg" + strconv.Itoa(ai))
		term := lirwire.AggTerm{Fn: fn, As: as}
		t := exprType{}
		switch {
		case fc.AggStar || fc.Args == nil || len(fc.Args.Items) == 0:
			if fn != "count" {
				return "", nil, fmt.Errorf("%s() requires an argument", fn)
			}
			t = exprType{scalar: lirwire.ScalarTypeInt64}
		default:
			if len(fc.Args.Items) != 1 {
				return "", nil, unsupportedf("multi-argument aggregate")
			}
			arg, at, err := c.lowerExpr(e, fc.Args.Items[0], nil)
			if err != nil {
				return "", nil, err
			}
			term.Arg = &arg
			switch fn {
			case "count":
				t = exprType{scalar: lirwire.ScalarTypeInt64}
			case "avg":
				t = exprType{scalar: lirwire.ScalarTypeFloat64, nullable: true}
			case "sum":
				t = exprType{scalar: at.scalar, nullable: true}
			default:
				t = exprType{scalar: at.scalar, format: at.format, nullable: true}
			}
		}
		aggs = append(aggs, term)
		byFP[fp] = colDef{name: as, typ: t}
	}
	node := c.add(lirwire.Aggregate(input, label, groups, aggs))
	aggEnv := &env{
		parent: e.parent,
		agg:    &aggScope{label: label, byFP: byFP, env: e},
	}
	_ = targets
	return node, aggEnv, nil
}

// distinctRedundant reports whether an aggregate's DISTINCT cannot change
// the result: its argument is a single column that is unique in its table
// (the primary key or a single-column unique index). This is ent's
// count(DISTINCT pk) edge-count pattern. Rows multiplied through several
// joins could still repeat a unique column; that multiplicity is accepted.
func distinctRedundant(e *env, fc *nodes.FuncCall) bool {
	if fc.Args == nil || len(fc.Args.Items) != 1 {
		return false
	}
	ref, ok := fc.Args.Items[0].(*nodes.ColumnRef)
	if !ok {
		return false
	}
	alias, col, star, err := splitColumnRef(ref)
	if err != nil || star {
		return false
	}
	scope, cd, err := e.lookup(alias, col)
	if err != nil || scope.table == nil {
		return false
	}
	if len(scope.table.PrimaryKey) == 1 && scope.table.PrimaryKey[0] == cd.name {
		return true
	}
	for _, ix := range scope.table.Indexes {
		if ix.Unique && len(ix.Columns) == 1 && ix.Columns[0] == cd.name {
			return true
		}
	}
	return false
}

// collectAggs finds aggregate calls in the SELECT list, HAVING, and ORDER
// BY, without descending into subqueries (their aggregates are their own).
func collectAggs(targetList *nodes.List, having nodes.Node, sortClause *nodes.List) []*nodes.FuncCall {
	var out []*nodes.FuncCall
	var walk func(n nodes.Node)
	walk = func(n nodes.Node) {
		switch v := n.(type) {
		case nil:
			return
		case *nodes.FuncCall:
			if _, ok := aggFuncs[funcName(v.Funcname)]; ok {
				out = append(out, v)
				return
			}
			if v.Args != nil {
				for _, a := range v.Args.Items {
					walk(a)
				}
			}
		case *nodes.A_Expr:
			walk(v.Lexpr)
			walk(v.Rexpr)
		case *nodes.BoolExpr:
			if v.Args != nil {
				for _, a := range v.Args.Items {
					walk(a)
				}
			}
		case *nodes.NullTest:
			walk(v.Arg)
		case *nodes.BooleanTest:
			walk(v.Arg)
		case *nodes.TypeCast:
			walk(v.Arg)
		case *nodes.ResTarget:
			walk(v.Val)
		case *nodes.SortBy:
			walk(v.Node)
		case *nodes.List:
			for _, item := range v.Items {
				walk(item)
			}
		case *nodes.SubLink:
			// stop: inner aggregates belong to the subquery
		}
	}
	if targetList != nil {
		walk(targetList)
	}
	walk(having)
	if sortClause != nil {
		walk(sortClause)
	}
	return out
}

// lowerSortClause lowers ORDER BY terms. When outCols/outScope are given
// (the DISTINCT case, ordering above the projection), terms must match
// output columns; otherwise terms lower against the pre-projection
// environment, resolving bare output aliases and ordinal positions first.
// Postgres defaults NULLS LAST ascending / NULLS FIRST descending — the
// engine fixes the opposite — so nullable terms get an is_null prefix term
// replicating the requested placement.
func (c *cc) lowerSortClause(e *env, sortClause *nodes.List, targets []*nodes.ResTarget, outCols []colDef, outScope string) ([]lirwire.OrderTerm, error) {
	var terms []lirwire.OrderTerm
	for _, item := range sortClause.Items {
		sb, ok := item.(*nodes.SortBy)
		if !ok {
			return nil, fmt.Errorf("unexpected sort item %T", item)
		}
		if sb.UseOp != nil {
			return nil, unsupportedf("ORDER BY USING")
		}
		node := sb.Node
		// Ordinal: ORDER BY 2.
		if ac, isConst := node.(*nodes.A_Const); isConst {
			if iv, isInt := ac.Val.(*nodes.Integer); isInt {
				idx := int(iv.Ival)
				if idx < 1 || idx > len(targets) {
					return nil, fmt.Errorf("ORDER BY position %d out of range", idx)
				}
				node = targets[idx-1].Val
			}
		}
		// Bare identifier matching an output alias.
		if ref, isRef := node.(*nodes.ColumnRef); isRef {
			if alias, col, star, err := splitColumnRef(ref); err == nil && !star && alias == "" {
				for _, rt := range targets {
					if rt.Name == col {
						node = rt.Val
						break
					}
				}
			}
		}

		var expr lirwire.Expr
		var t exprType
		if outScope != "" {
			fp := fingerprint(e, node)
			found := false
			for _, cd := range outCols {
				if fingerprintOfOutput(e, targets, cd, node) || fp != "!" && fp == fingerprintTarget(e, targets, cd) {
					expr = lirwire.Col(outScope, cd.name)
					t = cd.typ
					found = true
					break
				}
			}
			if !found {
				return nil, fmt.Errorf("for SELECT DISTINCT, ORDER BY expressions must appear in select list")
			}
		} else {
			var err error
			expr, t, err = c.lowerExpr(e, node, nil)
			if err != nil {
				return nil, err
			}
		}

		desc := sb.SortbyDir == nodes.SORTBY_DESC
		nullsFirst := desc
		switch sb.SortbyNulls {
		case nodes.SORTBY_NULLS_FIRST:
			nullsFirst = true
		case nodes.SORTBY_NULLS_LAST:
			nullsFirst = false
		}
		// Engine native placement: first when ascending, last when
		// descending. Emit the is_null prefix only when diverging.
		nativeFirst := !desc
		if t.nullable && nullsFirst != nativeFirst {
			nullDesc := nullsFirst
			terms = append(terms, orderTerm(lirwire.Unary("is_null", expr), nullDesc))
		}
		terms = append(terms, orderTerm(expr, desc))
	}
	return terms, nil
}

func orderTerm(expr lirwire.Expr, desc bool) lirwire.OrderTerm {
	t := lirwire.OrderTerm{Expr: expr}
	if desc {
		t.Desc = &desc
	}
	return t
}

// fingerprintTarget returns the fingerprint of the target whose output
// column is cd, or "!" when unknown.
func fingerprintTarget(e *env, targets []*nodes.ResTarget, cd colDef) string {
	for _, rt := range targets {
		name := rt.Name
		if name == "" {
			name = deriveName(rt.Val)
		}
		if sanitizeIdent(name) == cd.name {
			return fingerprint(e, rt.Val)
		}
	}
	return "!"
}

func fingerprintOfOutput(e *env, targets []*nodes.ResTarget, cd colDef, node nodes.Node) bool {
	fp := fingerprint(e, node)
	if fp == "!" {
		return false
	}
	return fp == fingerprintTarget(e, targets, cd)
}

// synthOrderTerms builds a deterministic ordering when SQL supplied none:
// primary keys of scanned tables, group keys above an aggregate, otherwise
// every column of every scope.
func (c *cc) synthOrderTerms(e *env, outCols []colDef) []lirwire.OrderTerm {
	var terms []lirwire.OrderTerm
	if e.agg != nil {
		for _, cd := range e.agg.byFP {
			terms = append(terms, lirwire.OrderTerm{Expr: lirwire.Col(e.agg.label, cd.name)})
		}
		return terms
	}
	for _, s := range e.scopes {
		if s.table != nil && len(s.table.PrimaryKey) > 0 {
			for _, pk := range s.table.PrimaryKey {
				terms = append(terms, lirwire.OrderTerm{Expr: lirwire.Col(s.label, pk)})
			}
			continue
		}
		for _, cd := range s.cols {
			terms = append(terms, lirwire.OrderTerm{Expr: lirwire.Col(s.label, cd.name)})
		}
	}
	return terms
}

func (c *cc) lowerLimitValue(n nodes.Node) (*int, error) {
	switch v := n.(type) {
	case nil:
		return nil, nil
	case *nodes.A_Const:
		if v.Isnull {
			return nil, nil
		}
		iv, ok := v.Val.(*nodes.Integer)
		if !ok {
			return nil, fmt.Errorf("LIMIT/OFFSET must be an integer")
		}
		i := int(iv.Ival)
		return &i, nil
	case *nodes.ParamRef:
		intT := exprType{scalar: lirwire.ScalarTypeInt64}
		expr, _, err := c.lowerParam(v, &intT)
		if err != nil {
			return nil, err
		}
		if c.args == nil {
			zero := 0
			return &zero, nil
		}
		i, err := int64ValueOf(expr)
		if err != nil {
			return nil, fmt.Errorf("LIMIT/OFFSET parameter: %w", err)
		}
		v2 := int(i)
		return &v2, nil
	}
	return nil, unsupportedf("computed LIMIT/OFFSET")
}

// int64ValueOf extracts the integer from a lowered literal expression.
func int64ValueOf(e lirwire.Expr) (int64, error) {
	lit, ok := e.ExprUnion.(*lirwire.LiteralExpr)
	if !ok {
		return 0, fmt.Errorf("not a literal")
	}
	iv, ok := lit.Value.ValueUnion.(*lirwire.Int64Value)
	if !ok || iv.Value == nil {
		return 0, fmt.Errorf("not an int64 literal")
	}
	return strconv.ParseInt(*iv.Value, 10, 64)
}
