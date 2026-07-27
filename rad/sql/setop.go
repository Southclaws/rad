package sql

import (
	"fmt"

	"github.com/pgplex/pgparser/nodes"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// lowerUnion compiles a UNION [ALL] tree: each branch projects into one
// canonical column shape, branches combine through concat, and UNION's
// set semantics come from a distinct above the concat. ORDER BY and
// LIMIT/OFFSET on the union apply to the combined relation and may only
// reference output column names or ordinals.
func (c *cc) lowerUnion(e *env, sel *nodes.SelectStmt, mode selMode) (selOut, error) {
	if sel.WithClause != nil {
		if err := c.registerCTEs(sel.WithClause); err != nil {
			return selOut{}, err
		}
	}
	out, err := c.lowerUnionTree(e, sel)
	if err != nil {
		return selOut{}, err
	}

	ordered := false
	if sel.SortClause != nil && len(sel.SortClause.Items) > 0 {
		terms, err := c.lowerUnionSort(sel.SortClause, out)
		if err != nil {
			return selOut{}, err
		}
		out.root = c.add(lirwire.Order(out.root, terms))
		ordered = true
	}

	limit, err := c.lowerLimitValue(sel.LimitCount)
	if err != nil {
		return selOut{}, err
	}
	offset, err := c.lowerLimitValue(sel.LimitOffset)
	if err != nil {
		return selOut{}, err
	}
	if limit != nil && *limit <= 1 {
		out.one = true
	}

	if mode == modeRoot && !ordered && !out.one {
		var terms []lirwire.OrderTerm
		for _, cd := range out.cols {
			terms = append(terms, lirwire.OrderTerm{Expr: lirwire.Col(out.scope, cd.name)})
		}
		if len(terms) == 0 {
			return selOut{}, unsupportedf("union with no orderable column")
		}
		out.root = c.add(lirwire.Order(out.root, terms))
		ordered = true
	}
	if limit != nil || (offset != nil && *offset > 0) {
		out.root = c.add(sliceNode(out.root, offset, limit))
	}

	out.ordered = ordered
	out.card = "many"
	if out.one {
		out.card = "first"
	}
	if mode == modeScalar && !out.one {
		return selOut{}, unsupportedf("scalar subquery not provably single-row (add LIMIT 1)")
	}
	return out, nil
}

// lowerUnionTree lowers one node of the union tree: a leaf SELECT, or a
// union whose branches align positionally onto the left branch's column
// names before combining.
func (c *cc) lowerUnionTree(e *env, sel *nodes.SelectStmt) (selOut, error) {
	if sel.Op == nodes.SETOP_NONE {
		return c.lowerSelect(&env{parent: e.parent}, sel, modeSub)
	}
	if sel.Op != nodes.SETOP_UNION {
		return selOut{}, unsupportedf("INTERSECT/EXCEPT")
	}
	if sel.Larg == nil || sel.Rarg == nil {
		return selOut{}, fmt.Errorf("malformed set operation")
	}
	l, err := c.lowerUnionTree(e, sel.Larg)
	if err != nil {
		return selOut{}, err
	}
	r, err := c.lowerUnionTree(e, sel.Rarg)
	if err != nil {
		return selOut{}, err
	}
	if len(l.cols) != len(r.cols) {
		return selOut{}, fmt.Errorf("each UNION query must have the same number of columns (%d vs %d)", len(l.cols), len(r.cols))
	}

	// Align the right branch onto the left's shape: names always (concat
	// requires positional name equality), plus int64→float64 widening when
	// the branches disagree numerically.
	lCast := make([]bool, len(l.cols))
	rCast := make([]bool, len(r.cols))
	needL, needR := false, false
	for i := range l.cols {
		lt, rt := l.cols[i].typ, r.cols[i].typ
		switch {
		case lt.scalar == rt.scalar:
		case numeric(lt.scalar) && numeric(rt.scalar):
			if lt.scalar == lirwire.ScalarTypeInt64 {
				lCast[i] = true
				needL = true
				l.cols[i].typ.scalar = lirwire.ScalarTypeFloat64
			} else {
				rCast[i] = true
				needR = true
			}
		default:
			return selOut{}, fmt.Errorf("UNION column %d has mismatched types %s and %s", i+1, lt.scalar, rt.scalar)
		}
		if r.cols[i].name != l.cols[i].name {
			needR = true
		}
	}
	if needL {
		l = c.alignBranchTo(l, l.cols, lCast)
	}
	if needR {
		r = c.alignBranchTo(r, l.cols, rCast)
	}

	scope := c.scope("u")
	node := c.add(lirwire.Concatenate(scope, l.root, r.root))
	cols := make([]colDef, len(l.cols))
	for i := range l.cols {
		cols[i] = colDef{
			name: l.cols[i].name,
			wire: l.cols[i].wire,
			typ: exprType{
				scalar:   l.cols[i].typ.scalar,
				format:   l.cols[i].typ.format,
				nullable: l.cols[i].typ.nullable || r.cols[i].typ.nullable,
			},
		}
	}
	out := selOut{root: node, cols: cols, scope: scope}
	if !sel.All {
		out.root = c.add(lirwire.Distinct(node))
	}
	return out, nil
}

// alignBranchTo wraps a branch in a projection renaming its columns
// positionally onto target names and applying flagged float64 casts.
func (c *cc) alignBranchTo(b selOut, target []colDef, cast []bool) selOut {
	label := c.scope(b.scope)
	fields := make([]lirwire.Field, len(b.cols))
	cols := make([]colDef, len(b.cols))
	for i, cd := range b.cols {
		expr := lirwire.Col(b.scope, cd.name)
		typ := cd.typ
		if cast[i] {
			expr = lirwire.Cast(expr, lirwire.ScalarTypeFloat64)
			typ.scalar = lirwire.ScalarTypeFloat64
		}
		fields[i] = lirwire.Field{As: target[i].name, Expr: expr}
		cols[i] = colDef{name: target[i].name, typ: typ}
	}
	b.root = c.add(lirwire.Project(b.root, label, nil, fields))
	b.scope = label
	b.cols = cols
	return b
}

// lowerUnionSort resolves union-level ORDER BY terms against the combined
// output columns (names or ordinals only, per SQL scoping for set ops).
func (c *cc) lowerUnionSort(sortClause *nodes.List, out selOut) ([]lirwire.OrderTerm, error) {
	var terms []lirwire.OrderTerm
	for _, item := range sortClause.Items {
		sb, ok := item.(*nodes.SortBy)
		if !ok {
			return nil, fmt.Errorf("unexpected sort item %T", item)
		}
		var cd *colDef
		switch v := sb.Node.(type) {
		case *nodes.A_Const:
			iv, isInt := v.Val.(*nodes.Integer)
			if !isInt {
				return nil, unsupportedf("ORDER BY expression on UNION")
			}
			idx := int(iv.Ival)
			if idx < 1 || idx > len(out.cols) {
				return nil, fmt.Errorf("ORDER BY position %d out of range", idx)
			}
			cd = &out.cols[idx-1]
		case *nodes.ColumnRef:
			alias, col, star, err := splitColumnRef(v)
			if err != nil || star || alias != "" {
				return nil, unsupportedf("ORDER BY expression on UNION")
			}
			for i := range out.cols {
				if out.cols[i].name == col {
					cd = &out.cols[i]
					break
				}
			}
			if cd == nil {
				return nil, fmt.Errorf("column %q does not exist", col)
			}
		default:
			return nil, unsupportedf("ORDER BY expression on UNION")
		}
		expr := lirwire.Col(out.scope, cd.name)
		desc := sb.SortbyDir == nodes.SORTBY_DESC
		nullsFirst := desc
		switch sb.SortbyNulls {
		case nodes.SORTBY_NULLS_FIRST:
			nullsFirst = true
		case nodes.SORTBY_NULLS_LAST:
			nullsFirst = false
		}
		if cd.typ.nullable && nullsFirst != !desc {
			terms = append(terms, orderTerm(lirwire.Unary("is_null", expr), nullsFirst))
		}
		terms = append(terms, orderTerm(expr, desc))
	}
	return terms, nil
}
