// Package harness runs planner battle tests end to end with no codegen in
// the loop: a real server (in-memory SlateDB, the full HTTP stack) fronted
// by the real client, with the schema created by direct catalog mutation
// instead of schema.rad migrations.
//
//	d := harness.New(t)
//	d.Table("tasks", harness.Text("id"), harness.Text("status")).Create()
//	d.Insert("tasks", harness.Row{"id": "t1", "status": "open"})
//	d.Query(lirwire.Query{
//		Nodes: map[string]lirwire.Node{
//			"t": lirwire.Scan("tasks", "t"),
//		},
//		Root: lirwire.Root{Node: "t", Cardinality: "many"},
//	}).Equals(`[{"id":"t1","status":"open"}]`)
//
// Queries are written as literal lirwire structs built through the lirwire
// constructors — the node map IS the test. This layer deliberately has no
// query-building abstraction beyond those constructors: what a test sends must
// be visible in the test.
//
// Every query travels client → HTTP → schema validation → binder → planner →
// executor and back, so an assertion here holds the whole read path, not a
// package. Result is the seam where explain output (plan, metrics, planner
// internals) will land — assertions gain plan-shape variants without
// touching existing tests.
package harness

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http/httptest"
	"reflect"
	"slices"
	"strings"
	"testing"

	"sort"

	radclient "github.com/Southclaws/rad/rad/client"
	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
	"github.com/Southclaws/rad/rad/server"
)

// DB is one live database for one test: an in-process server over in-memory
// SlateDB and a connected client. Everything is torn down by t.Cleanup.
type DB struct {
	T      *testing.T
	Client *radclient.Client
	Cat    *catalog.Catalog
	URL    string

	ctx context.Context
}

// New boots a fresh database and connects the real client to it. Safe for
// parallel tests — nothing is shared between instances.
func New(t *testing.T) *DB {
	t.Helper()
	ctx := context.Background()

	store, err := kvslate.Open("battle-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatalf("harness: open store: %v", err)
	}
	t.Cleanup(func() { _ = store.Close() })

	cat := catalog.New(store)
	db := frontend.Open(store)
	handler, err := server.New(db, cat)
	if err != nil {
		t.Fatalf("harness: build server: %v", err)
	}
	srv := httptest.NewServer(handler)
	t.Cleanup(srv.Close)

	url := "rad://" + strings.TrimPrefix(srv.URL, "http://")
	client, err := radclient.Dial(url)
	if err != nil {
		t.Fatalf("harness: dial %s: %v", url, err)
	}
	return &DB{T: t, Client: client, Cat: cat, URL: url, ctx: ctx}
}

// -
// schema: direct catalog mutation
// -

// Column definition sugar. Columns are non-nullable unless wrapped in Null.

func Text(name string) catalog.ColumnDef {
	return catalog.ColumnDef{Name: name, Type: catalog.TypeText}
}
func Int64(name string) catalog.ColumnDef {
	return catalog.ColumnDef{Name: name, Type: catalog.TypeInt64}
}
func Float64(name string) catalog.ColumnDef {
	return catalog.ColumnDef{Name: name, Type: catalog.TypeFloat64}
}
func Bool(name string) catalog.ColumnDef {
	return catalog.ColumnDef{Name: name, Type: catalog.TypeBool}
}

// Null makes a column definition nullable.
func Null(cd catalog.ColumnDef) catalog.ColumnDef {
	cd.Nullable = true
	return cd
}

// CreateTable applies a raw table definition directly to the catalog.
func (d *DB) CreateTable(def catalog.TableDef) {
	d.T.Helper()
	if _, err := d.Cat.CreateTable(d.ctx, def); err != nil {
		d.T.Fatalf("harness: create table %q: %v", def.Name, err)
	}
}

// Table starts a fluent table definition. The primary key defaults to an
// "id" column when one exists and PK is not called.
func (d *DB) Table(name string, cols ...catalog.ColumnDef) *TableBuilder {
	return &TableBuilder{d: d, def: catalog.TableDef{Name: name, Columns: cols}}
}

