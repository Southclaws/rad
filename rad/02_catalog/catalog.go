// Package catalog defines table metadata and persists it as JSON values in
// the KV store under /rad/catalog/... keys.
package catalog

import (
	"context"
	"encoding/json"
	"fmt"
	"slices"
	"strconv"
	"strings"

	keyenc "rad/rad/01_kv/keyenc"

	kv "rad/rad/01_kv"
)

type Table struct {
	ID          string       `json:"id"`
	Name        string       `json:"name"`
	Columns     []Column     `json:"columns"`
	PrimaryKey  []string     `json:"primary_key"`
	Indexes     []Index      `json:"indexes"`
	ForeignKeys []ForeignKey `json:"foreign_keys"`
}

type Column struct {
	ID       string `json:"id"`
	Name     string `json:"name"`
	Type     Type   `json:"type"`
	Nullable bool   `json:"nullable"`
	// Format is optional semantic metadata (e.g. "email", "uuid",
	// "unix_ms"). The database does not interpret it; codegen and tooling
	// may.
	Format string `json:"format,omitempty"`
	// Default, when non-nil, fills the column on inserts that omit it.
	Default *Default `json:"default,omitempty"`
}

type Index struct {
	ID      string   `json:"id"`
	Name    string   `json:"name"`
	Columns []string `json:"columns"`
	Unique  bool     `json:"unique"`
}

type ForeignKey struct {
	ID         string   `json:"id"`
	Name       string   `json:"name"`
	Columns    []string `json:"columns"`
	RefTableID string   `json:"ref_table_id"`
	RefColumns []string `json:"ref_columns"`
}

func (t *Table) Column(name string) (Column, bool) {
	for _, c := range t.Columns {
		if c.Name == name {
			return c, true
		}
	}
	return Column{}, false
}

func (t *Table) Index(name string) (Index, bool) {
	for _, idx := range t.Indexes {
		if idx.Name == name {
			return idx, true
		}
	}
	return Index{}, false
}

// Definition inputs for CreateTable; IDs are assigned by the catalog.

type TableDef struct {
	Name        string
	Columns     []ColumnDef
	PrimaryKey  []string
	Indexes     []IndexDef
	ForeignKeys []ForeignKeyDef
}

type ColumnDef struct {
	Name     string
	Type     Type
	Nullable bool
	Format   string
	Default  *Default
}

type IndexDef struct {
	Name    string
	Columns []string
	Unique  bool
}

type ForeignKeyDef struct {
	Name       string
	Columns    []string
	RefTable   string // table name, resolved to an ID at creation time
	RefColumns []string
}

// Catalog reads and writes table metadata in the KV store. A Rad instance
// is exactly one database — there is no schema or database hierarchy; two
// databases are two Rad deployments. DDL runs in a SerializableSnapshot
// transaction, so the ID counter and name-key writes commit atomically and
// concurrent DDL conflicts instead of corrupting the counter.
type Catalog struct {
	store kv.TransactionalKV
}

func New(store kv.TransactionalKV) *Catalog {
	return &Catalog{store: store}
}

// Reader is the catalog read API over one KV view. A Reader taken from a
// transaction sees that transaction's snapshot plus its own writes, and its
// lookups join the transaction's read set — under SerializableSnapshot,
// concurrent DDL on a table the statement touched conflicts at commit
// instead of passing unseen. Catalog methods read committed state; anything
// that must be consistent with data reads goes through a Reader.
type Reader struct {
	view kv.KV
}

func NewReader(view kv.KV) Reader { return Reader{view: view} }

func (r Reader) GetTable(ctx context.Context, tableName string) (Table, bool, error) {
	return getTable(ctx, r.view, tableName)
}

func (r Reader) GetTableByID(ctx context.Context, id string) (Table, bool, error) {
	return getTableByID(ctx, r.view, id)
}

// ListTables returns every table in the database, sorted by name.
func (r Reader) ListTables(ctx context.Context) ([]Table, error) {
	return listTables(ctx, r.view)
}

// ddl runs fn in a transaction and commits it if fn returns nil.
func (c *Catalog) ddl(ctx context.Context, fn func(view kv.KV) error) error {
	txn, err := c.store.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		return err
	}
	defer txn.Rollback()
	if err := fn(txn); err != nil {
		return err
	}
	return txn.Commit(ctx)
}

const (
	nextIDKey       = "/rad/catalog/meta/next_id"
	tablePrefix     = "/rad/catalog/table/"
	tableNamePrefix = "/rad/catalog/table_name/"
)

