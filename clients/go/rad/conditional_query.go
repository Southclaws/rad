package rad

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"sync/atomic"

	"github.com/Southclaws/rad/clients/go/api/oas"
	"github.com/Southclaws/rad/clients/go/protocol"
	"github.com/Southclaws/rad/clients/go/protocol/lirwire"
)

const (
	queryRequestMediaType  = "application/vnd.rad.lir+json"
	queryResponseMediaType = "application/json"
)

// ConditionalQueryStats is one client statistics snapshot.
type ConditionalQueryStats struct {
	Changed          uint64
	Unchanged        uint64
	CacheErrors      uint64
	Recoveries       uint64
	RecoveryFailures uint64
}

type conditionalQueryCounters struct {
	changed          atomic.Uint64
	unchanged        atomic.Uint64
	cacheErrors      atomic.Uint64
	recoveries       atomic.Uint64
	recoveryFailures atomic.Uint64
}

// QueryStats returns one concurrent-safe statistics snapshot.
func (c *Client) QueryStats() ConditionalQueryStats {
	return ConditionalQueryStats{
		Changed:          c.queryStats.changed.Load(),
		Unchanged:        c.queryStats.unchanged.Load(),
		CacheErrors:      c.queryStats.cacheErrors.Load(),
		Recoveries:       c.queryStats.recoveries.Load(),
		RecoveryFailures: c.queryStats.recoveryFailures.Load(),
	}
}

func fillQueryIsolation(token *[queryCacheKeyBytes]byte) error {
	if _, err := rand.Read(token[:]); err != nil {
		return fmt.Errorf("rad: create query cache isolation token: %w", err)
	}
	return nil
}

func (c *Client) queryDatum(ctx context.Context, query lirwire.Query) (any, error) {
	if err := c.ensureSchema(ctx); err != nil {
		return nil, err
	}
	raw, err := protocol.MarshalQuery(query)
	if err != nil {
		return nil, err
	}
	key := c.queryKey(raw)
	entry, held, cacheUsable := c.loadQueryEntry(ctx, key)
	if !cacheUsable {
		return c.queryUnconditional(ctx, raw)
	}
	return c.queryWithRecovery(ctx, raw, key, entry, held)
}

func (c *Client) loadQueryEntry(
	ctx context.Context,
	key QueryCacheKey,
) (QueryCacheEntry, bool, bool) {
	if c.queryCache == nil {
		return QueryCacheEntry{}, false, true
	}
	entry, found, err := c.queryCache.Get(ctx, key)
	if err != nil {
		c.queryStats.cacheErrors.Add(1)
		return QueryCacheEntry{}, false, false
	}
	if !found {
		return QueryCacheEntry{}, false, true
	}
	if !validETag(entry.ETag) || !json.Valid([]byte(entry.Body)) {
		if err := c.queryCache.Delete(ctx, key); err != nil {
			c.queryStats.cacheErrors.Add(1)
			return QueryCacheEntry{}, false, false
		}
		c.queryStats.recoveries.Add(1)
		return QueryCacheEntry{}, false, true
	}
	return entry, true, true
}

func (c *Client) queryUnconditional(ctx context.Context, raw []byte) (any, error) {
	response, err := c.oas.Query(ctx, oas.Query(raw), oas.QueryParams{})
	if err != nil {
		return nil, transportError(err)
	}
	body, _, err := c.queryResponseBody(response, QueryCacheEntry{}, false)
	if err != nil {
		return nil, err
	}
	c.queryStats.changed.Add(1)
	return decodeQueryBody(body)
}

