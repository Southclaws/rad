package schema_test

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"testing"
	"time"

	radclient "github.com/Southclaws/rad/rad/client"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	"github.com/Southclaws/rad/tests/harness"
)

func newDatabase(t *testing.T, options ...exec.Option) *harness.DB {
	t.Helper()
	return harness.NewSchema(t, options...)
}

func transitionGraph(t *testing.T, client *radclient.Client, ids []string) map[string]radclient.TransitionControl {
	t.Helper()
	controls, err := client.SchemaTransitions(t.Context())
	if err != nil {
		t.Fatalf("list schema transitions: %v", err)
	}
	wanted := make(map[string]bool, len(ids))
	for _, id := range ids {
		wanted[id] = true
	}
	graph := make(map[string]radclient.TransitionControl, len(ids))
	for _, control := range controls {
		if wanted[control.TransitionID] {
			graph[control.TransitionID] = control
		}
	}
	if len(graph) != len(ids) {
		t.Fatalf("transition graph has %d requested nodes, want %d: ids=%v controls=%#v", len(graph), len(ids), ids, controls)
	}
	return graph
}

func transitionIDsByKind(graph map[string]radclient.TransitionControl, kind radclient.TransitionKind) []string {
	var ids []string
	for id, control := range graph {
		if control.TransitionKind == kind {
			ids = append(ids, id)
		}
	}
	slices.Sort(ids)
	return ids
}

func requireProblemCode(t *testing.T, err error, code string) *radclient.APIError {
	t.Helper()
	var problem *radclient.APIError
	if !errors.As(err, &problem) {
		t.Fatalf("error = %v, want API error with code %q", err, code)
	}
	if problem.Problem.Code != code {
		t.Fatalf("problem code = %q, want %q: %v", problem.Problem.Code, code, err)
	}
	return problem
}

// createAcrossPublication attempts every currently valid representation of a
// row. A failed response is followed by a point read before retrying so a
// transport loss cannot turn a committed create into a duplicate-write loop.
func createAcrossPublication(
	ctx context.Context,
	client *radclient.Client,
	table string,
	key map[string]any,
	representations ...map[string]any,
) error {
	var failures []error
	for {
		failures = failures[:0]
		for _, values := range representations {
			if _, err := client.Create(ctx, table, values); err == nil {
				return nil
			} else {
				failures = append(failures, err)
			}
		}
		if _, found, err := client.Get(ctx, table, key); err == nil && found {
			return nil
		} else if err != nil {
			failures = append(failures, err)
		}
		select {
		case <-ctx.Done():
			return fmt.Errorf("create %s %v across schema publication: %w: %w", table, key, errors.Join(failures...), ctx.Err())
		case <-time.After(time.Millisecond):
		}
	}
}