// nextID allocates a monotonically increasing ID with the given prefix
// (e.g. "t3", "c17"). Safe under concurrency when view is a transaction:
// two allocators both write the counter key, so one commit conflicts.
func nextID(ctx context.Context, view kv.KV, kind string) (string, error) {
	var n uint64
	raw, ok, err := view.Get(ctx, []byte(nextIDKey))
	if err != nil {
		return "", err
	}
	if ok {
		n, err = strconv.ParseUint(string(raw), 10, 64)
		if err != nil {
			return "", fmt.Errorf("catalog: corrupt next_id %q: %w", raw, err)
		}
	}
	n++
	if err := view.Put(ctx, []byte(nextIDKey), []byte(strconv.FormatUint(n, 10))); err != nil {
		return "", err
	}
	return fmt.Sprintf("%s%d", kind, n), nil
}

// validateDefault checks a column default against the column's type: a
// generator function must be compatible (uuid on text, now_ms on int64).
// Literal defaults carry the column's type by construction.
func validateDefault(cd ColumnDef) error {
	if cd.Default == nil {
		return nil
	}
	switch cd.Default.Func {
	case "":
		return nil
	case DefaultUUID:
		if cd.Type != TypeText {
			return fmt.Errorf("catalog: column %q: uuid() default requires a string column", cd.Name)
		}
	case DefaultNowMS:
		if cd.Type != TypeInt64 {
			return fmt.Errorf("catalog: column %q: now_ms() default requires an int64 column", cd.Name)
		}
	default:
		return fmt.Errorf("catalog: column %q: unknown default function %q", cd.Name, cd.Default.Func)
	}
	return nil
}

func (c *Catalog) CreateTable(ctx context.Context, def TableDef) (Table, error) {
	var tbl Table
	err := c.ddl(ctx, func(view kv.KV) error {
		var err error
		tbl, err = createTable(ctx, view, def)
		return err
	})
	if err != nil {
		return Table{}, err
	}
	return tbl, nil
}

func createTable(ctx context.Context, view kv.KV, def TableDef) (Table, error) {
	if def.Name == "" {
		return Table{}, fmt.Errorf("catalog: table name is required")
	}
	nameKey := tableNamePrefix + def.Name
	if _, ok, err := view.Get(ctx, []byte(nameKey)); err != nil {
		return Table{}, err
	} else if ok {
		return Table{}, fmt.Errorf("catalog: table %q already exists", def.Name)
	}

	tbl := Table{Name: def.Name}
	var err error
	tbl.ID, err = nextID(ctx, view, "t")
	if err != nil {
		return Table{}, err
	}

	seen := map[string]bool{}
	for _, cd := range def.Columns {
		if seen[cd.Name] {
			return Table{}, fmt.Errorf("catalog: duplicate column %q", cd.Name)
		}
		seen[cd.Name] = true
		switch cd.Type {
		case TypeText, TypeInt64, TypeFloat64, TypeBool:
		default:
			return Table{}, fmt.Errorf("catalog: column %q has unsupported type %q", cd.Name, cd.Type)
		}
		if err := validateDefault(cd); err != nil {
			return Table{}, err
		}
		id, err := nextID(ctx, view, "c")
		if err != nil {
			return Table{}, err
		}
		tbl.Columns = append(tbl.Columns, Column{
			ID: id, Name: cd.Name, Type: cd.Type, Nullable: cd.Nullable,
			Format: cd.Format, Default: cd.Default,
		})
	}

	if len(def.PrimaryKey) == 0 {
		return Table{}, fmt.Errorf("catalog: table %q needs a primary key", def.Name)
	}
	for _, name := range def.PrimaryKey {
		col, ok := tbl.Column(name)
		if !ok {
			return Table{}, fmt.Errorf("catalog: primary key column %q does not exist", name)
		}
		if col.Nullable {
			return Table{}, fmt.Errorf("catalog: primary key column %q must not be nullable", name)
		}
	}
	tbl.PrimaryKey = def.PrimaryKey

	for _, id := range def.Indexes {
		if len(id.Columns) == 0 {
			return Table{}, fmt.Errorf("catalog: index %q has no columns", id.Name)
		}
		for _, name := range id.Columns {
			if _, ok := tbl.Column(name); !ok {
				return Table{}, fmt.Errorf("catalog: index %q references unknown column %q", id.Name, name)
			}
		}
		iid, err := nextID(ctx, view, "i")
		if err != nil {
			return Table{}, err
		}
		tbl.Indexes = append(tbl.Indexes, Index{ID: iid, Name: id.Name, Columns: id.Columns, Unique: id.Unique})
	}

	for _, fd := range def.ForeignKeys {
		var ref Table
		if fd.RefTable == def.Name {
			// Self-referential foreign key: validate against the table
			// being created.
			ref = tbl
			ref.PrimaryKey = def.PrimaryKey
		} else {
			var ok bool
			var err error
			ref, ok, err = getTable(ctx, view, fd.RefTable)
			if err != nil {
				return Table{}, err
			}
			if !ok {
				return Table{}, fmt.Errorf("catalog: foreign key %q references unknown table %q", fd.Name, fd.RefTable)
			}
		}
		// POC restriction: foreign keys may only reference the parent's
		// full primary key.
		if len(fd.RefColumns) != len(ref.PrimaryKey) {
			return Table{}, fmt.Errorf("catalog: foreign key %q must reference %q's primary key", fd.Name, fd.RefTable)
		}
		for i, name := range fd.RefColumns {
			if name != ref.PrimaryKey[i] {
				return Table{}, fmt.Errorf("catalog: foreign key %q must reference %q's primary key", fd.Name, fd.RefTable)
			}
		}
		if len(fd.Columns) != len(fd.RefColumns) {
			return Table{}, fmt.Errorf("catalog: foreign key %q column count mismatch", fd.Name)
		}
		for i, name := range fd.Columns {
			col, ok := tbl.Column(name)
			if !ok {
				return Table{}, fmt.Errorf("catalog: foreign key %q references unknown column %q", fd.Name, name)
			}
			refCol, _ := ref.Column(fd.RefColumns[i])
			if col.Type != refCol.Type {
				return Table{}, fmt.Errorf("catalog: foreign key %q type mismatch on %q", fd.Name, name)
			}
		}
		fid, err := nextID(ctx, view, "fk")
		if err != nil {
			return Table{}, err
		}
		tbl.ForeignKeys = append(tbl.ForeignKeys, ForeignKey{
			ID: fid, Name: fd.Name, Columns: fd.Columns,
			RefTableID: ref.ID, RefColumns: fd.RefColumns,
		})
	}

	raw, err := json.Marshal(tbl)
	if err != nil {
		return Table{}, err
	}
	if err := view.Put(ctx, []byte(tablePrefix+tbl.ID), raw); err != nil {
		return Table{}, err
	}
	if err := view.Put(ctx, []byte(nameKey), []byte(tbl.ID)); err != nil {
		return Table{}, err
	}
	return tbl, nil
}

