package rad

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/Southclaws/rad/clients/go/protocol"
	"github.com/Southclaws/rad/clients/go/protocol/lirwire"
)

const testEntityTag = `W/"rad-query-test"`

func testQuery() lirwire.Query {
	cell, err := lirwire.MakeCell(lirwire.ScalarType("int64"), int64(1))
	if err != nil {
		panic(err)
	}
	return lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"row": lirwire.Rows("row", []lirwire.RowsColumn{{
				Name: "value",
				Type: lirwire.ScalarType("int64"),
			}}, [][]lirwire.Cell{{cell}}),
		},
		Root: lirwire.Root{Node: "row", Cardinality: "exactly_one"},
	}
}

func queryResponseHeaders(response http.ResponseWriter, entityTag string) {
	response.Header().Set("ETag", entityTag)
	response.Header().Set("Accept-Query", `"application/vnd.rad.lir+json"`)
	response.Header().Set("Cache-Control", "private, no-cache")
	response.Header().Set("Vary", "Accept, Authorization, Content-Encoding, Content-Type")
}

func dialTestServer(t *testing.T, handler http.Handler, options ...Option) *Client {
	t.Helper()
	server := httptest.NewServer(handler)
	t.Cleanup(server.Close)
	client, err := Dial("rad://"+strings.TrimPrefix(server.URL, "http://"), options...)
	if err != nil {
		t.Fatal(err)
	}
	return client
}

func TestQueryUsesGeneratedQueryTransport(t *testing.T) {
	wantBody, err := protocol.MarshalQuery(testQuery())
	if err != nil {
		t.Fatal(err)
	}
	client := dialTestServer(t, http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		if request.Method != "QUERY" {
			t.Errorf("method = %q, want QUERY", request.Method)
		}
		if request.Header.Get("Content-Type") != queryRequestMediaType {
			t.Errorf("content type = %q", request.Header.Get("Content-Type"))
		}
		body := make([]byte, request.ContentLength)
		_, _ = request.Body.Read(body)
		if string(body) != string(wantBody) {
			t.Errorf("body = %s, want %s", body, wantBody)
		}
		queryResponseHeaders(response, testEntityTag)
		response.Header().Set("Content-Type", "application/json")
		_, _ = response.Write([]byte(`{"value":1}`))
	}))

	value, err := client.QueryDatum(context.Background(), testQuery())
	if err != nil {
		t.Fatal(err)
	}
	if value.(map[string]any)["value"].(interface{ String() string }).String() != "1" {
		t.Fatalf("value = %#v", value)
	}
}

func TestConditionalQueryRetainsRawJSONAndIsolatesDecodedValues(t *testing.T) {
	cache, err := NewMemoryQueryCache()
	if err != nil {
		t.Fatal(err)
	}
	var requests atomic.Uint64
	client := dialTestServer(t, http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		requests.Add(1)
		queryResponseHeaders(response, testEntityTag)
		if request.Header.Get("If-None-Match") == testEntityTag {
			response.WriteHeader(http.StatusNotModified)
			return
		}
		response.Header().Set("Content-Type", "application/json")
		_, _ = response.Write([]byte(`{"items":[{"value":1}]}`))
	}), WithQueryCache(cache))

	first, err := client.QueryDatum(context.Background(), testQuery())
	if err != nil {
		t.Fatal(err)
	}
	first.(map[string]any)["changed"] = true
	second, err := client.QueryDatum(context.Background(), testQuery())
	if err != nil {
		t.Fatal(err)
	}
	if _, found := second.(map[string]any)["changed"]; found {
		t.Fatal("a decoded result was shared between callers")
	}
	if requests.Load() != 2 {
		t.Fatalf("requests = %d, want 2", requests.Load())
	}
	stats := client.QueryStats()
	if stats.Changed != 1 || stats.Unchanged != 1 {
		t.Fatalf("query stats = %#v", stats)
	}
	cacheStats := cache.Stats()
	if cacheStats.Hits != 1 || cacheStats.Misses != 1 || cacheStats.Admissions != 1 {
		t.Fatalf("cache stats = %#v", cacheStats)
	}
}

