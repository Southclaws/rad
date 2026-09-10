package rad

import (
	"container/list"
	"context"
	"errors"
	"sync"
)

const (
	defaultQueryCacheBytes     = 32 * 1024 * 1024
	defaultQueryCacheEntries   = 1024
	defaultQueryCacheResultMax = 4 * 1024 * 1024
	queryCacheKeyBytes         = 32
)

// QueryCacheKey identifies one query request in one client isolation scope.
type QueryCacheKey struct {
	digest [queryCacheKeyBytes]byte
}

// QueryCacheEntry holds one validated entity tag and its immutable JSON body.
type QueryCacheEntry struct {
	ETag string
	Body string
}

// QueryCache stores query response bodies for conditional requests.
type QueryCache interface {
	Get(context.Context, QueryCacheKey) (QueryCacheEntry, bool, error)
	Put(context.Context, QueryCacheKey, QueryCacheEntry) error
	Delete(context.Context, QueryCacheKey) error
}

// QueryCacheLimits bounds memory use by total bytes, entry count, and body size.
type QueryCacheLimits struct {
	Bytes          int
	Entries        int
	MaxResultBytes int
}

// DefaultQueryCacheLimits returns the fixed memory-cache resource limits.
func DefaultQueryCacheLimits() QueryCacheLimits {
	return QueryCacheLimits{
		Bytes:          defaultQueryCacheBytes,
		Entries:        defaultQueryCacheEntries,
		MaxResultBytes: defaultQueryCacheResultMax,
	}
}

// MemoryQueryCacheStats is one consistent memory-cache statistics snapshot.
type MemoryQueryCacheStats struct {
	Entries            int
	RetainedBytes      int
	Hits               uint64
	Misses             uint64
	Admissions         uint64
	Replacements       uint64
	Evictions          uint64
	OversizedResults   uint64
	EntryLimitPressure uint64
	ByteLimitPressure  uint64
	PressureRejections uint64
}

type memoryQueryCacheEntry struct {
	key    QueryCacheKey
	value  QueryCacheEntry
	weight int
}

// MemoryQueryCache is a thread-safe LRU query cache with fixed policy.
type MemoryQueryCache struct {
	mu       sync.Mutex
	limits   QueryCacheLimits
	entries  map[QueryCacheKey]*list.Element
	recency  list.List
	retained int
	stats    MemoryQueryCacheStats
}

// NewMemoryQueryCache creates a memory cache. Omit limits to use the defaults.
func NewMemoryQueryCache(limits ...QueryCacheLimits) (*MemoryQueryCache, error) {
	if len(limits) > 1 {
		return nil, errors.New("rad: query cache accepts at most one limits value")
	}
	selected := DefaultQueryCacheLimits()
	if len(limits) == 1 {
		selected = limits[0]
	}
	if selected.Bytes <= 0 || selected.Entries <= 0 || selected.MaxResultBytes <= 0 {
		return nil, errors.New("rad: query cache limits must be positive")
	}
	return &MemoryQueryCache{
		limits:  selected,
		entries: make(map[QueryCacheKey]*list.Element),
	}, nil
}

func (c *MemoryQueryCache) Get(ctx context.Context, key QueryCacheKey) (QueryCacheEntry, bool, error) {
	if err := ctx.Err(); err != nil {
		return QueryCacheEntry{}, false, err
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	element, ok := c.entries[key]
	if !ok {
		c.stats.Misses++
		return QueryCacheEntry{}, false, nil
	}
	c.recency.MoveToFront(element)
	c.stats.Hits++
	return element.Value.(*memoryQueryCacheEntry).value, true, nil
}

func (c *MemoryQueryCache) Put(ctx context.Context, key QueryCacheKey, value QueryCacheEntry) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if len(value.Body) > c.limits.MaxResultBytes {
		c.stats.OversizedResults++
		c.removeExisting(key)
		return nil
	}
	weight := queryCacheKeyBytes + len(value.ETag) + len(value.Body)
	if weight > c.limits.Bytes {
		c.stats.ByteLimitPressure++
		c.stats.PressureRejections++
		c.removeExisting(key)
		return nil
	}
	if element, ok := c.entries[key]; ok {
		entry := element.Value.(*memoryQueryCacheEntry)
		c.retained -= entry.weight
		entry.value = value
		entry.weight = weight
		c.retained += weight
		c.recency.MoveToFront(element)
		c.stats.Replacements++
		c.evictToLimits()
		return nil
	}
	entry := &memoryQueryCacheEntry{key: key, value: value, weight: weight}
	element := c.recency.PushFront(entry)
	c.entries[key] = element
	c.retained += weight
	c.stats.Admissions++
	c.evictToLimits()
	return nil
}

func (c *MemoryQueryCache) Delete(ctx context.Context, key QueryCacheKey) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if element, ok := c.entries[key]; ok {
		c.remove(element, false)
	}
	return nil
}

// Stats returns one consistent statistics snapshot.
func (c *MemoryQueryCache) Stats() MemoryQueryCacheStats {
	c.mu.Lock()
	defer c.mu.Unlock()
	stats := c.stats
	stats.Entries = len(c.entries)
	stats.RetainedBytes = c.retained
	return stats
}

func (c *MemoryQueryCache) evictToLimits() {
	entryPressure := len(c.entries) > c.limits.Entries
	bytePressure := c.retained > c.limits.Bytes
	if entryPressure {
		c.stats.EntryLimitPressure++
	}
	if bytePressure {
		c.stats.ByteLimitPressure++
	}
	for len(c.entries) > c.limits.Entries || c.retained > c.limits.Bytes {
		c.remove(c.recency.Back(), true)
	}
}

func (c *MemoryQueryCache) remove(element *list.Element, eviction bool) {
	entry := element.Value.(*memoryQueryCacheEntry)
	delete(c.entries, entry.key)
	c.recency.Remove(element)
	c.retained -= entry.weight
	if eviction {
		c.stats.Evictions++
	}
}

func (c *MemoryQueryCache) removeExisting(key QueryCacheKey) {
	if element, ok := c.entries[key]; ok {
		c.remove(element, false)
	}
}
