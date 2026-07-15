package radclient

// The autocommit data path, expressed as PIR programs over POST /execute.
// A read is a one-statement query program; a single-row write is a
// one-statement mutation whose input is a one-row `rows` relation. Building
// those typed relations needs the target table's column types, which the
// client fetches once from the catalog and caches. Relations are built with
// the lirwire builders and carried in a statement as opaque marshalled bytes.

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"sync"

	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

// schemaCache lazily holds the database's table definitions, so mutations can
// build typed `rows` relations without a round-trip per call.
type schemaCache struct {
	once   sync.Once
	byName map[string]protocol.TableInfo
	err    error
}

func (c *Client) tableInfo(ctx context.Context, table string) (protocol.TableInfo, error) {
	c.schema.once.Do(func() {
		tables, err := c.Tables(ctx)
		if err != nil {
			c.schema.err = err
			return
		}
		c.schema.byName = make(map[string]protocol.TableInfo, len(tables))
		for _, t := range tables {
			c.schema.byName[t.Name] = t
		}
	})
	if c.schema.err != nil {
		return protocol.TableInfo{}, c.schema.err
	}
	t, ok := c.schema.byName[table]
	if !ok {
		return protocol.TableInfo{}, fmt.Errorf("rad: unknown table %q", table)
	}
	return t, nil
}

// oneRowRelation builds a one-row `rows` relation over the given columns,
// typed from the catalog. Columns absent from cells are simply not in the
// relation — a create then applies their defaults.
func (c *Client) oneRowRelation(ctx context.Context, table string, cells map[string]any) (lirwire.Query, error) {
	info, err := c.tableInfo(ctx, table)
	if err != nil {
		return lirwire.Query{}, err
	}
	types := make(map[string]protocol.ColumnInfo, len(info.Columns))
	for _, col := range info.Columns {
		types[col.Name] = col
	}
	var names []string
	for name := range cells {
		names = append(names, name)
	}
	sortStrings(names)

	cols := make([]lirwire.RowsColumn, len(names))
	row := make([]lirwire.Value, len(names))
	for i, name := range names {
		col, ok := types[name]
		if !ok {
			return lirwire.Query{}, fmt.Errorf("rad: table %q has no column %q", table, name)
		}
		cols[i] = lirwire.RowsColumn{Name: name, Type: col.Type, Nullable: nullableFlag(col.Nullable)}
		v, err := lirwire.SetAny(cells[name])
		if err != nil {
			return lirwire.Query{}, fmt.Errorf("rad: column %q: %w", name, err)
		}
		row[i] = v
	}
	return lirwire.Query{
		Nodes: map[string]lirwire.Node{"r": lirwire.Rows("r", cols, [][]lirwire.Value{row})},
		Root:  lirwire.Root{Node: "r", Cardinality: "many"},
	}, nil
}

// programDatum runs a one-statement program and returns its result datum.
func (c *Client) programDatum(ctx context.Context, stmt pirwire.Statement) (any, error) {
	res, err := c.Execute(ctx, pirwire.Prog("", stmt))
	if err != nil {
		return nil, err
	}
	return res.Result, nil
}

// relationBytes marshals a wire query into a statement's opaque relation
// payload. It fails only if a literal carries invalid JSON, which the lirwire
// builders never produce.
func relationBytes(q lirwire.Query) (pirwire.Relation, error) {
	raw, err := json.Marshal(q)
	if err != nil {
		return nil, fmt.Errorf("rad: encode relation: %w", err)
	}
	return raw, nil
}

// firstRecord views a mutation result datum (an array of affected rows) as at
// most one record.
func firstRecord(d any) (protocol.Record, bool) {
	rows, ok := d.([]any)
	if !ok || len(rows) == 0 {
		return nil, false
	}
	rec, ok := rows[0].(protocol.Record)
	if !ok {
		if m, isMap := rows[0].(map[string]any); isMap {
			return m, true
		}
		return nil, false
	}
	return rec, true
}

// pointRead builds the point-read query used by Get: scan, filter on the key
// columns, take the first row.
func pointRead(table string, key map[string]any) (lirwire.Query, error) {
	pred, err := keyPredicate("s", key)
	if err != nil {
		return lirwire.Query{}, err
	}
	return lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"s":     lirwire.Scan(table, "s"),
			"keyed": lirwire.Filter("s", pred),
			"one":   lirwire.Slice("keyed", 0, intPtr(1)),
		},
		Root: lirwire.Root{Node: "one", Cardinality: "first"},
	}, nil
}

func intPtr(n int) *int { return &n }

// nullableFlag renders a column's nullability as the wire's optional flag:
// present only when true (the schema omits a false flag).
func nullableFlag(b bool) *bool {
	if b {
		return &b
	}
	return nil
}

// execQueryDatum runs a query as a one-statement program.
func (c *Client) execQueryDatum(ctx context.Context, q lirwire.Query) (any, error) {
	rel, err := relationBytes(q)
	if err != nil {
		return nil, err
	}
	return c.programDatum(ctx, pirwire.Query("q", rel))
}

func (c *Client) execQuery(ctx context.Context, q lirwire.Query) ([]protocol.Record, error) {
	d, err := c.execQueryDatum(ctx, q)
	if err != nil {
		return nil, err
	}
	return datumRecords(d)
}

