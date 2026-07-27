package concurrent

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"testing"
	"time"
)

type scenario struct {
	Name            string `json:"name"`
	Seed            int64  `json:"seed"`
	InitialRows     int    `json:"initial_rows"`
	Rounds          int    `json:"rounds"`
	HTTPWriters     int    `json:"http_writers"`
	PostgresWriters int    `json:"postgres_writers"`
	HTTPReaders     int    `json:"http_readers"`
	PostgresReaders int    `json:"postgres_readers"`
	MetadataAdds    int    `json:"metadata_adds_per_round"`
	IndexBatchSize  int    `json:"index_batch_size"`
	MaxRetries      int    `json:"max_retries"`
	Timeout         string `json:"timeout"`
	timeout         time.Duration
}

func loadScenarios(t *testing.T) []scenario {
	t.Helper()
	paths, err := filepath.Glob(filepath.Join("testdata", "scenarios", "*.json"))
	if err != nil {
		t.Fatal(err)
	}
	if len(paths) == 0 {
		t.Fatal("concurrent: no scenario definitions")
	}

	result := make([]scenario, 0, len(paths))
	for _, path := range paths {
		raw, err := os.ReadFile(path)
		if err != nil {
			t.Fatalf("read scenario %s: %v", path, err)
		}
		var s scenario
		if err := json.Unmarshal(raw, &s); err != nil {
			t.Fatalf("decode scenario %s: %v", path, err)
		}
		if err := applyScenarioOverrides(&s); err != nil {
			t.Fatalf("scenario %s overrides: %v", path, err)
		}
		if err := s.validate(); err != nil {
			t.Fatalf("scenario %s: %v", path, err)
		}
		result = append(result, s)
	}
	return result
}

func applyScenarioOverrides(s *scenario) error {
	if raw := os.Getenv("RAD_CONCURRENT_SEED"); raw != "" {
		seed, err := strconv.ParseInt(raw, 10, 64)
		if err != nil {
			return fmt.Errorf("RAD_CONCURRENT_SEED: %w", err)
		}
		s.Seed = seed
	}
	if raw := os.Getenv("RAD_CONCURRENT_ROUNDS"); raw != "" {
		rounds, err := strconv.Atoi(raw)
		if err != nil {
			return fmt.Errorf("RAD_CONCURRENT_ROUNDS: %w", err)
		}
		s.Rounds = rounds
	}
	return nil
}

func (s *scenario) validate() error {
	if s.Name == "" {
		return fmt.Errorf("name is required")
	}
	if s.InitialRows < s.HTTPWriters || s.InitialRows < 1 {
		return fmt.Errorf("initial_rows %d must cover %d HTTP writer-owned rows", s.InitialRows, s.HTTPWriters)
	}
	if s.Rounds < 1 || s.HTTPWriters < 1 || s.PostgresWriters < 1 {
		return fmt.Errorf("rounds and both writer counts must be positive")
	}
	if s.HTTPReaders < 1 || s.PostgresReaders < 1 || s.MetadataAdds < 1 {
		return fmt.Errorf("both reader counts and metadata_adds_per_round must be positive")
	}
	if s.IndexBatchSize < 1 || s.MaxRetries < 1 {
		return fmt.Errorf("index_batch_size and max_retries must be positive")
	}
	timeout, err := time.ParseDuration(s.Timeout)
	if err != nil || timeout <= 0 {
		return fmt.Errorf("timeout %q is invalid", s.Timeout)
	}
	s.timeout = timeout
	return nil
}
