package sql

import (
	"fmt"
	"strings"

	"github.com/pgplex/pgparser/nodes"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// lowerFrom lowers a FROM clause into one relation, folding comma-separated
// items into cross joins. Scopes register into e as they lower so join
// predicates and later clauses can resolve them.
func (c *cc) lowerFrom(e *env, items []nodes.Node) (string, error) {
	var cur string
	for i, item := range items {
		root, err := c.lowerFromItem(e, item)
		if err != nil {
			return "", err
		}
		if i == 0 {
			cur = root
			continue
		}
		cur = c.add(lirwire.Join(cur, root, "inner", lirwire.Lit(lirwire.Bool(true))))
	}
	return cur, nil
}

func (c *cc) lowerFromItem(e *env, item nodes.Node) (string, error) {
	switch v := item.(type) {
	case *nodes.RangeVar:
		return c.lowerRangeVar(e, v)
	case *nodes.RangeSubselect:
		return c.lowerRangeSubselect(e, v)
	case *nodes.JoinExpr:
		return c.lowerJoinExpr(e, v)
	}
	return "", unsupportedf("FROM item %T", item)
}

func (c *cc) lowerRangeVar(e *env, rv *nodes.RangeVar) (string, error) {
	name := strings.ToLower(rv.Relname)
	alias := name
	if rv.Alias != nil && rv.Alias.Aliasname != "" {
		alias = strings.ToLower(rv.Alias.Aliasname)
	}

	// The frontier of the recursive CTE currently being compiled.
	if c.rec != nil && name == c.rec.name {
		label := c.scope(alias)
		root := c.add(lirwire.RecursiveRef(c.rec.name, label))
		e.scopes = append(e.scopes, &scopeDef{alias: alias, label: label, cols: c.rec.cols})
		return root, nil
	}

	// CTE bindings shadow tables.
	if cte, ok := c.ctes[name]; ok {
		label := c.scope(alias)
		root := c.add(lirwire.Ref(name, label))
		e.scopes = append(e.scopes, &scopeDef{alias: alias, label: label, cols: cte.cols})
		return root, nil
	}

	table, ok := c.schema.Table(name)
	if !ok {
		return "", fmt.Errorf("relation %q does not exist", name)
	}
	label := c.scope(alias)
	root := c.add(lirwire.Scan(table.Name, label))
	cols := make([]colDef, 0, len(table.Columns))
	for _, col := range table.Columns {
		cols = append(cols, colDef{
			name: col.Name,
			typ:  exprType{scalar: col.Scalar, format: col.Format, nullable: col.Nullable},
		})
	}
	e.scopes = append(e.scopes, &scopeDef{alias: alias, label: label, cols: cols, table: table})
	return root, nil
}

func (c *cc) lowerRangeSubselect(e *env, rs *nodes.RangeSubselect) (string, error) {
	if rs.Lateral {
		return "", unsupportedf("LATERAL")
	}
	sub, ok := rs.Subquery.(*nodes.SelectStmt)
	if !ok {
		return "", unsupportedf("FROM subquery %T", rs.Subquery)
	}
	if rs.Alias == nil || rs.Alias.Aliasname == "" {
		return "", fmt.Errorf("subquery in FROM must have an alias")
	}
	alias := strings.ToLower(rs.Alias.Aliasname)
	out, err := c.lowerSelect(&env{parent: e.parent}, sub, modeSub)
	if err != nil {
		return "", err
	}
	root, cols, label := out.root, out.cols, out.scope
	if rs.Alias.Colnames != nil && len(rs.Alias.Colnames.Items) > 0 {
		root, cols, label, err = c.renameProject(root, label, cols, rs.Alias.Colnames)
		if err != nil {
			return "", err
		}
	}
	e.scopes = append(e.scopes, &scopeDef{alias: alias, label: label, cols: cols})
	return root, nil
}

func (c *cc) lowerJoinExpr(e *env, je *nodes.JoinExpr) (string, error) {
	if je.IsNatural {
		return "", unsupportedf("NATURAL join")
	}
	if je.UsingClause != nil && len(je.UsingClause.Items) > 0 {
		return "", unsupportedf("JOIN USING")
	}
	kind := ""
	left, right := je.Larg, je.Rarg
	switch je.Jointype {
	case nodes.JOIN_INNER:
		kind = "inner"
	case nodes.JOIN_LEFT:
		kind = "left"
	case nodes.JOIN_RIGHT:
		// A right join is a left join with the sides swapped.
		kind = "left"
		left, right = right, left
	case nodes.JOIN_FULL:
		return "", unsupportedf("FULL OUTER JOIN")
	default:
		return "", unsupportedf("join type %d", je.Jointype)
	}
	lroot, err := c.lowerFromItem(e, left)
	if err != nil {
		return "", err
	}
	rroot, err := c.lowerFromItem(e, right)
	if err != nil {
		return "", err
	}
	on := lirwire.Lit(lirwire.Bool(true))
	if je.Quals != nil {
		on, _, err = c.lowerExpr(e, je.Quals, &boolType)
		if err != nil {
			return "", err
		}
	}
	return c.add(lirwire.Join(lroot, rroot, kind, on)), nil
}

// renameProject wraps a relation in a projection renaming its columns
// positionally (CTE and subquery alias column lists).
func (c *cc) renameProject(root, label string, cols []colDef, colnames *nodes.List) (string, []colDef, string, error) {
	if len(colnames.Items) > len(cols) {
		return "", nil, "", fmt.Errorf("column alias list has %d names, relation has %d columns", len(colnames.Items), len(cols))
	}
	newLabel := c.scope(label)
	fields := make([]lirwire.Field, 0, len(cols))
	renamed := make([]colDef, 0, len(cols))
	for i, cd := range cols {
		name := cd.name
		if i < len(colnames.Items) {
			s, ok := colnames.Items[i].(*nodes.String)
			if !ok {
				return "", nil, "", fmt.Errorf("unexpected column alias node %T", colnames.Items[i])
			}
			name = sanitizeIdent(s.Str)
			if name == "" {
				return "", nil, "", fmt.Errorf("empty column alias")
			}
		}
		fields = append(fields, lirwire.Field{As: name, Expr: lirwire.Col(label, cd.name)})
		renamed = append(renamed, colDef{name: name, typ: cd.typ})
	}
	node := c.add(lirwire.Project(root, newLabel, nil, fields))
	return node, renamed, newLabel, nil
}
