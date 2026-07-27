package store

import (
	"reflect"
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func TestCanonicalWriteProtocolOrdersEveryObligationWithoutMutatingInput(t *testing.T) {
	protocol := model.WriteProtocol{
		TableID: "t1", Generation: 9,
		ReadyIndexes: []model.Index{
			{ID: "i2", LogicalID: "ix2"},
			{ID: "i1", LogicalID: "ix1"},
		},
		DeltaSinks: []model.IndexDeltaSink{
			{TransitionID: "tr2"},
			{TransitionID: "tr1"},
		},
		ColumnReplacements: []model.ColumnReplacementWrite{
			{TransitionID: "tr4"},
			{TransitionID: "tr3"},
		},
		ConstraintChecks: []model.ConstraintCheck{
			{TransitionID: "tr6"},
			{TransitionID: "tr5"},
		},
		FinalizationGate: &model.SchemaFinalizationGate{TransitionID: "tr7"},
	}
	gateCopy := *protocol.FinalizationGate
	original := model.WriteProtocol{
		TableID: protocol.TableID, Generation: protocol.Generation,
		ReadyIndexes:       append([]model.Index(nil), protocol.ReadyIndexes...),
		DeltaSinks:         append([]model.IndexDeltaSink(nil), protocol.DeltaSinks...),
		ColumnReplacements: append([]model.ColumnReplacementWrite(nil), protocol.ColumnReplacements...),
		ConstraintChecks:   append([]model.ConstraintCheck(nil), protocol.ConstraintChecks...),
		FinalizationGate:   &gateCopy,
	}

	got := canonicalWriteProtocol(protocol)
	if !reflect.DeepEqual(protocol, original) {
		t.Fatalf("canonicalization mutated caller input:\n got: %+v\nwant: %+v", protocol, original)
	}
	if got.ReadyIndexes[0].LogicalID != "ix1" ||
		got.DeltaSinks[0].TransitionID != "tr1" ||
		got.ColumnReplacements[0].TransitionID != "tr3" ||
		got.ConstraintChecks[0].TransitionID != "tr5" ||
		got.FinalizationGate == nil || got.FinalizationGate.TransitionID != "tr7" {
		t.Fatalf("non-canonical protocol = %+v", got)
	}
}

func TestWriteProtocolRejectsLegacyMultipleFinalizationGates(t *testing.T) {
	raw := []byte(`{
		"table_id":"t1",
		"generation":9,
		"finalization_gates":[
			{"transition_id":"tr1","object_id":"ix1","kind":"index_build"},
			{"transition_id":"tr2","object_id":"ix2","kind":"index_build"}
		]
	}`)
	_, err := decodeWriteProtocolDefinition("items", raw)
	if err == nil || !strings.Contains(err.Error(), "finalization_gates") {
		t.Fatalf("multiple-gate definition error = %v", err)
	}
	reason, ok := reject.ReasonOf(err)
	if !ok || reason != reject.ReasonCatalogCorrupt {
		t.Fatalf("multiple-gate reason = %q, ok=%v, want catalog_corrupt", reason, ok)
	}
}