// TableBuilder accumulates a table definition; Create applies it.
type TableBuilder struct {
	d   *DB
	def catalog.TableDef
}

func (tb *TableBuilder) PK(cols ...string) *TableBuilder {
	tb.def.PrimaryKey = cols
	return tb
}

func (tb *TableBuilder) Index(name string, cols ...string) *TableBuilder {
	tb.def.Indexes = append(tb.def.Indexes, catalog.IndexDef{Name: name, Columns: cols})
	return tb
}

func (tb *TableBuilder) Unique(name string, cols ...string) *TableBuilder {
	tb.def.Indexes = append(tb.def.Indexes, catalog.IndexDef{Name: name, Columns: cols, Unique: true})
	return tb
}

// FK declares a single-column foreign key — the overwhelmingly common case;
// use AddFK for composite keys.
func (tb *TableBuilder) FK(name, col, refTable, refCol string) *TableBuilder {
	return tb.AddFK(catalog.ForeignKeyDef{
		Name: name, Columns: []string{col}, RefTable: refTable, RefColumns: []string{refCol},
	})
}

func (tb *TableBuilder) AddFK(def catalog.ForeignKeyDef) *TableBuilder {
	tb.def.ForeignKeys = append(tb.def.ForeignKeys, def)
	return tb
}

func (tb *TableBuilder) Create() {
	tb.d.T.Helper()
	if len(tb.def.PrimaryKey) == 0 {
		for _, c := range tb.def.Columns {
			if c.Name == "id" {
				tb.def.PrimaryKey = []string{"id"}
				break
			}
		}
	}
	tb.d.CreateTable(tb.def)
}

// -
// seeding
// -

// Row is one row of seed data, keyed by column name.
type Row = map[string]any

// Insert seeds rows through the real client as one execution program: a
// single create statement whose input is a literal `rows` relation. Values
// coerce exactly as an application's writes would, and the whole batch
// commits in one transaction — one storage flush per call, which keeps a
// thousand-test suite fast, and it exercises the intended batch-create path.
func (d *DB) Insert(table string, rows ...Row) {
	d.T.Helper()
	if len(rows) == 0 {
		return
	}
	tbl, ok, err := d.Cat.GetTable(d.ctx, table)
	if err != nil || !ok {
		d.T.Fatalf("harness: insert into %q: table not found (%v)", table, err)
	}

	// Group rows by the exact set of columns they provide: a `rows` relation
	// is positional, and a create applies defaults for whatever columns the
	// relation omits, so rows that omit different columns need different
	// statements. Within a group every row supplies the same columns.
	sigs, groups := groupBySignature(rows)
	stmts := make([]pirwire.Statement, len(sigs))
	var lastName string
	for gi, sig := range sigs {
		grp := groups[sig]
		names := grp.names
		cols := make([]lirwire.RowsColumn, len(names))
		for i, name := range names {
			col, ok := tbl.Column(name)
			if !ok {
				d.T.Fatalf("harness: insert into %q: no column %q", table, name)
			}
			cols[i] = lirwire.RowsColumn{Name: name, Type: string(col.Type), Nullable: nullableFlag(col.Nullable)}
		}
		cells := make([][]lirwire.Value, len(grp.rows))
		for i, r := range grp.rows {
			cell := make([]lirwire.Value, len(names))
			for j, name := range names {
				v, err := lirwire.SetAny(r[name])
				if err != nil {
					d.T.Fatalf("harness: insert into %q: column %q: %v", table, name, err)
				}
				cell[j] = v
			}
			cells[i] = cell
		}
		rel := lirwire.Query{
			Nodes: map[string]lirwire.Node{"r": lirwire.Rows("r", cols, cells)},
			Root:  lirwire.Root{Node: "r", Cardinality: "many"},
		}
		name := fmt.Sprintf("seed%d", gi)
		stmts[gi] = pirwire.Create(name, table, d.relation(rel))
		lastName = name
	}

	result := ""
	if len(stmts) > 1 {
		result = lastName
	}
	if _, err := d.Client.Execute(d.ctx, pirwire.Prog(result, stmts...)); err != nil {
		d.T.Fatalf("harness: insert into %q: %v", table, err)
	}
}

