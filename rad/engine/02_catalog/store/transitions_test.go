package store

import (
	"bytes"
	"context"
	"encoding/json"
	"strconv"
	"testing"
	"time"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func validTransition(id string) model.SchemaTransition {
	now := time.Now().UTC()
	return model.SchemaTransition{
		ID: id, Kind: model.TransitionIndexBuild, State: model.TransitionBuilding,
		Generation: 1, TableID: "t1", TableSchemaID: 1,
		CreatedAt: now, UpdatedAt: now,
	}
}

func TestTransitionStorageValidatesDurableCheckpoints(t *testing.T) {
	ctx := context.Background()
	database := openCatalogStore(t)

	invalid := validTransition("tr-invalid")
	invalid.Generation = 0
	if err := SaveTransition(ctx, database, invalid); err == nil {
		t.Fatal("saved a transition with generation zero")
	}

	invalid = validTransition("tr-invalid-state")
	invalid.State = model.TransitionState("surprising")
	if err := SaveTransition(ctx, database, invalid); err == nil {
		t.Fatal("saved a transition with an unknown lifecycle state")
	}
	raw, err := json.Marshal(invalid)
	if err != nil {
		t.Fatal(err)
	}
	if err := database.Put(ctx, transitionKey(invalid.ID), raw); err != nil {
		t.Fatal(err)
	}
	if _, _, err := GetTransition(ctx, database, invalid.ID); err == nil {
		t.Fatal("read a transition with an unknown lifecycle state")
	}

	invalid = validTransition("tr-invalid-limits")
	invalid.DeltaSoftLimit = 20
	invalid.DeltaHardLimit = 10
	if err := SaveTransition(ctx, database, invalid); err == nil {
		t.Fatal("saved a transition whose soft limit exceeds its hard limit")
	}
}

func TestTransitionStorageRejectsKeyIdentityDrift(t *testing.T) {
	ctx := context.Background()
	database := openCatalogStore(t)
	transition := validTransition("tr-original")
	if err := SaveTransition(ctx, database, transition); err != nil {
		t.Fatal(err)
	}
	raw, ok, err := database.Get(ctx, transitionKey(transition.ID))
	if err != nil || !ok {
		t.Fatalf("read original transition: found=%v err=%v", ok, err)
	}
	if err := database.Put(ctx, transitionKey("tr-alias"), raw); err != nil {
		t.Fatal(err)
	}
	if _, _, err := GetTransition(ctx, database, "tr-alias"); err == nil {
		t.Fatal("accepted a transition record stored under another identity")
	}
	if _, err := ListTransitions(ctx, database); err == nil {
		t.Fatal("listed a transition record stored under another identity")
	}
}

func TestCompactedTerminalTransitionRemainsValid(t *testing.T) {
	ctx := context.Background()
	database := openCatalogStore(t)
	transition := validTransition("tr-ready")
	transition.State = model.TransitionReady
	if err := SaveTransition(ctx, database, transition); err != nil {
		t.Fatal(err)
	}
	if err := CompactTransition(ctx, database, transition.ID, time.Now().UTC()); err != nil {
		t.Fatal(err)
	}
	got, ok, err := GetTransition(ctx, database, transition.ID)
	if err != nil || !ok || got.CompactedAt.IsZero() {
		t.Fatalf("compacted transition: found=%v transition=%+v err=%v", ok, got, err)
	}
}

func TestDurableWorkListsRejectKeyIdentityDrift(t *testing.T) {
	ctx := context.Background()
	t.Run("reclamation", func(t *testing.T) {
		database := openCatalogStore(t)
		reclamation := model.Reclamation{
			ID: "reclaim-original", Kind: model.ReclamationTable,
			RetiredCatalogVersion: 1, TableID: "t1", TableSchemaID: 1,
		}
		if err := QueueReclamation(ctx, database, reclamation); err != nil {
			t.Fatal(err)
		}
		raw, ok, err := database.Get(ctx, reclamationKey(reclamation.ID))
		if err != nil || !ok {
			t.Fatalf("read reclamation: found=%v err=%v", ok, err)
		}
		if err := database.Put(ctx, reclamationKey("reclaim-alias"), raw); err != nil {
			t.Fatal(err)
		}
		if _, err := ListReclamations(ctx, database); err == nil {
			t.Fatal("listed a reclamation record stored under another identity")
		}
	})

	t.Run("retention pin", func(t *testing.T) {
		database := openCatalogStore(t)
		pin := model.RetentionPin{
			ID: "pin-original", OwnerKind: model.RetentionOwnerPreparedPlan,
			OwnerID: "plan-1", Resource: model.RetentionResource{
				Kind: model.RetentionTableDefinition, TableSchemaID: 1,
				DefinitionGeneration: 1,
			},
		}
		if err := SaveRetentionPin(ctx, database, pin); err != nil {
			t.Fatal(err)
		}
		raw, ok, err := database.Get(ctx, retentionPinKey(pin.ID))
		if err != nil || !ok {
			t.Fatalf("read retention pin: found=%v err=%v", ok, err)
		}
		if err := database.Put(ctx, retentionPinKey("pin-alias"), raw); err != nil {
			t.Fatal(err)
		}
		if _, err := ListRetentionPins(ctx, database); err == nil {
			t.Fatal("listed a retention pin stored under another identity")
		}
	})
}

func TestDurableWorkDecodersRejectUnknownAndTrailingFields(t *testing.T) {
	ctx := context.Background()
	t.Run("transition unknown field", func(t *testing.T) {
		database := openCatalogStore(t)
		transition := validTransition("tr-future")
		raw, err := json.Marshal(transition)
		if err != nil {
			t.Fatal(err)
		}
		raw = append(raw[:len(raw)-1], []byte(`,"future_field":true}`)...)
		if err := database.Put(ctx, transitionKey(transition.ID), raw); err != nil {
			t.Fatal(err)
		}
		if _, _, err := GetTransition(ctx, database, transition.ID); err == nil {
			t.Fatal("decoded a transition with an unknown durable field")
		} else {
			requireCatalogCorrupt(t, err)
		}
	})

	t.Run("transition trailing value", func(t *testing.T) {
		database := openCatalogStore(t)
		transition := validTransition("tr-trailing")
		raw, err := json.Marshal(transition)
		if err != nil {
			t.Fatal(err)
		}
		raw = append(raw, []byte(` {}`)...)
		if err := database.Put(ctx, transitionKey(transition.ID), raw); err != nil {
			t.Fatal(err)
		}
		if _, _, err := GetTransition(ctx, database, transition.ID); err == nil {
			t.Fatal("decoded a transition with a trailing JSON value")
		}
	})

	t.Run("reclamation unknown field", func(t *testing.T) {
		database := openCatalogStore(t)
		reclamation := model.Reclamation{
			ID: "reclaim-future", Kind: model.ReclamationTable,
			RetiredCatalogVersion: 1, TableID: "t1", TableSchemaID: 1,
		}
		if err := QueueReclamation(ctx, database, reclamation); err != nil {
			t.Fatal(err)
		}
		raw, ok, err := database.Get(ctx, reclamationKey(reclamation.ID))
		if err != nil || !ok {
			t.Fatalf("read reclamation: found=%v err=%v", ok, err)
		}
		raw = append(raw[:len(raw)-1], []byte(`,"future_field":true}`)...)
		if err := database.Put(ctx, reclamationKey(reclamation.ID), raw); err != nil {
			t.Fatal(err)
		}
		if _, _, err := GetReclamation(ctx, database, reclamation.ID); err == nil {
			t.Fatal("decoded a reclamation with an unknown durable field")
		}
	})

	t.Run("retention pin unknown field", func(t *testing.T) {
		database := openCatalogStore(t)
		pin := model.RetentionPin{
			ID: "pin-future", OwnerKind: model.RetentionOwnerPreparedPlan,
			OwnerID: "plan-1", Resource: model.RetentionResource{
				Kind: model.RetentionTableDefinition, TableSchemaID: 1,
				DefinitionGeneration: 1,
			},
		}
		if err := SaveRetentionPin(ctx, database, pin); err != nil {
			t.Fatal(err)
		}
		raw, ok, err := database.Get(ctx, retentionPinKey(pin.ID))
		if err != nil || !ok {
			t.Fatalf("read retention pin: found=%v err=%v", ok, err)
		}
		raw = append(raw[:len(raw)-1], []byte(`,"future_field":true}`)...)
		if err := database.Put(ctx, retentionPinKey(pin.ID), raw); err != nil {
			t.Fatal(err)
		}
		if _, _, err := GetRetentionPin(ctx, database, pin.ID); err == nil {
			t.Fatal("decoded a retention pin with an unknown durable field")
		}
	})
}

func TestIndexDeltaDecodeBindsOrderedKeyToDurableIdentity(t *testing.T) {
	ctx := context.Background()
	database := openCatalogStore(t)
	delta := model.IndexDelta{
		Operation: model.IndexDeltaPut,
		PK:        []byte("pk"),
		Tuple:     []byte("tuple"),
	}
	sequence, err := AppendIndexDelta(ctx, database, "tr-delta", 0, delta)
	if err != nil {
		t.Fatal(err)
	}
	key := DeltaKey("tr-delta", sequence)
	raw, ok, err := database.Get(ctx, key)
	if err != nil || !ok {
		t.Fatalf("read delta: found=%v err=%v", ok, err)
	}
	got, err := DecodeIndexDelta("tr-delta", key, raw)
	if err != nil || got.Sequence != sequence || got.Operation != model.IndexDeltaPut {
		t.Fatalf("decode valid delta: delta=%+v err=%v", got, err)
	}

	for _, test := range []struct {
		name string
		key  []byte
		raw  []byte
	}{
		{name: "wrong transition", key: key, raw: raw},
		{name: "noncanonical key", key: []byte("/rad/catalog/transition_delta/tr-delta/1"), raw: raw},
		{name: "trailing value", key: key, raw: append(bytes.Clone(raw), []byte(` {}`)...)},
	} {
		t.Run(test.name, func(t *testing.T) {
			transitionID := "tr-delta"
			if test.name == "wrong transition" {
				transitionID = "tr-other"
			}
			if _, err := DecodeIndexDelta(transitionID, test.key, test.raw); err == nil {
				t.Fatal("decoded a delta whose durable key/value identity was invalid")
			} else {
				requireCatalogCorrupt(t, err)
			}
		})
	}

	tampered := got
	tampered.Sequence++
	raw, err = json.Marshal(tampered)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := DecodeIndexDelta("tr-delta", key, raw); err == nil {
		t.Fatal("decoded a delta whose embedded sequence disagreed with its key")
	}
	tampered = got
	tampered.Operation = model.IndexDeltaOperation("surprising")
	raw, err = json.Marshal(tampered)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := DecodeIndexDelta("tr-delta", key, raw); err == nil {
		t.Fatal("decoded a delta with an unknown operation")
	}
}

func TestIndexDeltaSequenceExhaustionFailsClosed(t *testing.T) {
	ctx := context.Background()
	database := openCatalogStore(t)
	if err := database.Put(ctx, deltaSequenceKey("tr-exhausted"), []byte(strconv.FormatUint(^uint64(0), 10))); err != nil {
		t.Fatal(err)
	}
	if _, err := AppendIndexDelta(ctx, database, "tr-exhausted", 0, model.IndexDelta{
		Operation: model.IndexDeltaPut,
	}); err == nil {
		t.Fatal("wrapped an exhausted durable delta sequence")
	}
}

func TestUniqueClaimRejectsKeyValueIdentityDrift(t *testing.T) {
	ctx := context.Background()
	database := openCatalogStore(t)
	tuple := []byte("tuple")
	pk := []byte("pk-1")
	if err := PutUniqueClaim(ctx, database, "tr-claim", tuple, pk); err != nil {
		t.Fatal(err)
	}
	start, end := UniqueClaimRange("tr-claim")
	it, err := database.Scan(ctx, start, end)
	if err != nil {
		t.Fatal(err)
	}
	if !it.Next() {
		_ = it.Close()
		t.Fatal("missing unique claim")
	}
	key := bytes.Clone(it.Key())
	if err := it.Close(); err != nil {
		t.Fatal(err)
	}
	raw, err := json.Marshal(model.UniqueIndexClaim{Tuple: tuple, PK: []byte("other-pk")})
	if err != nil {
		t.Fatal(err)
	}
	if err := database.Put(ctx, key, raw); err != nil {
		t.Fatal(err)
	}
	if err := PutUniqueClaim(ctx, database, "tr-claim", tuple, []byte("pk-2")); err == nil {
		t.Fatal("accepted a unique claim whose key and value identify different rows")
	} else {
		requireCatalogCorrupt(t, err)
	}
}

func requireCatalogCorrupt(t *testing.T, err error) {
	t.Helper()
	reason, marked := reject.ReasonOf(err)
	if !marked || reason != reject.ReasonCatalogCorrupt {
		t.Fatalf("storage error = %v, reason=%q marked=%v", err, reason, marked)
	}
}
