package sql

import (
	"fmt"
	"strings"

	"github.com/pgplex/pgparser/nodes"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// registerCTEs compiles WITH bindings. A CTE body is uncorrelated (Postgres
// gives it no access to enclosing scopes), so each compiles under a fresh
// environment. A recursive CTE must be `anchor UNION [ALL] step`: UNION ALL
// keeps every generated row (accumulation "all"), UNION deduplicates
// (accumulation "new"); the step's self-reference lowers to the binding's
// frontier.
func (c *cc) registerCTEs(w *nodes.WithClause) error {
	if w.Ctes == nil {
		return nil
	}
	for _, item := range w.Ctes.Items {
		cte, ok := item.(*nodes.CommonTableExpr)
		if !ok {
			return fmt.Errorf("unexpected WITH item %T", item)
		}
		name := strings.ToLower(cte.Ctename)
		if _, dup := c.ctes[name]; dup {
			return fmt.Errorf("WITH name %q used twice", name)
		}
		body, ok := cte.Ctequery.(*nodes.SelectStmt)
		if !ok {
			return unsupportedf("non-SELECT CTE body %T", cte.Ctequery)
		}

		if body.Op == nodes.SETOP_UNION && w.Recursive && referencesName(body.Rarg, name) {
			if err := c.registerRecursiveCTE(name, cte, body); err != nil {
				return err
			}
			continue
		}
		out, err := c.lowerSelect(&env{}, body, modeSub)
		if err != nil {
			return err
		}
		root, cols := out.root, out.cols
		if cte.Aliascolnames != nil && len(cte.Aliascolnames.Items) > 0 {
			root, cols, _, err = c.renameProject(root, out.scope, cols, cte.Aliascolnames)
			if err != nil {
				return err
			}
		}
		c.bindings[name] = lirwire.Derived(root)
		c.ctes[name] = &cteDef{name: name, cols: cols}
	}
	return nil
}

// referencesName reports whether a select tree's FROM clauses reference
// the given relation name. WITH RECURSIVE marks the whole clause, so a
// member that never references itself is an ordinary (possibly UNION)
// binding, not a recursive one.
func referencesName(n nodes.Node, name string) bool {
	switch v := n.(type) {
	case nil:
		return false
	case *nodes.SelectStmt:
		// A leaf statement's Larg/Rarg are typed-nil pointers, which the
		// nil case above does not catch.
		if v == nil {
			return false
		}
		if referencesName(v.Larg, name) || referencesName(v.Rarg, name) {
			return true
		}
		if v.FromClause != nil {
			for _, item := range v.FromClause.Items {
				if referencesName(item, name) {
					return true
				}
			}
		}
		return false
	case *nodes.RangeVar:
		return v != nil && strings.ToLower(v.Relname) == name
	case *nodes.RangeSubselect:
		return v != nil && referencesName(v.Subquery, name)
	case *nodes.JoinExpr:
		return v != nil && (referencesName(v.Larg, name) || referencesName(v.Rarg, name))
	}
	return false
}

func (c *cc) registerRecursiveCTE(name string, cte *nodes.CommonTableExpr, body *nodes.SelectStmt) error {
	if c.rec != nil {
		return unsupportedf("nested recursive CTEs")
	}
	if body.Larg == nil || body.Rarg == nil {
		return fmt.Errorf("malformed recursive CTE %q", name)
	}
	anchorOut, err := c.lowerSelect(&env{}, body.Larg, modeSub)
	if err != nil {
		return err
	}
	anchorRoot, cols := anchorOut.root, anchorOut.cols
	if cte.Aliascolnames != nil && len(cte.Aliascolnames.Items) > 0 {
		anchorRoot, cols, _, err = c.renameProject(anchorRoot, anchorOut.scope, cols, cte.Aliascolnames)
		if err != nil {
			return err
		}
	}

	c.rec = &recCTE{name: name, cols: cols}
	stepOut, err := c.lowerSelect(&env{}, body.Rarg, modeSub)
	c.rec = nil
	if err != nil {
		return err
	}
	if len(stepOut.cols) != len(cols) {
		return fmt.Errorf("recursive CTE %q: anchor has %d columns, step has %d", name, len(cols), len(stepOut.cols))
	}
	stepRoot := stepOut.root
	// The step's output must carry the anchor's column names.
	rename := false
	for i := range cols {
		if stepOut.cols[i].name != cols[i].name {
			rename = true
			break
		}
	}
	if rename {
		label := c.scope(name + "_step")
		fields := make([]lirwire.Field, 0, len(cols))
		for i, cd := range cols {
			fields = append(fields, lirwire.Field{As: cd.name, Expr: lirwire.Col(stepOut.scope, stepOut.cols[i].name)})
		}
		stepRoot = c.add(lirwire.Project(stepRoot, label, nil, fields))
	}

	accumulation := "new"
	if body.All {
		accumulation = "all"
	}
	c.bindings[name] = lirwire.Recursive(anchorRoot, stepRoot, accumulation)
	c.ctes[name] = &cteDef{name: name, cols: cols}
	return nil
}