// nullableFlag renders a column's nullability as the wire's optional flag:
// present only when true.
func nullableFlag(b bool) *bool {
	if b {
		return &b
	}
	return nil
}

type seedGroup struct {
	names []string
	rows  []Row
}

// groupBySignature buckets rows by their sorted column set, preserving
// first-seen order of the signatures for deterministic statement order.
func groupBySignature(rows []Row) ([]string, map[string]*seedGroup) {
	var order []string
	groups := map[string]*seedGroup{}
	for _, r := range rows {
		names := make([]string, 0, len(r))
		for name := range r {
			names = append(names, name)
		}
		sort.Strings(names)
		sig := strings.Join(names, ",")
		g, ok := groups[sig]
		if !ok {
			g = &seedGroup{names: names}
			groups[sig] = g
			order = append(order, sig)
		}
		g.rows = append(g.rows, r)
	}
	return order, groups
}

// -
// querying
// -

// Result is one executed query: what was sent, what came back. Datum is the
// result exactly as the root materialised (array / object / naked value /
// nil); Records is its record view when one exists. Explain output (plan
// shape, metrics, planner internals) lands here when the wire grows it;
// assertions then extend without churning existing tests.
type Result struct {
	T       *testing.T
	Query   lirwire.Query
	Datum   any
	Records []protocol.Record
	Err     error
}

// relation marshals a wire query into a statement's opaque relation payload,
// failing the test if it cannot be encoded.
func (d *DB) relation(q lirwire.Query) pirwire.Relation {
	d.T.Helper()
	raw, err := json.Marshal(q)
	if err != nil {
		d.T.Fatalf("harness: encode relation: %v", err)
	}
	return raw
}

// Query runs a raw LIR query as a one-statement execution program and returns
// its Result for assertion chaining.
func (d *DB) Query(q lirwire.Query) *Result {
	d.T.Helper()
	var datum any
	res, err := d.Client.Execute(d.ctx, pirwire.Prog("", pirwire.Query("result", d.relation(q))))
	if err == nil {
		datum = res.Result
	}
	r := &Result{T: d.T, Query: q, Datum: datum, Err: err}
	if err == nil {
		switch v := datum.(type) {
		case nil:
		case []any:
			for _, el := range v {
				if rec, ok := el.(map[string]any); ok {
					r.Records = append(r.Records, rec)
				}
			}
		case map[string]any:
			r.Records = []protocol.Record{v}
		}
	}
	return r
}

// Rows returns the records, failing the test on any query error.
func (r *Result) Rows() []protocol.Record {
	r.T.Helper()
	r.ok()
	return r.Records
}

// Equals asserts the exact result: rows, fields, values, and order, as JSON.
func (r *Result) Equals(wantJSON string) *Result {
	r.T.Helper()
	r.ok()
	got, want := canonical(r.T, r.Records), canonical(r.T, parseJSON(r.T, wantJSON))
	if got != want {
		r.fail("result mismatch", got, want)
	}
	return r
}

// EqualsUnordered asserts the same multiset of rows in any order — for
// relations with no total order, where row sequence is physical.
func (r *Result) EqualsUnordered(wantJSON string) *Result {
	r.T.Helper()
	r.ok()
	got := make([]string, len(r.Records))
	for i, rec := range r.Records {
		got[i] = canonical(r.T, rec)
	}
	parsed, okList := parseJSON(r.T, wantJSON).([]any)
	if !okList {
		r.T.Fatalf("harness: EqualsUnordered wants a JSON array, got: %s", wantJSON)
	}
	want := make([]string, len(parsed))
	for i, rec := range parsed {
		want[i] = canonical(r.T, rec)
	}
	slices.Sort(got)
	slices.Sort(want)
	if !reflect.DeepEqual(got, want) {
		r.fail("result mismatch (order-insensitive)",
			canonical(r.T, r.Records), canonical(r.T, parsed))
	}
	return r
}

