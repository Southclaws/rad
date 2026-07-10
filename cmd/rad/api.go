package main

import (
	"context"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net/http"
	"strconv"

	"rad/rad/01_kv"
	"rad/rad/01_kv/keyenc"
	"rad/rad/02_catalog"
	qir "rad/rad/03_qir"
	"rad/rad/05_exec"
)

type server struct {
	store  kv.KV
	cat    *catalog.Catalog
	eng    *exec.Engine
	dbPath string
}

const (
	defaultLimit = 100
	maxLimit     = 1000
)

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(v)
}

func limitParam(r *http.Request) int {
	n, err := strconv.Atoi(r.URL.Query().Get("limit"))
	if err != nil || n <= 0 {
		return defaultLimit
	}
	return min(n, maxLimit)
}

func (s *server) handleMeta(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, map[string]any{
		"dbPath": s.dbPath,
	})
}

// handleSchema returns every table definition in the configured schema.
func (s *server) handleSchema(w http.ResponseWriter, r *http.Request) {
	tables, err := s.tables(r.Context())
	if err != nil {
		httpError(w, 500, err)
		return
	}
	writeJSON(w, map[string]any{"tables": tables})
}

// tables loads all table definitions by scanning the catalog namespace.
func (s *server) tables(ctx context.Context) ([]catalog.Table, error) {
	prefix := []byte("/rad/catalog/table/")
	it, err := s.store.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		return nil, err
	}
	defer it.Close()

	tables := []catalog.Table{}
	for it.Next() {
		var t catalog.Table
		if err := json.Unmarshal(it.Value(), &t); err != nil {
			return nil, fmt.Errorf("corrupt catalog entry %q: %w", it.Key(), err)
		}
		tables = append(tables, t)
	}
	return tables, it.Err()
}

// kvEntry is one scan result with both raw and human-readable forms.
type kvEntry struct {
	Key          string `json:"key"` // base64
	KeyDisplay   string `json:"keyDisplay"`
	ValueSize    int    `json:"valueSize"`
	ValueDisplay string `json:"valueDisplay"` // possibly truncated
}

func (s *server) handleKVScan(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	prefix := []byte(q.Get("prefix"))
	limit := limitParam(r)

	start := prefix
	if after := q.Get("after"); after != "" {
		key, err := base64.StdEncoding.DecodeString(after)
		if err != nil {
			httpError(w, 400, fmt.Errorf("bad after cursor: %w", err))
			return
		}
		// Smallest key strictly greater than the cursor.
		start = append(key, 0x00)
	}
	var end []byte
	if len(prefix) > 0 {
		end = keyenc.PrefixEnd(prefix)
	}

	dec := s.newKeyDecoder(r.Context())
	it, err := s.store.Scan(r.Context(), start, end)
	if err != nil {
		httpError(w, 500, err)
		return
	}
	defer it.Close()

	entries := []kvEntry{}
	var lastKey []byte
	truncated := false
	for it.Next() {
		if len(entries) >= limit {
			truncated = true
			break
		}
		key := append([]byte(nil), it.Key()...)
		lastKey = key
		entries = append(entries, kvEntry{
			Key:          base64.StdEncoding.EncodeToString(key),
			KeyDisplay:   dec.key(key),
			ValueSize:    len(it.Value()),
			ValueDisplay: clip(dec.value(key, it.Value()), 160),
		})
	}
	if err := it.Err(); err != nil {
		httpError(w, 500, err)
		return
	}

	resp := map[string]any{"entries": entries, "truncated": truncated}
	if truncated && lastKey != nil {
		resp["nextAfter"] = base64.StdEncoding.EncodeToString(lastKey)
	}
	writeJSON(w, resp)
}

func (s *server) handleKVGet(w http.ResponseWriter, r *http.Request) {
	key, err := base64.StdEncoding.DecodeString(r.URL.Query().Get("key"))
	if err != nil {
		httpError(w, 400, fmt.Errorf("bad key: %w", err))
		return
	}
	value, ok, err := s.store.Get(r.Context(), key)
	if err != nil {
		httpError(w, 500, err)
		return
	}
	if !ok {
		httpError(w, 404, fmt.Errorf("key not found"))
		return
	}

	dec := s.newKeyDecoder(r.Context())
	resp := map[string]any{
		"key":          base64.StdEncoding.EncodeToString(key),
		"keyDisplay":   dec.key(key),
		"keyHex":       hex.Dump(key),
		"valueSize":    len(value),
		"valueDisplay": dec.value(key, value),
		"valueHex":     hex.Dump(value),
	}
	if json.Valid(value) {
		resp["valueJSON"] = json.RawMessage(value)
	}
	writeJSON(w, resp)
}

// cell is one column value of one row, pre-rendered for the grid.
type cell struct {
	Display string `json:"d"`
	Null    bool   `json:"null,omitempty"`
}

type rowResult struct {
	Key        string `json:"key,omitempty"` // base64 of the underlying data key
	KeyDisplay string `json:"keyDisplay,omitempty"`
	Cells      []cell `json:"cells"`
}

func rowResults(tbl catalog.Table, rows []qir.Row, keys [][]byte, dec *keyDecoder) []rowResult {
	out := make([]rowResult, len(rows))
	for i, row := range rows {
		rr := rowResult{Cells: make([]cell, len(tbl.Columns))}
		if keys != nil {
			rr.Key = base64.StdEncoding.EncodeToString(keys[i])
			rr.KeyDisplay = dec.key(keys[i])
		}
		for j, col := range tbl.Columns {
			v := row[col.Name]
			rr.Cells[j] = cell{Display: valueDisplay(v), Null: v.Null}
		}
		out[i] = rr
	}
	return out
}

