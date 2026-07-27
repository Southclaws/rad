package concurrent

import (
	"context"
	"testing"
)

func TestConcurrentScenarios(t *testing.T) {
	for _, loaded := range loadScenarios(t) {
		s := loaded
		t.Run(s.Name, func(t *testing.T) {
			ctx, cancel := context.WithTimeout(context.Background(), s.timeout)
			defer cancel()

			w := newWorkload(t, s)
			defer func() {
				path, err := w.journal.writeIfRequested()
				if err != nil {
					t.Errorf("write concurrency journal: %v", err)
				} else if path != "" {
					t.Logf("concurrency journal: %s", path)
				}
			}()
			if err := w.bootstrap(ctx); err != nil {
				t.Fatalf("bootstrap: %v", err)
			}

			for round := 0; round < s.Rounds; round++ {
				actions := w.actionsForRound(round)
				if err := w.director.runRound(ctx, round, actions); err != nil {
					t.Fatalf("%v\nreplay: RAD_CONCURRENT_SEED=%d RAD_CONCURRENT_ROUNDS=%d go test ./tests/concurrent -run %s\njournal tail:\n%s",
						err, s.Seed, s.Rounds, s.Name, w.journal.tail(80))
				}
			}
			if err := w.awaitScheduler(ctx); err != nil {
				t.Fatalf("await automatic index build: %v\njournal tail:\n%s", err, w.journal.tail(80))
			}
			if err := w.assertFinal(ctx); err != nil {
				t.Fatalf("quiescent invariant: %v\njournal tail:\n%s", err, w.journal.tail(80))
			}

			actionsPerRound := s.HTTPWriters + s.PostgresWriters + s.HTTPReaders +
				s.PostgresReaders + s.MetadataAdds + 4
			t.Logf("seed=%d rounds=%d concurrent_actions=%d outside_operations>=%d",
				s.Seed, s.Rounds, actionsPerRound, actionsPerRound*s.Rounds)
		})
	}
}
