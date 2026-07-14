package radclient

// The autocommit data path, expressed as PIR programs over POST /execute.
// A read is a one-statement query program; a single-row write is a
// one-statement mutation whose input is a one-row `rows` relation. Building
// those typed relations needs the target table's column types, which the
// client fetches once from the catalog and caches.

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"sync"

	"github.com/Southclaws/rad/rad/protocol"
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
// typed from the catalog. Columns absent from cols are simply not in the
// relation — a create then applies their defaults.
func (c *Client) oneRowRelation(ctx context.Context, table string, cells map[string]any) (protocol.Query, error) {
	info, err := c.tableInfo(ctx, table)
	if err != nil {
		return protocol.Query{}, err
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

	cols := make([]protocol.RowsColumn, len(names))
	row := make([]any, len(names))
	for i, name := range names {
		col, ok := types[name]
		if !ok {
			return protocol.Query{}, fmt.Errorf("rad: table %q has no column %q", table, name)
		}
		cols[i] = protocol.RowsColumn{Name: name, Type: col.Type, Nullable: col.Nullable}
		row[i] = cells[name]
	}
	return protocol.Query{
		Nodes: map[string]protocol.Node{
			"r": {Kind: "rows", Scope: "r", Columns: cols, Rows: [][]any{row}},
		},
		Root: protocol.Root{Node: "r", Cardinality: "many"},
	}, nil
}

// programDatum runs a one-statement program and returns its result datum.
func (c *Client) programDatum(ctx context.Context, stmt protocol.Statement) (any, error) {
	res, err := c.Execute(ctx, protocol.Program{Statements: []protocol.Statement{stmt}})
	if err != nil {
		return nil, err
	}
	return res.Result, nil
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
func pointRead(table string, key map[string]any) protocol.Query {
	preds := make([]*protocol.Expr, 0, len(key))
	for col, val := range key {
		preds = append(preds, protocol.Eq(protocol.Col("s", col), protocol.Lit(val)))
	}
	return protocol.Query{
		Nodes: map[string]protocol.Node{
			"s":     {Kind: "scan", Table: table, Scope: "s"},
			"keyed": {Kind: "filter", Input: "s", Predicate: protocol.AndAll(preds)},
			"one":   {Kind: "slice", Input: "keyed", Limit: intPtr(1)},
		},
		Root: protocol.Root{Node: "one", Cardinality: "first"},
	}
}

func intPtr(n int) *int { return &n }

// execQueryDatum runs a query as a one-statement program.
func (c *Client) execQueryDatum(ctx context.Context, q protocol.Query) (any, error) {
	return c.programDatum(ctx, protocol.Read("q", q))
}

func (c *Client) execQuery(ctx context.Context, q protocol.Query) ([]protocol.Record, error) {
	d, err := c.execQueryDatum(ctx, q)
	if err != nil {
		return nil, err
	}
	return datumRecords(d)
}

// execGet is a point read as a first-cardinality query program.
func (c *Client) execGet(ctx context.Context, table string, key map[string]any) (protocol.Record, bool, error) {
	d, err := c.programDatum(ctx, protocol.Read("get", pointRead(table, key)))
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
	d, err := c.programDatum(ctx, protocol.Create("create", table, rel))
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

	var cols []protocol.RowsColumn
	var row []any
	add := func(name string, val any) error {
		col, ok := types[name]
		if !ok {
			return fmt.Errorf("rad: table %q has no column %q", table, name)
		}
		cols = append(cols, protocol.RowsColumn{Name: name, Type: col.Type, Nullable: col.Nullable})
		row = append(row, val)
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

	rel := protocol.Query{
		Nodes: map[string]protocol.Node{
			"r": {Kind: "rows", Scope: "r", Columns: cols, Rows: [][]any{row}},
		},
		Root: protocol.Root{Node: "r", Cardinality: "many"},
	}
	d, err := c.programDatum(ctx, protocol.Update("update", table, rel))
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
	fields := make([]protocol.Field, 0, len(key))
	for col := range key {
		fields = append(fields, protocol.Field{As: col, Expr: *protocol.Col("s", col)})
	}
	rel := protocol.Query{
		Nodes: map[string]protocol.Node{
			"s":     {Kind: "scan", Table: table, Scope: "s"},
			"keyed": {Kind: "filter", Input: "s", Predicate: keyPredicate(key)},
			"p":     {Kind: "project", Input: "keyed", Fields: fields},
		},
		Root: protocol.Root{Node: "p", Cardinality: "many"},
	}
	res, err := c.Execute(ctx, protocol.Program{Statements: []protocol.Statement{protocol.Delete("delete", table, rel)}})
	if err != nil {
		return false, err
	}
	return len(res.Statements) == 1 && res.Statements[0].Affected > 0, nil
}

func keyPredicate(key map[string]any) *protocol.Expr {
	preds := make([]*protocol.Expr, 0, len(key))
	for col, val := range key {
		preds = append(preds, protocol.Eq(protocol.Col("s", col), protocol.Lit(val)))
	}
	return protocol.AndAll(preds)
}

func sortStrings(s []string) {
	for i := 1; i < len(s); i++ {
		for j := i; j > 0 && s[j-1] > s[j]; j-- {
			s[j-1], s[j] = s[j], s[j-1]
		}
	}
}
