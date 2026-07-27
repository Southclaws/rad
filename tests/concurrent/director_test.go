package concurrent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"math/rand"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"
)

type action struct {
	Round  int
	Actor  string
	Kind   string
	Detail map[string]any
	Run    func(context.Context) (map[string]any, error)
}

type actionResult struct {
	action action
	detail map[string]any
	err    error
}

type journalEvent struct {
	Sequence uint64         `json:"sequence"`
	Round    int            `json:"round"`
	Actor    string         `json:"actor"`
	Kind     string         `json:"kind"`
	Status   string         `json:"status"`
	Attempt  int            `json:"attempt,omitempty"`
	Detail   map[string]any `json:"detail,omitempty"`
	Error    string         `json:"error,omitempty"`
}

type journal struct {
	scenario scenario
	mu       sync.Mutex
	events   []journalEvent
}

func newJournal(s scenario) *journal {
	return &journal{scenario: s}
}

func (j *journal) record(event journalEvent) {
	j.mu.Lock()
	defer j.mu.Unlock()
	event.Sequence = uint64(len(j.events) + 1)
	j.events = append(j.events, event)
}

func (j *journal) retry(round int, actor, kind string, attempt int, err error) {
	j.record(journalEvent{
		Round: round, Actor: actor, Kind: kind, Status: "retry", Attempt: attempt, Error: err.Error(),
	})
}

func (j *journal) tail(limit int) string {
	j.mu.Lock()
	defer j.mu.Unlock()
	start := len(j.events) - limit
	if start < 0 {
		start = 0
	}
	var out strings.Builder
	enc := json.NewEncoder(&out)
	for _, event := range j.events[start:] {
		_ = enc.Encode(event)
	}
	return out.String()
}

func (j *journal) writeIfRequested() (string, error) {
	dir := os.Getenv("RAD_CONCURRENT_JOURNAL")
	if dir == "" {
		return "", nil
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return "", err
	}
	path := filepath.Join(dir, fmt.Sprintf("%s-seed-%d.json", j.scenario.Name, j.scenario.Seed))
	j.mu.Lock()
	payload := struct {
		Scenario scenario       `json:"scenario"`
		Events   []journalEvent `json:"events"`
	}{Scenario: j.scenario, Events: append([]journalEvent(nil), j.events...)}
	j.mu.Unlock()
	raw, err := json.MarshalIndent(payload, "", "  ")
	if err != nil {
		return "", err
	}
	if err := os.WriteFile(path, raw, 0o644); err != nil {
		return "", err
	}
	return path, nil
}

type director struct {
	journal *journal
	random  *rand.Rand
}

func (d *director) runRound(ctx context.Context, round int, actions []action) error {
	actions = append([]action(nil), actions...)
	d.random.Shuffle(len(actions), func(i, k int) { actions[i], actions[k] = actions[k], actions[i] })
	release := make(chan struct{})
	results := make(chan actionResult, len(actions))
	for _, item := range actions {
		item := item
		go func() {
			<-release
			d.journal.record(journalEvent{
				Round: item.Round, Actor: item.Actor, Kind: item.Kind, Status: "started", Detail: item.Detail,
			})
			result := actionResult{action: item}
			func() {
				defer func() {
					if recovered := recover(); recovered != nil {
						result.err = fmt.Errorf("panic: %v", recovered)
					}
				}()
				result.detail, result.err = item.Run(ctx)
			}()
			results <- result
		}()
	}
	d.journal.record(journalEvent{
		Round: round, Actor: "director", Kind: "round", Status: "released",
		Detail: map[string]any{"actions": len(actions)},
	})
	close(release)

	var failures []error
	for range actions {
		select {
		case <-ctx.Done():
			return fmt.Errorf("round %d watchdog: %w", round, ctx.Err())
		case result := <-results:
			status := "succeeded"
			if result.err != nil {
				status = "failed"
				failures = append(failures, fmt.Errorf("%s/%s: %w", result.action.Actor, result.action.Kind, result.err))
			}
			event := journalEvent{
				Round: result.action.Round, Actor: result.action.Actor, Kind: result.action.Kind,
				Status: status, Detail: result.detail,
			}
			if result.err != nil {
				event.Error = result.err.Error()
			}
			d.journal.record(event)
		}
	}
	if len(failures) > 0 {
		return fmt.Errorf("round %d had %d failures: %v", round, len(failures), failures)
	}
	return nil
}