// execGet is a point read as a first-cardinality query program.
func (c *Client) execGet(ctx context.Context, table string, key map[string]any) (protocol.Record, bool, error) {
	q, err := pointRead(table, key)
	if err != nil {
		return nil, false, err
	}
	rel, err := relationBytes(q)
	if err != nil {
		return nil, false, err
	}
	d, err := c.programDatum(ctx, pirwire.Query("get", rel))
	if err != nil {
		return nil, false, err
	}
	rec, ok := d.(protocol.Record)
	if !ok {
		if m, isMap := d.(map[string]any); isMap {
			return m, true, nil
		}
		return nil, false, nil
	}
	return rec, true, nil
}

// execCreate inserts one row via a create statement over a one-row relation.
func (c *Client) execCreate(ctx context.Context, table string, values map[string]any) (protocol.Record, error) {
	rel, err := c.oneRowRelation(ctx, table, values)
	if err != nil {
		return nil, err
	}
	raw, err := relationBytes(rel)
	if err != nil {
		return nil, err
	}
	d, err := c.programDatum(ctx, pirwire.Create("create", table, raw))
	if err != nil {
		return nil, err
	}
	rec, _ := firstRecord(d)
	return rec, nil
}

// execUpdate identifies the row by its primary key and assigns the set/clear
// columns via a one-row relation typed from the catalog — a cleared column is
// a typed NULL, which a bare literal cannot express. A missing key yields a
// not-found error, translated to (nil, false) to keep the point-update
// contract.
func (c *Client) execUpdate(ctx context.Context, table string, key, set map[string]any, clear []string) (protocol.Record, bool, error) {
	info, err := c.tableInfo(ctx, table)
	if err != nil {
		return nil, false, err
	}
	types := make(map[string]protocol.ColumnInfo, len(info.Columns))
	for _, col := range info.Columns {
		types[col.Name] = col
	}

	var cols []lirwire.RowsColumn
	var row []lirwire.Value
	add := func(name string, val any) error {
		col, ok := types[name]
		if !ok {
			return fmt.Errorf("rad: table %q has no column %q", table, name)
		}
		v, err := lirwire.SetAny(val)
		if err != nil {
			return fmt.Errorf("rad: column %q: %w", name, err)
		}
		cols = append(cols, lirwire.RowsColumn{Name: name, Type: col.Type, Nullable: nullableFlag(col.Nullable)})
		row = append(row, v)
		return nil
	}
	for k, v := range key {
		if err := add(k, v); err != nil {
			return nil, false, err
		}
	}
	for k, v := range set {
		if err := add(k, v); err != nil {
			return nil, false, err
		}
	}
	for _, k := range clear {
		if err := add(k, nil); err != nil {
			return nil, false, err
		}
	}

	rel := lirwire.Query{
		Nodes: map[string]lirwire.Node{"r": lirwire.Rows("r", cols, [][]lirwire.Value{row})},
		Root:  lirwire.Root{Node: "r", Cardinality: "many"},
	}
	raw, err := relationBytes(rel)
	if err != nil {
		return nil, false, err
	}
	d, err := c.programDatum(ctx, pirwire.Update("update", table, raw))
	if err != nil {
		if isTargetNotFound(err) {
			return nil, false, nil
		}
		return nil, false, err
	}
	rec, found := firstRecord(d)
	return rec, found, nil
}

// isTargetNotFound reports whether err is a mutation that identified no
// existing row — the point-update/delete miss, surfaced as found=false.
func isTargetNotFound(err error) bool {
	var ae *APIError
	return errors.As(err, &ae) && strings.Contains(ae.Problem.Detail, "target not found")
}

// execDelete identifies rows the same way, projecting the primary key.
func (c *Client) execDelete(ctx context.Context, table string, key map[string]any) (bool, error) {
	pred, err := keyPredicate("s", key)
	if err != nil {
		return false, err
	}
	fields := make([]lirwire.Field, 0, len(key))
	for col := range key {
		fields = append(fields, lirwire.Field{As: col, Expr: lirwire.Col("s", col)})
	}
	rel := lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"s":     lirwire.Scan(table, "s"),
			"keyed": lirwire.Filter("s", pred),
			"p":     lirwire.Project("keyed", "", nil, fields),
		},
		Root: lirwire.Root{Node: "p", Cardinality: "many"},
	}
	raw, err := relationBytes(rel)
	if err != nil {
		return false, err
	}
	res, err := c.Execute(ctx, pirwire.Prog("", pirwire.Delete("delete", table, raw)))
	if err != nil {
		return false, err
	}
	return len(res.Statements) == 1 && res.Statements[0].Affected > 0, nil
}

// keyPredicate ANDs an equality per key column against the given scope.
func keyPredicate(scope string, key map[string]any) (lirwire.Expr, error) {
	preds := make([]lirwire.Expr, 0, len(key))
	for col, val := range key {
		v, err := lirwire.SetAny(val)
		if err != nil {
			return lirwire.Expr{}, fmt.Errorf("rad: key column %q: %w", col, err)
		}
		preds = append(preds, lirwire.Binary("eq", lirwire.Col(scope, col), lirwire.Lit(v)))
	}
	return lirwire.AndAll(preds), nil
}

func sortStrings(s []string) {
	for i := 1; i < len(s); i++ {
		for j := i; j > 0 && s[j-1] > s[j]; j-- {
			s[j-1], s[j] = s[j], s[j-1]
		}
	}
}
