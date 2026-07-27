package concurrent

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"sync"
	"testing"
	"time"

	"github.com/Southclaws/rad/rad/engine/02_catalog/change"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

type scheduleStep struct {
	Actor      string          `json:"actor"`
	Point      exec.YieldPoint `json:"point"`
	Entity     string          `json:"entity,omitempty"`
	Occurrence uint64          `json:"occurrence"`
}

type scheduledArrival struct {
	step    scheduleStep
	release chan struct{}
}

type scheduleController struct {
	mu          sync.Mutex
	armed       bool
	disarmed    chan struct{}
	occurrences map[string]uint64
	arrivals    chan scheduledArrival
	recorded    []scheduleStep
}

func newScheduleController() *scheduleController {
	return &scheduleController{
		occurrences: make(map[string]uint64),
		arrivals:    make(chan scheduledArrival, 64),
	}
}

func (c *scheduleController) arm() {
	c.mu.Lock()
	c.disarmed = make(chan struct{})
	c.armed = true
	c.mu.Unlock()
}

func (c *scheduleController) disarm() {
	c.mu.Lock()
	if c.armed {
		c.armed = false
		close(c.disarmed)
	}
	c.mu.Unlock()
}

func (c *scheduleController) hook(ctx context.Context, event exec.YieldEvent) {
	c.mu.Lock()
	if !c.armed {
		c.mu.Unlock()
		return
	}
	disarmed := c.disarmed
	key := fmt.Sprintf("%s\x00%s\x00%s", event.Actor, event.Point, event.Entity)
	c.occurrences[key]++
	step := scheduleStep{
		Actor: event.Actor, Point: event.Point, Entity: event.Entity,
		Occurrence: c.occurrences[key],
	}
	c.mu.Unlock()

	release := make(chan struct{})
	select {
	case c.arrivals <- scheduledArrival{step: step, release: release}:
	case <-ctx.Done():
		return
	}
	select {
	case <-release:
	case <-disarmed:
	case <-ctx.Done():
	}
}

func (c *scheduleController) drive(ctx context.Context, schedule []scheduleStep) error {
	var waiting []scheduledArrival
	for _, expected := range schedule {
		for {
			if index := slices.IndexFunc(waiting, func(arrival scheduledArrival) bool {
				return scheduleStepMatches(expected, arrival.step)
			}); index >= 0 {
				arrival := waiting[index]
				waiting = slices.Delete(waiting, index, index+1)
				c.recorded = append(c.recorded, arrival.step)
				close(arrival.release)
				break
			}
			select {
			case arrival := <-c.arrivals:
				if slices.ContainsFunc(waiting, func(blocked scheduledArrival) bool {
					return blocked.step == arrival.step
				}) {
					return fmt.Errorf("duplicate blocked yield event %+v", arrival.step)
				}
				waiting = append(waiting, arrival)
			case <-ctx.Done():
				return fmt.Errorf(
					"waiting for yield event %+v with arrivals %+v: %w",
					expected,
					waiting,
					ctx.Err(),
				)
			}
		}
	}
	if len(waiting) != 0 {
		return fmt.Errorf("schedule left %d arrived events blocked: %+v", len(waiting), waiting)
	}
	return nil
}

func scheduleStepMatches(expected, actual scheduleStep) bool {
	return expected.Actor == actual.Actor &&
		expected.Point == actual.Point &&
		expected.Occurrence == actual.Occurrence &&
		(expected.Entity == actual.Entity || expected.Entity == "*")
}

func TestSemanticScheduleIsExactlyReplayable(t *testing.T) {
	assertSemanticScheduleReplay(
		t,
		"rename-during-write.json",
		runRenameDuringWriteSchedule,
		"renamed:row-present",
	)
}

func assertSemanticScheduleReplay(
	t *testing.T,
	fixture string,
	run func(*testing.T, []scheduleStep) ([]scheduleStep, string),
	wantState string,
) {
	t.Helper()
	raw, err := os.ReadFile(filepath.Join("testdata", "schedules", fixture))
	if err != nil {
		t.Fatal(err)
	}
	var schedule []scheduleStep
	if err := json.Unmarshal(raw, &schedule); err != nil {
		t.Fatal(err)
	}
	firstSchedule, firstState := run(t, schedule)
	recordedRaw, err := json.Marshal(firstSchedule)
	if err != nil {
		t.Fatal(err)
	}
	var replay []scheduleStep
	if err := json.Unmarshal(recordedRaw, &replay); err != nil {
		t.Fatal(err)
	}
	secondSchedule, secondState := run(t, replay)
	replayedRaw, err := json.Marshal(secondSchedule)
	if err != nil {
		t.Fatal(err)
	}
	if string(replayedRaw) != string(recordedRaw) {
		t.Fatalf("schedule replay differs byte-for-byte:\nfirst  %s\nsecond %s", recordedRaw, replayedRaw)
	}
	if firstState != secondState || firstState != wantState {
		t.Fatalf("replayed state first=%q second=%q", firstState, secondState)
	}
}

func runRenameDuringWriteSchedule(t *testing.T, schedule []scheduleStep) ([]scheduleStep, string) {
	t.Helper()
	controller := newScheduleController()
	db := newChaosDB(t, exec.WithYieldHook(controller.hook))
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	t.Cleanup(cancel)
	tableID := pirwire.SchemaID(40_000)
	idColumn := pirwire.SchemaID(1)
	valueColumn := pirwire.SchemaID(2)
	if _, err := db.Control.Execute(ctx, pirwire.Prog("",
		pirwire.CreateTable("items", pirwire.TableDefinition{
			ID: &tableID, Name: "scheduled_items",
			Columns: []pirwire.ColumnDefinition{
				{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
				{ID: &valueColumn, Name: "value", Type: pirwire.ColumnTypeText},
			},
			PrimaryKey: []string{"id"},
		}),
	)); err != nil {
		t.Fatal(err)
	}
	controller.arm()

	results := make(chan error, 2)
	go func() {
		actorCtx := exec.WithYieldActor(ctx, "writer")
		results <- db.Harness.Insert(actorCtx, "scheduled_items", lir.Row{
			"id": lir.Int64(1), "value": lir.Text("kept-across-rename"),
		})
	}()
	go func() {
		actorCtx := exec.WithYieldActor(ctx, "renamer")
		results <- db.Harness.CatalogTxn(actorCtx, func(_ *exec.Tx, mutation *change.Mutation) error {
			return mutation.RenameTableBySchemaID(actorCtx, model.SchemaID(tableID), "renamed_items")
		})
	}()

	if err := controller.drive(ctx, schedule); err != nil {
		t.Fatal(err)
	}
	for range 2 {
		if err := <-results; err != nil {
			t.Fatalf("scheduled operation: %v", err)
		}
	}
	controller.disarm()
	row, found, err := db.Harness.GetByPrimaryKey(ctx, "renamed_items", lir.Row{"id": lir.Int64(1)})
	if err != nil || !found || row["value"].Text != "kept-across-rename" {
		t.Fatalf("scheduled final row: found=%v row=%v err=%v", found, row, err)
	}
	if _, found, err := db.Harness.GetByPrimaryKey(ctx, "scheduled_items", lir.Row{"id": lir.Int64(1)}); err == nil || found {
		t.Fatalf("old name remained bindable: found=%v err=%v", found, err)
	}
	return append([]scheduleStep(nil), controller.recorded...), "renamed:row-present"
}
