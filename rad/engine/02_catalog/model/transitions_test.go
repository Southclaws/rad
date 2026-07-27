package model

import (
	"encoding/json"
	"testing"
)

func TestTransitionControlKeepsWorkerInternalsOffTheWire(t *testing.T) {
	control := SchemaTransition{
		ID: "tr42", Kind: TransitionIndexBuild, State: TransitionCatchingUp,
		Generation: 7, OwnerEpoch: 11, BasePosition: "base-internal",
		BarrierPosition: "barrier-internal", WorkState: TransitionWorkDegraded,
		Index: Index{LogicalID: "ix43"}, RowsScanned: 12, AppliedDelta: 3,
		DeltaHighWater: 5,
	}.Control()
	raw, err := json.Marshal(control)
	if err != nil {
		t.Fatal(err)
	}
	var object map[string]any
	if err := json.Unmarshal(raw, &object); err != nil {
		t.Fatal(err)
	}
	for _, internal := range []string{"owner_epoch", "base_position", "barrier_position", "work_state"} {
		if _, exists := object[internal]; exists {
			t.Fatalf("control result exposed internal field %q: %s", internal, raw)
		}
	}
	if object["retained_work_state"] != string(TransitionWorkDegraded) {
		t.Fatalf("retained_work_state = %#v, want degraded: %s", object["retained_work_state"], raw)
	}
	if object["delta_lag"] != float64(2) {
		t.Fatalf("delta_lag = %#v, want 2: %s", object["delta_lag"], raw)
	}
}