func (c *Client) queryWithRecovery(
	ctx context.Context,
	raw []byte,
	key QueryCacheKey,
	held QueryCacheEntry,
	hasHeld bool,
) (any, error) {
	for attempt := 0; attempt < 2; attempt++ {
		params := oas.QueryParams{}
		if hasHeld {
			params.IfNoneMatch.SetTo(held.ETag)
		}
		response, err := c.oas.Query(ctx, oas.Query(raw), params)
		if err != nil {
			return nil, transportError(err)
		}
		body, changed, err := c.queryResponseBody(response, held, hasHeld)
		if err == nil {
			if changed {
				c.queryStats.changed.Add(1)
				if c.queryCache != nil {
					ok := response.(*oas.QueryOKHeaders)
					entry := QueryCacheEntry{ETag: ok.ETag, Body: body}
					if err := c.queryCache.Put(ctx, key, entry); err != nil {
						c.queryStats.cacheErrors.Add(1)
					}
				}
			} else {
				c.queryStats.unchanged.Add(1)
			}
			return decodeQueryBody(body)
		}
		if !errors.Is(err, errQueryCacheRecovery) || attempt != 0 {
			if errors.Is(err, errQueryCacheRecovery) {
				c.queryStats.recoveryFailures.Add(1)
			}
			return nil, err
		}
		if c.queryCache != nil {
			if deleteErr := c.queryCache.Delete(ctx, key); deleteErr != nil {
				c.queryStats.cacheErrors.Add(1)
				return c.queryUnconditional(ctx, raw)
			}
		}
		c.queryStats.recoveries.Add(1)
		hasHeld = false
		held = QueryCacheEntry{}
	}
	return nil, errQueryCacheRecovery
}

var errQueryCacheRecovery = errors.New("rad: inconsistent conditional query response")

func (c *Client) queryResponseBody(
	response oas.QueryRes,
	held QueryCacheEntry,
	hasHeld bool,
) (string, bool, error) {
	switch response := response.(type) {
	case *oas.QueryOKHeaders:
		body := string(response.Response)
		if !validETag(response.ETag) || !json.Valid([]byte(body)) {
			return "", false, errors.New("rad: invalid conditional query response")
		}
		return body, true, nil
	case *oas.QueryNotModified:
		if !hasHeld || !validETag(response.ETag) || !weakETagEqual(response.ETag, held.ETag) {
			return "", false, errQueryCacheRecovery
		}
		return held.Body, false, nil
	case *oas.QueryBadRequest:
		return "", false, apiError(oas.Problem(*response))
	case *oas.QueryNotAcceptable:
		return "", false, apiError(oas.Problem(*response))
	case *oas.QueryRequestEntityTooLarge:
		return "", false, apiError(oas.Problem(*response))
	case *oas.QueryUnsupportedMediaType:
		return "", false, apiError(oas.Problem(*response))
	case *oas.QueryUnprocessableEntity:
		c.schema.invalidate()
		return "", false, apiError(oas.Problem(*response))
	case *oas.InternalServerErrorStatusCode:
		return "", false, apiError(response.Response)
	default:
		return "", false, fmt.Errorf("rad: unexpected QUERY response %T", response)
	}
}

func decodeQueryBody(body string) (any, error) {
	return decodeResult(oas.Value([]byte(body)))
}

func (c *Client) queryKey(request []byte) QueryCacheKey {
	hash := sha256.New()
	writeQueryKeyField(hash, 1, c.queryIsolation[:])
	writeQueryKeyField(hash, 2, []byte(c.endpoint))
	writeQueryKeyField(hash, 3, request)
	writeQueryKeyField(hash, 4, []byte(queryRequestMediaType))
	writeQueryKeyField(hash, 5, []byte(queryResponseMediaType))
	var key QueryCacheKey
	copy(key.digest[:], hash.Sum(nil))
	return key
}

type queryKeyHash interface {
	Write([]byte) (int, error)
}

func writeQueryKeyField(hash queryKeyHash, tag byte, value []byte) {
	var length [8]byte
	binary.BigEndian.PutUint64(length[:], uint64(len(value)))
	_, _ = hash.Write([]byte{tag})
	_, _ = hash.Write(length[:])
	_, _ = hash.Write(value)
}

func validETag(value string) bool {
	if len(value) >= 2 && value[:2] == "W/" {
		value = value[2:]
	}
	if len(value) < 2 || value[0] != '"' || value[len(value)-1] != '"' {
		return false
	}
	for _, value := range []byte(value[1 : len(value)-1]) {
		if value == 0x21 || value >= 0x23 && value <= 0x7e || value >= 0x80 {
			continue
		}
		return false
	}
	return true
}

func weakETagEqual(left, right string) bool {
	if len(left) >= 2 && left[:2] == "W/" {
		left = left[2:]
	}
	if len(right) >= 2 && right[:2] == "W/" {
		right = right[2:]
	}
	return left == right
}