func (c *Catalog) GetTable(ctx context.Context, tableName string) (Table, bool, error) {
	return getTable(ctx, c.store, tableName)
}

// Reader returns a read API bound to committed state. For reads that must
// share a transaction's snapshot, use NewReader with the transaction.
func (c *Catalog) Reader() Reader { return NewReader(c.store) }

func getTable(ctx context.Context, view kv.KV, tableName string) (Table, bool, error) {
	id, ok, err := view.Get(ctx, []byte(tableNamePrefix+tableName))
	if err != nil || !ok {
		return Table{}, false, err
	}
	return getTableByID(ctx, view, string(id))
}

func (c *Catalog) GetTableByID(ctx context.Context, id string) (Table, bool, error) {
	return getTableByID(ctx, c.store, id)
}

// ListTables returns every table in the database, sorted by name.
func (c *Catalog) ListTables(ctx context.Context) ([]Table, error) {
	return listTables(ctx, c.store)
}

func listTables(ctx context.Context, view kv.KV) ([]Table, error) {
	prefix := []byte(tablePrefix)
	it, err := view.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		return nil, err
	}
	defer it.Close()

	var tables []Table
	for it.Next() {
		var t Table
		if err := json.Unmarshal(it.Value(), &t); err != nil {
			return nil, fmt.Errorf("catalog: corrupt table entry %q: %w", it.Key(), err)
		}
		tables = append(tables, t)
	}
	if err := it.Err(); err != nil {
		return nil, err
	}
	slices.SortFunc(tables, func(a, b Table) int { return strings.Compare(a.Name, b.Name) })
	return tables, nil
}

func getTableByID(ctx context.Context, view kv.KV, id string) (Table, bool, error) {
	raw, ok, err := view.Get(ctx, []byte(tablePrefix+id))
	if err != nil || !ok {
		return Table{}, false, err
	}
	var t Table
	if err := json.Unmarshal(raw, &t); err != nil {
		return Table{}, false, err
	}
	return t, true, nil
}