func TestMemoryQueryCacheEnforcesLimitsAndLRU(t *testing.T) {
	cache, err := NewMemoryQueryCache(QueryCacheLimits{Bytes: 1024, Entries: 2, MaxResultBytes: 8})
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	keys := []QueryCacheKey{{digest: [32]byte{1}}, {digest: [32]byte{2}}, {digest: [32]byte{3}}}
	for _, key := range keys[:2] {
		if err := cache.Put(ctx, key, QueryCacheEntry{ETag: `"x"`, Body: `{}`}); err != nil {
			t.Fatal(err)
		}
	}
	if _, _, err := cache.Get(ctx, keys[0]); err != nil {
		t.Fatal(err)
	}
	if err := cache.Put(ctx, keys[2], QueryCacheEntry{ETag: `"x"`, Body: `{}`}); err != nil {
		t.Fatal(err)
	}
	if _, found, _ := cache.Get(ctx, keys[1]); found {
		t.Fatal("least recently used entry remains resident")
	}
	if err := cache.Put(ctx, keys[1], QueryCacheEntry{ETag: `"x"`, Body: `123456789`}); err != nil {
		t.Fatal(err)
	}
	stats := cache.Stats()
	if stats.Entries != 2 || stats.Evictions != 1 || stats.OversizedResults != 1 {
		t.Fatalf("cache stats = %#v", stats)
	}
}

func TestMemoryQueryCacheReplacesEntriesAndReportsBytePressure(t *testing.T) {
	cache, err := NewMemoryQueryCache(QueryCacheLimits{Bytes: 44, Entries: 4, MaxResultBytes: 32})
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	first := QueryCacheKey{digest: [32]byte{1}}
	second := QueryCacheKey{digest: [32]byte{2}}
	if err := cache.Put(ctx, first, QueryCacheEntry{ETag: `"a"`, Body: `{}`}); err != nil {
		t.Fatal(err)
	}
	if err := cache.Put(ctx, first, QueryCacheEntry{ETag: `"b"`, Body: `[]`}); err != nil {
		t.Fatal(err)
	}
	entry, found, err := cache.Get(ctx, first)
	if err != nil || !found || entry.ETag != `"b"` {
		t.Fatalf("entry = %#v, found = %t, error = %v", entry, found, err)
	}
	if err := cache.Put(ctx, second, QueryCacheEntry{ETag: `"c"`, Body: `{}`}); err != nil {
		t.Fatal(err)
	}
	stats := cache.Stats()
	if stats.Replacements != 1 || stats.ByteLimitPressure != 1 || stats.Evictions != 1 {
		t.Fatalf("cache stats = %#v", stats)
	}
}

func TestMemoryQueryCacheRemovesAStaleEntryAfterARejectedReplacement(t *testing.T) {
	cache, err := NewMemoryQueryCache(QueryCacheLimits{Bytes: 128, Entries: 2, MaxResultBytes: 4})
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	key := QueryCacheKey{digest: [32]byte{1}}
	if err := cache.Put(ctx, key, QueryCacheEntry{ETag: `"a"`, Body: `{}`}); err != nil {
		t.Fatal(err)
	}
	if err := cache.Put(ctx, key, QueryCacheEntry{ETag: `"b"`, Body: `{"value":1}`}); err != nil {
		t.Fatal(err)
	}
	if _, found, err := cache.Get(ctx, key); err != nil || found {
		t.Fatalf("found = %t, error = %v", found, err)
	}
	stats := cache.Stats()
	if stats.Entries != 0 || stats.OversizedResults != 1 {
		t.Fatalf("cache stats = %#v", stats)
	}
}

type failingQueryCache struct{}

func (failingQueryCache) Get(context.Context, QueryCacheKey) (QueryCacheEntry, bool, error) {
	return QueryCacheEntry{}, false, errors.New("cache unavailable")
}

func (failingQueryCache) Put(context.Context, QueryCacheKey, QueryCacheEntry) error {
	return errors.New("cache unavailable")
}

func (failingQueryCache) Delete(context.Context, QueryCacheKey) error {
	return errors.New("cache unavailable")
}

func TestQueryCacheErrorUsesAnUnconditionalQuery(t *testing.T) {
	var ifNoneMatch string
	client := dialTestServer(t, http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		ifNoneMatch = request.Header.Get("If-None-Match")
		queryResponseHeaders(response, testEntityTag)
		response.Header().Set("Content-Type", "application/json")
		_, _ = response.Write([]byte(`{"value":1}`))
	}), WithQueryCache(failingQueryCache{}))

	if _, err := client.QueryDatum(context.Background(), testQuery()); err != nil {
		t.Fatal(err)
	}
	if ifNoneMatch != "" {
		t.Fatalf("If-None-Match = %q", ifNoneMatch)
	}
	stats := client.QueryStats()
	if stats.CacheErrors != 1 || stats.Changed != 1 {
		t.Fatalf("query stats = %#v", stats)
	}
}

