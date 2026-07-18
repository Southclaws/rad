package ui

import (
	"context"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net/http"
	"strconv"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

type devtool struct {
	store kv.KV
}

const (
	defaultLimit = 100
	maxLimit     = 1000
)

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(v)
}

func httpError(w http.ResponseWriter, code int, err error) {
	http.Error(w, fmt.Sprintf(`{"error":%q}`, err.Error()), code)
}

func limitParam(r *http.Request) int {
	n, err := strconv.Atoi(r.URL.Query().Get("limit"))
	if err != nil || n <= 0 {
		return defaultLimit
	}
	return min(n, maxLimit)
}

// tables loads all table definitions by scanning the catalog namespace.
func (s *devtool) tables(ctx context.Context) ([]model.Table, error) {
	prefix := []byte("/rad/catalog/table/")
	it, err := s.store.Scan(ctx, prefix, keyenc.PrefixEnd(prefix))
	if err != nil {
		return nil, err
	}
	defer it.Close()

	tables := []model.Table{}
	for it.Next() {
		var t model.Table
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

func (s *devtool) handleKVScan(w http.ResponseWriter, r *http.Request) {
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

func (s *devtool) handleKVGet(w http.ResponseWriter, r *http.Request) {
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

func clip(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}
