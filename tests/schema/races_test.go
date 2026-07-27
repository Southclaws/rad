package schema_test

import (
	"slices"
	"sync"
	"testing"

	radclient "github.com/Southclaws/rad/rad/client"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	"github.com/Southclaws/rad/rad/protocol"
)

const oneTextValue = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: string, nullable: true }
`

func TestCompetingSchemaAppliesHaveOneSerializableWinner(t *testing.T) {
	ctx := t.Context()
	db := newDatabase(t, exec.WithSchemaJobScheduling(false))
	db.SchemaMigrateReady(oneTextValue, false)

	const integerTarget = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: int64, nullable: true }
`
	const booleanTarget = `
tables:
  - id: 1
    name: events
    columns:
      - { id: 1, name: id, type: int64, pk: true }
      - { id: 2, name: value, type: bool, nullable: true }
`

	base := db.SchemaPlan(integerTarget)
	other := db.SchemaPlan(booleanTarget)
	if base.CurrentVersion != other.CurrentVersion || base.CurrentHash != other.CurrentHash {
		t.Fatalf("preflights did not bind the same catalog: integer=%#v boolean=%#v", base, other)
	}

	type outcome struct {
		target    string
		diff      protocol.SchemaDiff
		migration protocol.SchemaMigration
		err       error
	}
	outcomes := make(chan outcome, 2)
	start := make(chan struct{})
	var wait sync.WaitGroup
	for _, candidate := range []outcome{{target: integerTarget, diff: base}, {target: booleanTarget, diff: other}} {
		client, err := radclient.Dial(db.URL)
		if err != nil {
			t.Fatal(err)
		}
		wait.Add(1)
		go func(candidate outcome) {
			defer wait.Done()
			<-start
			candidate.migration, candidate.err = client.SchemaMigrate(ctx, candidate.target, protocol.SchemaIdentity{
				SchemaVersion: candidate.diff.CurrentVersion,
				SchemaHash:    candidate.diff.CurrentHash,
			}, false)
			outcomes <- candidate
		}(candidate)
	}
	close(start)
	wait.Wait()
	close(outcomes)

	var winner, loser outcome
	for result := range outcomes {
		if result.err == nil {
			if winner.target != "" {
				t.Fatalf("both competing migrations won: %#v and %#v", winner.migration, result.migration)
			}
			winner = result
		} else {
			if loser.target != "" {
				t.Fatalf("both competing migrations failed: %v and %v", loser.err, result.err)
			}
			loser = result
		}
	}
	if winner.target == "" || loser.target == "" {
		t.Fatalf("winner=%#v loser=%#v", winner, loser)
	}
	requireProblemCode(t, loser.err, protocol.CodeConflict)
	if winner.migration.State != protocol.SchemaMigrationConverging || len(winner.migration.TransitionIDs) != 1 {
		t.Fatalf("winner = %#v", winner.migration)
	}

	replayed, err := db.Client.SchemaMigrate(t.Context(), winner.target, protocol.SchemaIdentity{
		SchemaVersion: winner.diff.CurrentVersion,
		SchemaHash:    winner.diff.CurrentHash,
	}, false)
	if err != nil || !slices.Equal(replayed.TransitionIDs, winner.migration.TransitionIDs) {
		t.Fatalf("lost-response replay = %#v, %v; want IDs %v", replayed, err, winner.migration.TransitionIDs)
	}
	blocked := db.SchemaPlan(loser.target)
	if len(blocked.Blocking) != 1 || blocked.Blocking[0].Kind != "active_schema_transition_conflict" {
		t.Fatalf("losing target findings = %#v", blocked.Blocking)
	}

	if _, err := db.Client.CancelSchemaTransition(t.Context(), winner.migration.TransitionIDs[0]); err != nil {
		t.Fatal(err)
	}
	fresh := db.SchemaPlan(loser.target)
	if len(fresh.Blocking) != 0 {
		t.Fatalf("terminal winner still blocks a fresh target: %#v", fresh.Blocking)
	}
	retried, err := db.SchemaApply(loser.target, fresh, false)
	if err != nil {
		t.Fatalf("apply loser after cancellation: %v", err)
	}
	if len(retried.TransitionIDs) != 1 || slices.Contains(winner.migration.TransitionIDs, retried.TransitionIDs[0]) {
		t.Fatalf("fresh migration reused cancelled identity: old=%v new=%v", winner.migration.TransitionIDs, retried.TransitionIDs)
	}
	if _, err := db.Client.CancelSchemaTransition(t.Context(), retried.TransitionIDs[0]); err != nil {
		t.Fatal(err)
	}
}

func TestCancelledDependentGraphCanBeStartedFresh(t *testing.T) {
	db := newDatabase(t, exec.WithSchemaJobScheduling(false))
	db.SchemaMigrateReady(twoValueColumns, false)

	first := db.SchemaMigrate(replacedValueColumns, false)
	if len(first.TransitionIDs) != 3 {
		t.Fatalf("first graph = %#v", first)
	}
	firstGraph := transitionGraph(t, db.Client, first.TransitionIDs)
	indexIDs := transitionIDsByKind(firstGraph, radclient.TransitionIndexBuild)
	replacementIDs := transitionIDsByKind(firstGraph, radclient.TransitionColumnReplacement)
	for _, id := range append(indexIDs, replacementIDs...) {
		cancelled, err := db.Client.CancelSchemaTransition(t.Context(), id)
		if err != nil {
			t.Fatalf("cancel transition %q: %v", id, err)
		}
		if cancelled.State != radclient.TransitionCancelled {
			t.Fatalf("cancelled transition %q state = %q", id, cancelled.State)
		}
	}

	second := db.SchemaMigrate(replacedValueColumns, false)
	if len(second.TransitionIDs) != 3 {
		t.Fatalf("second graph = %#v", second)
	}
	for _, id := range second.TransitionIDs {
		if slices.Contains(first.TransitionIDs, id) {
			t.Fatalf("fresh graph reused cancelled transition %q: first=%v second=%v", id, first.TransitionIDs, second.TransitionIDs)
		}
	}
	secondGraph := transitionGraph(t, db.Client, second.TransitionIDs)
	secondReplacements := transitionIDsByKind(secondGraph, radclient.TransitionColumnReplacement)
	secondIndexes := transitionIDsByKind(secondGraph, radclient.TransitionIndexBuild)
	if len(secondIndexes) != 1 || len(secondReplacements) != 2 {
		t.Fatalf("second graph shape = %#v", secondGraph)
	}
	prerequisites := slices.Clone(secondGraph[secondIndexes[0]].Prerequisites)
	slices.Sort(prerequisites)
	if !slices.Equal(prerequisites, secondReplacements) {
		t.Fatalf("fresh graph prerequisites = %v, want %v", prerequisites, secondReplacements)
	}
	for _, id := range append(secondIndexes, secondReplacements...) {
		if _, err := db.Client.CancelSchemaTransition(t.Context(), id); err != nil {
			t.Fatalf("clean up transition %q: %v", id, err)
		}
	}
}