// EqualsDatum asserts the exact result datum — the assertion for scalar and
// first roots, where the result is not a record list.
func (r *Result) EqualsDatum(wantJSON string) *Result {
	r.T.Helper()
	r.ok()
	got, want := canonical(r.T, r.Datum), canonical(r.T, parseJSON(r.T, wantJSON))
	if got != want {
		r.fail("result datum mismatch", got, want)
	}
	return r
}

// Len asserts the row count.
func (r *Result) Len(n int) *Result {
	r.T.Helper()
	r.ok()
	if len(r.Records) != n {
		r.fail(fmt.Sprintf("row count = %d, want %d", len(r.Records), n),
			canonical(r.T, r.Records), "")
	}
	return r
}

// Empty asserts no rows.
func (r *Result) Empty() *Result { return r.Len(0) }

// ExpectError asserts the query failed and the error contains substr —
// binder rejections arrive as 422 problems whose detail keeps the
// "planner:" prefix.
func (r *Result) ExpectError(substr string) *Result {
	r.T.Helper()
	if r.Err == nil {
		r.T.Fatalf("harness: query succeeded, want error containing %q\nquery: %s\ngot: %s",
			substr, r.queryJSON(), canonical(r.T, r.Records))
	}
	if !strings.Contains(r.Err.Error(), substr) {
		r.T.Fatalf("harness: error = %q, want it to contain %q", r.Err, substr)
	}
	return r
}

// ExpectStatus asserts the query failed with the given HTTP status.
func (r *Result) ExpectStatus(status int) *Result {
	r.T.Helper()
	var ae *radclient.APIError
	if r.Err == nil || !errors.As(r.Err, &ae) {
		r.T.Fatalf("harness: want an API error with status %d, got err=%v", status, r.Err)
	}
	if ae.Problem.Status != status {
		r.T.Fatalf("harness: status = %d (%s), want %d", ae.Problem.Status, ae.Problem.Detail, status)
	}
	return r
}

// ExpectCode asserts the query failed with the given problem code — the
// classification contract ("invalid" for bind-time rejections,
// "execution_failed" for data-dependent runtime failures).
func (r *Result) ExpectCode(code string) *Result {
	r.T.Helper()
	var ae *radclient.APIError
	if r.Err == nil || !errors.As(r.Err, &ae) {
		r.T.Fatalf("harness: want an API error with code %q, got err=%v", code, r.Err)
	}
	if ae.Problem.Code != code {
		r.T.Fatalf("harness: code = %q (%s), want %q", ae.Problem.Code, ae.Problem.Detail, code)
	}
	return r
}

func (r *Result) ok() {
	r.T.Helper()
	if r.Err != nil {
		r.T.Fatalf("harness: query failed: %v\nquery: %s", r.Err, r.queryJSON())
	}
}

func (r *Result) fail(msg, got, want string) {
	r.T.Helper()
	out := fmt.Sprintf("harness: %s\nquery: %s\n  got: %s", msg, r.queryJSON(), got)
	if want != "" {
		out += "\n want: " + want
	}
	r.T.Fatal(out)
}

func (r *Result) queryJSON() string {
	raw, err := protocol.MarshalQuery(r.Query)
	if err != nil {
		return fmt.Sprintf("<unmarshalable: %v>", err)
	}
	return string(raw)
}

// -
// JSON comparison
// -

// parseJSON decodes a want-literal with number fidelity matching the
// client's decoding.
func parseJSON(t *testing.T, s string) any {
	t.Helper()
	dec := json.NewDecoder(strings.NewReader(s))
	dec.UseNumber()
	var v any
	if err := dec.Decode(&v); err != nil {
		t.Fatalf("harness: invalid want JSON: %v\n%s", err, s)
	}
	return v
}

// canonical renders a value as compact JSON with sorted keys — the
// comparison and failure-message form.
func canonical(t *testing.T, v any) string {
	t.Helper()
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatalf("harness: marshal: %v", err)
	}
	return string(b)
}
