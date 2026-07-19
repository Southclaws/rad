package sql

import (
	"fmt"
	"strconv"
	"strings"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// cc is the per-statement compile context: the LIR node graph under
// construction, name generators, CTE bindings, and parameter state. A fresh
// cc is used for every statement of a program and for every Compile call —
// prepare-mode lowering (nil args) infers parameter types; execute-mode
// lowering re-runs with decoded argument values inlined as literals.
type cc struct {
	schema   *Schema
	nodes    map[string]lirwire.Node
	bindings map[string]lirwire.Binding
	nextID   int
	scopeSeq map[string]int
	params   *paramTracker
	args     []lirwire.Value
	ctes     map[string]*cteDef
	rec      *recCTE
}

// cteDef records a compiled WITH binding and its output shape.
type cteDef struct {
	name string
	cols []colDef
}

// recCTE is the recursive CTE currently being compiled; while set, a
// FROM reference to its name lowers to a recursive_ref over the frontier.
type recCTE struct {
	name string
	cols []colDef
}

func newCC(schema *Schema, params *paramTracker, args []lirwire.Value) *cc {
	return &cc{
		schema:   schema,
		nodes:    map[string]lirwire.Node{},
		bindings: map[string]lirwire.Binding{},
		scopeSeq: map[string]int{},
		params:   params,
		args:     args,
		ctes:     map[string]*cteDef{},
	}
}

func (c *cc) add(n lirwire.Node) string {
	c.nextID++
	id := "n" + strconv.Itoa(c.nextID)
	c.nodes[id] = n
	return id
}

// scope returns a fresh scope label, derived from the SQL alias where one
// exists so plans stay readable. Labels are unique across the whole query.
func (c *cc) scope(base string) string {
	base = sanitizeIdent(base)
	if base == "" {
		base = "s"
	}
	c.scopeSeq[base]++
	if n := c.scopeSeq[base]; n > 1 {
		return base + "_" + strconv.Itoa(n)
	}
	return base
}

func (c *cc) query(root string, card string) lirwire.Query {
	q := lirwire.Query{Nodes: c.nodes, Root: lirwire.Root{Node: root, Cardinality: card}}
	if len(c.bindings) > 0 {
		q.Bindings = c.bindings
	}
	return q
}

// sanitizeIdent folds a SQL identifier into the engine's lowercase
// snake-case identifier space.
func sanitizeIdent(s string) string {
	s = strings.ToLower(s)
	var b strings.Builder
	for _, r := range s {
		switch {
		case r >= 'a' && r <= 'z', r >= '0' && r <= '9', r == '_':
			b.WriteRune(r)
		}
	}
	out := b.String()
	if out != "" && out[0] >= '0' && out[0] <= '9' {
		out = "_" + out
	}
	return out
}

// paramTracker accumulates $N parameter types during prepare-mode lowering.
// Types are assigned from context (comparison operands, insert targets,
// casts, LIMIT); the first assignment wins and a conflicting later
// assignment is an error. Parameters that never receive context default to
// text.
type paramTracker struct {
	types []ParamType
	known []bool
}

func (p *paramTracker) grow(n int) {
	for len(p.types) < n {
		p.types = append(p.types, ParamType{Scalar: lirwire.ScalarTypeText})
		p.known = append(p.known, false)
	}
}

func (p *paramTracker) infer(n int, t exprType) error {
	p.grow(n)
	i := n - 1
	if !p.known[i] {
		p.types[i] = ParamType{Scalar: t.scalar, Format: t.format}
		p.known[i] = true
		return nil
	}
	if p.types[i].Scalar != t.scalar {
		return fmt.Errorf("parameter $%d used as both %s and %s", n, p.types[i].Scalar, t.scalar)
	}
	return nil
}

func (p *paramTracker) typeOf(n int) ParamType {
	p.grow(n)
	return p.types[n-1]
}

// colDef is one resolvable column of a scope. name is the LIR field name;
// wire, when set, is the SQL-visible output name it was sanitized or
// uniquified from.
type colDef struct {
	name string
	wire string
	typ  exprType
}

// scopeDef is one FROM item visible to name resolution: its SQL alias, the
// unique LIR scope label it was lowered under, and its columns. table is set
// for direct table scans so primary keys are available for synthesized
// ordering and mutation shaping.
type scopeDef struct {
	alias string
	label string
	cols  []colDef
	table *Table
}

func (s *scopeDef) col(name string) (*colDef, bool) {
	for i := range s.cols {
		if s.cols[i].name == name {
			return &s.cols[i], true
		}
	}
	return nil, false
}

// aggScope rewrites post-aggregate expressions: any subtree whose
// fingerprint matches a GROUP BY key or a collected aggregate call resolves
// to the aggregate node's output column instead of lowering structurally.
// env is the pre-aggregate environment fingerprints normalize against.
type aggScope struct {
	label string
	byFP  map[string]colDef
	env   *env
}

// env is the name-resolution environment: the scopes of one SELECT level,
// chained to the enclosing level for correlated subqueries.
type env struct {
	parent *env
	scopes []*scopeDef
	agg    *aggScope
}

func (e *env) lookup(alias, col string) (*scopeDef, *colDef, error) {
	for lvl := e; lvl != nil; lvl = lvl.parent {
		if alias != "" {
			for _, s := range lvl.scopes {
				if s.alias == alias {
					c, ok := s.col(col)
					if !ok {
						return nil, nil, fmt.Errorf("column %s.%s does not exist", alias, col)
					}
					return s, c, nil
				}
			}
			continue
		}
		var foundScope *scopeDef
		var foundCol *colDef
		for _, s := range lvl.scopes {
			if c, ok := s.col(col); ok {
				if foundScope != nil {
					return nil, nil, fmt.Errorf("column reference %q is ambiguous", col)
				}
				foundScope, foundCol = s, c
			}
		}
		if foundScope != nil {
			return foundScope, foundCol, nil
		}
	}
	if alias != "" {
		return nil, nil, fmt.Errorf("missing FROM-clause entry for table %q", alias)
	}
	return nil, nil, fmt.Errorf("column %q does not exist", col)
}