func valueDisplay(v qir.Value) string {
	if v.Null {
		return "NULL"
	}
	switch v.Type {
	case catalog.TypeText:
		return v.Text
	case catalog.TypeInt64:
		return strconv.FormatInt(v.Int64, 10)
	case catalog.TypeFloat64:
		return strconv.FormatFloat(v.Float64, 'g', -1, 64)
	}
	return v.String()
}

// handleTableRows pages through a table in primary key order via a raw range
// scan on the data prefix (cursor = last data key).
func (s *server) handleTableRows(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	tbl, ok, err := s.cat.GetTable(ctx, r.PathValue("table"))
	if err != nil {
		httpError(w, 500, err)
		return
	}
	if !ok {
		httpError(w, 404, fmt.Errorf("table not found"))
		return
	}
	limit := limitParam(r)

	prefix := exec.DataPrefix(tbl.ID)
	start := prefix
	if after := r.URL.Query().Get("after"); after != "" {
		key, err := base64.StdEncoding.DecodeString(after)
		if err != nil {
			httpError(w, 400, fmt.Errorf("bad after cursor: %w", err))
			return
		}
		start = append(key, 0x00)
	}

	it, err := s.store.Scan(ctx, start, keyenc.PrefixEnd(prefix))
	if err != nil {
		httpError(w, 500, err)
		return
	}
	defer it.Close()

	var rows []qir.Row
	var keys [][]byte
	truncated := false
	for it.Next() {
		if len(rows) >= limit {
			truncated = true
			break
		}
		row, err := exec.UnmarshalRow(tbl, it.Value())
		if err != nil {
			httpError(w, 500, fmt.Errorf("corrupt row at %q: %w", it.Key(), err))
			return
		}
		rows = append(rows, row)
		keys = append(keys, append([]byte(nil), it.Key()...))
	}
	if err := it.Err(); err != nil {
		httpError(w, 500, err)
		return
	}

	dec := s.newKeyDecoder(ctx)
	resp := map[string]any{
		"table":     tableColumns(tbl),
		"rows":      rowResults(tbl, rows, keys, dec),
		"truncated": truncated,
	}
	if truncated && len(keys) > 0 {
		resp["nextAfter"] = base64.StdEncoding.EncodeToString(keys[len(keys)-1])
	}
	writeJSON(w, resp)
}

// handleIndexScan filters a table through one of its indexes: ?index=<name>
// plus one &v=<value> per leading index column (coerced by column type).
func (s *server) handleIndexScan(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	tableName := r.PathValue("table")
	tbl, ok, err := s.cat.GetTable(ctx, tableName)
	if err != nil {
		httpError(w, 500, err)
		return
	}
	if !ok {
		httpError(w, 404, fmt.Errorf("table not found"))
		return
	}
	idx, ok := tbl.Index(r.URL.Query().Get("index"))
	if !ok {
		httpError(w, 404, fmt.Errorf("index not found"))
		return
	}

	values := r.URL.Query()["v"]
	if len(values) > len(idx.Columns) {
		httpError(w, 400, fmt.Errorf("index %q has %d columns, got %d values", idx.Name, len(idx.Columns), len(values)))
		return
	}
	prefix := qir.Row{}
	for i, raw := range values {
		col, _ := tbl.Column(idx.Columns[i])
		v, err := coerce(raw, col.Type)
		if err != nil {
			httpError(w, 400, fmt.Errorf("column %q: %w", col.Name, err))
			return
		}
		prefix[col.Name] = v
	}

	rows, err := s.eng.ScanIndex(ctx, tableName, idx.Name, prefix)
	if err != nil {
		httpError(w, 500, err)
		return
	}
	writeJSON(w, map[string]any{
		"table": tableColumns(tbl),
		"rows":  rowResults(tbl, rows, nil, nil),
	})
}

func coerce(raw string, t catalog.Type) (qir.Value, error) {
	switch t {
	case catalog.TypeText:
		return qir.Text(raw), nil
	case catalog.TypeInt64:
		i, err := strconv.ParseInt(raw, 10, 64)
		if err != nil {
			return qir.Value{}, fmt.Errorf("not an int64: %q", raw)
		}
		return qir.Int64(i), nil
	case catalog.TypeFloat64:
		f, err := strconv.ParseFloat(raw, 64)
		if err != nil {
			return qir.Value{}, fmt.Errorf("not a float64: %q", raw)
		}
		return qir.Float64(f), nil
	}
	return qir.Value{}, fmt.Errorf("unsupported type %q", t)
}

func tableColumns(tbl catalog.Table) map[string]any {
	cols := make([]map[string]any, len(tbl.Columns))
	pk := map[string]bool{}
	for _, name := range tbl.PrimaryKey {
		pk[name] = true
	}
	for i, c := range tbl.Columns {
		cols[i] = map[string]any{
			"name": c.Name, "type": string(c.Type),
			"nullable": c.Nullable, "pk": pk[c.Name],
		}
	}
	return map[string]any{"name": tbl.Name, "id": tbl.ID, "columns": cols}
}

func clip(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}