func retry[T any](
	ctx context.Context,
	journal *journal,
	a action,
	max int,
	isConflict func(error) bool,
	fn func() (T, error),
) (T, error) {
	return retryAttempts(ctx, max, 0, isConflict, func(attempt int, err error) {
		journal.retry(a.Round, a.Actor, a.Kind, attempt, err)
	}, func(int) (T, error) {
		return fn()
	})
}

// retryAttempts is the one retry loop used by the outside-in concurrent
// suite. Callers supply only the error policy and operation shape, keeping
// retry bounds, cancellation, yielding, and exhaustion reporting consistent.
func retryAttempts[T any](
	ctx context.Context,
	max int,
	delay time.Duration,
	isRetryable func(error) bool,
	onRetry func(attempt int, err error),
	fn func(attempt int) (T, error),
) (T, error) {
	var zero T
	if max <= 0 {
		return zero, fmt.Errorf("retry attempts must be positive, got %d", max)
	}
	var lastErr error
	for attempt := range max {
		select {
		case <-ctx.Done():
			return zero, ctx.Err()
		default:
		}
		value, err := fn(attempt)
		if err == nil {
			return value, nil
		}
		if !isRetryable(err) {
			return zero, err
		}
		lastErr = err
		if attempt+1 == max {
			break
		}
		if onRetry != nil {
			onRetry(attempt+1, err)
		}
		if delay > 0 {
			timer := time.NewTimer(delay)
			select {
			case <-ctx.Done():
				timer.Stop()
				return zero, ctx.Err()
			case <-timer.C:
			}
			continue
		}
		select {
		case <-ctx.Done():
			return zero, ctx.Err()
		default:
			runtime.Gosched()
		}
	}
	return zero, fmt.Errorf("exhausted %d retries: %w", max, lastErr)
}

func TestRetryAttemptsContract(t *testing.T) {
	t.Run("success after bounded retries", func(t *testing.T) {
		attempts := 0
		var retries []int
		got, err := retryAttempts(t.Context(), 4, 0, func(error) bool { return true },
			func(attempt int, _ error) { retries = append(retries, attempt) },
			func(attempt int) (int, error) {
				attempts++
				if attempt < 2 {
					return 0, errors.New("retry")
				}
				return 42, nil
			})
		if err != nil || got != 42 || attempts != 3 || !slices.Equal(retries, []int{1, 2}) {
			t.Fatalf("retry result=%d attempts=%d retries=%v err=%v", got, attempts, retries, err)
		}
	})

	t.Run("exhaustion does not report a nonexistent retry", func(t *testing.T) {
		attempts, retries := 0, 0
		_, err := retryAttempts(t.Context(), 3, 0, func(error) bool { return true },
			func(int, error) { retries++ },
			func(int) (struct{}, error) {
				attempts++
				return struct{}{}, errors.New("retry")
			})
		if err == nil || attempts != 3 || retries != 2 {
			t.Fatalf("attempts=%d retries=%d err=%v", attempts, retries, err)
		}
	})

	t.Run("cancelled context performs no operation", func(t *testing.T) {
		ctx, cancel := context.WithCancel(t.Context())
		cancel()
		attempts := 0
		_, err := retryAttempts(ctx, 3, 0, func(error) bool { return true }, nil,
			func(int) (struct{}, error) {
				attempts++
				return struct{}{}, nil
			})
		if !errors.Is(err, context.Canceled) || attempts != 0 {
			t.Fatalf("attempts=%d err=%v", attempts, err)
		}
	})
}