type fixedQueryCache struct {
	mu    sync.Mutex
	entry QueryCacheEntry
	found bool
}

func (c *fixedQueryCache) Get(context.Context, QueryCacheKey) (QueryCacheEntry, bool, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.entry, c.found, nil
}

func (c *fixedQueryCache) Put(_ context.Context, _ QueryCacheKey, entry QueryCacheEntry) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.entry = entry
	c.found = true
	return nil
}

func (c *fixedQueryCache) Delete(context.Context, QueryCacheKey) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.entry = QueryCacheEntry{}
	c.found = false
	return nil
}

func TestInconsistentNotModifiedRetriesWithoutACondition(t *testing.T) {
	cache := &fixedQueryCache{entry: QueryCacheEntry{ETag: testEntityTag, Body: `{"value":1}`}, found: true}
	var requests atomic.Uint64
	client := dialTestServer(t, http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		requestNumber := requests.Add(1)
		if requestNumber == 1 {
			queryResponseHeaders(response, `W/"different"`)
			response.WriteHeader(http.StatusNotModified)
			return
		}
		if request.Header.Get("If-None-Match") != "" {
			t.Errorf("recovery request kept If-None-Match")
		}
		queryResponseHeaders(response, testEntityTag)
		response.Header().Set("Content-Type", "application/json")
		_, _ = response.Write([]byte(`{"value":2}`))
	}), WithQueryCache(cache))

	value, err := client.QueryDatum(context.Background(), testQuery())
	if err != nil {
		t.Fatal(err)
	}
	if value.(map[string]any)["value"].(interface{ String() string }).String() != "2" {
		t.Fatalf("value = %#v", value)
	}
	if requests.Load() != 2 || client.QueryStats().Recoveries != 1 {
		t.Fatalf("requests = %d, stats = %#v", requests.Load(), client.QueryStats())
	}
}

func TestCorruptCacheEntryRetriesWithoutACondition(t *testing.T) {
	cache := &fixedQueryCache{
		entry: QueryCacheEntry{ETag: testEntityTag, Body: `{`},
		found: true,
	}
	var ifNoneMatch string
	client := dialTestServer(t, http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		ifNoneMatch = request.Header.Get("If-None-Match")
		queryResponseHeaders(response, testEntityTag)
		response.Header().Set("Content-Type", "application/json")
		_, _ = response.Write([]byte(`{"value":2}`))
	}), WithQueryCache(cache))

	value, err := client.QueryDatum(context.Background(), testQuery())
	if err != nil {
		t.Fatal(err)
	}
	if ifNoneMatch != "" || value.(map[string]any)["value"].(interface{ String() string }).String() != "2" {
		t.Fatalf("If-None-Match = %q, value = %#v", ifNoneMatch, value)
	}
	if client.QueryStats().Recoveries != 1 {
		t.Fatalf("query stats = %#v", client.QueryStats())
	}
}

func TestNotModifiedUsesTheHeldEntry(t *testing.T) {
	cache := &fixedQueryCache{
		entry: QueryCacheEntry{ETag: testEntityTag, Body: `{"value":1}`},
		found: true,
	}
	client := dialTestServer(t, http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		cache.mu.Lock()
		cache.entry = QueryCacheEntry{ETag: `W/"new"`, Body: `{"value":2}`}
		cache.mu.Unlock()
		queryResponseHeaders(response, testEntityTag)
		response.WriteHeader(http.StatusNotModified)
	}), WithQueryCache(cache))

	value, err := client.QueryDatum(context.Background(), testQuery())
	if err != nil {
		t.Fatal(err)
	}
	if value.(map[string]any)["value"].(interface{ String() string }).String() != "1" {
		t.Fatalf("value = %#v", value)
	}
}

func TestQueryErrorDoesNotReturnAHeldBody(t *testing.T) {
	cache := &fixedQueryCache{
		entry: QueryCacheEntry{ETag: testEntityTag, Body: `{"value":1}`},
		found: true,
	}
	client := dialTestServer(t, http.HandlerFunc(func(response http.ResponseWriter, _ *http.Request) {
		response.Header().Set("Content-Type", "application/problem+json")
		response.WriteHeader(http.StatusBadRequest)
		_, _ = response.Write([]byte(`{"type":"about:blank","title":"Invalid request","status":400,"detail":"bad query","code":"invalid","stage":"schema","reason":"schema_violation"}`))
	}), WithQueryCache(cache))

	value, err := client.QueryDatum(context.Background(), testQuery())
	if err == nil || value != nil {
		t.Fatalf("value = %#v, error = %v", value, err)
	}
}
