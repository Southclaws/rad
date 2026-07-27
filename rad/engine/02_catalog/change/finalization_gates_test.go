package change

import (
	"context"
	"errors"
	"fmt"
	"reflect"
	"testing"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/reject"
)

func openFinalizationGateDatabase(t *testing.T) *kvslate.Store {
	t.Helper()
	database, err := kvslate.Open("finalization-gates-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = database.Close() })
	return database
}

func TestMultiTableFinalizationGatesAreCanonicalAtomicAndReleasable(t *testing.T) {
	ctx := context.Background()
	database := openFinalizationGateDatabase(t)
	service := New(database)
	left, err := service.CreateTable(ctx, model.TableDef{
		Name: "left", Columns: []model.ColumnDef{{Name: "id", Type: model.TypeInt64}},
		PrimaryKey: []string{"id"},
	})
	if err != nil {
		t.Fatal(err)
	}
	right, err := service.CreateTable(ctx, model.TableDef{
		Name: "right", Columns: []model.ColumnDef{{Name: "id", Type: model.TypeInt64}},
		PrimaryKey: []string{"id"},
	})
	if err != nil {
		t.Fatal(err)
	}
	transition := model.SchemaTransition{
		ID: "tr-gates", Kind: model.TransitionConstraintValidation, ObjectID: "ct-test",
		GateTableIDs: []string{right.ID, left.ID, right.ID},
	}
	wantIDs := []string{left.ID, right.ID}
	if left.ID > right.ID {
		wantIDs[0], wantIDs[1] = wantIDs[1], wantIDs[0]
	}
	if got := canonicalGateTableIDs(transition.GateTableIDs); !reflect.DeepEqual(got, wantIDs) {
		t.Fatalf("canonical gate IDs = %v, want %v", got, wantIDs)
	}

	txn, err := database.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		t.Fatal(err)
	}
	mutation := &Mutation{view: txn}
	if err := mutation.acquireSchemaFinalizationGates(ctx, transition); err != nil {
		_ = txn.Rollback()
		t.Fatal(err)
	}
	if err := txn.Commit(ctx); err != nil {
		t.Fatal(err)
	}
	for _, id := range wantIDs {
		table, ok, err := store.New(database).GetTableByID(ctx, id)
		if err != nil || !ok {
			t.Fatalf("get gated table %q: ok=%v err=%v", id, ok, err)
		}
		protocol, err := store.ReadWriteProtocol(ctx, database, table)
		if err != nil {
			t.Fatal(err)
		}
		if protocol.FinalizationGate == nil ||
			protocol.FinalizationGate.TransitionID != transition.ID {
			t.Fatalf("table %q gate = %+v", table.Name, protocol.FinalizationGate)
		}
	}

	// Reacquiring the same transition commits without publishing another write
	// protocol generation.
	generationByTable := make(map[string]uint64, len(wantIDs))
	for _, id := range wantIDs {
		table, ok, err := store.New(database).GetTableByID(ctx, id)
		if err != nil || !ok {
			t.Fatalf("get gated table %q: ok=%v err=%v", id, ok, err)
		}
		generationByTable[id] = table.WriteProtocolGeneration
	}
	txn, err = database.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		t.Fatal(err)
	}
	mutation = &Mutation{view: txn}
	if err := mutation.acquireSchemaFinalizationGates(ctx, transition); err != nil {
		_ = txn.Rollback()
		t.Fatalf("reacquire same gates: %v", err)
	}
	if err := txn.Commit(ctx); err != nil {
		t.Fatal(err)
	}
	for _, id := range wantIDs {
		table, ok, err := store.New(database).GetTableByID(ctx, id)
		if err != nil || !ok {
			t.Fatalf("get reacquired table %q: ok=%v err=%v", id, ok, err)
		}
		if table.WriteProtocolGeneration != generationByTable[id] {
			t.Fatalf(
				"table %q protocol generation changed on reacquisition: %d -> %d",
				table.Name,
				generationByTable[id],
				table.WriteProtocolGeneration,
			)
		}
	}

	// A different transition cannot share any table in the gated set.
	txn, err = database.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		t.Fatal(err)
	}
	mutation = &Mutation{view: txn}
	competing := transition
	competing.ID = "tr-competing"
	if err := mutation.acquireSchemaFinalizationGates(ctx, competing); !errors.Is(err, kv.ErrConflict) {
		_ = txn.Rollback()
		t.Fatalf("competing gate error = %v, want conflict", err)
	}
	if err := txn.Rollback(); err != nil {
		t.Fatal(err)
	}

	txn, err = database.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		t.Fatal(err)
	}
	mutation = &Mutation{view: txn}
	if err := mutation.releaseSchemaFinalizationGates(ctx, transition); err != nil {
		_ = txn.Rollback()
		t.Fatal(err)
	}
	if err := txn.Commit(ctx); err != nil {
		t.Fatal(err)
	}
	for _, id := range wantIDs {
		table, ok, err := store.New(database).GetTableByID(ctx, id)
		if err != nil || !ok {
			t.Fatalf("get released table %q: ok=%v err=%v", id, ok, err)
		}
		protocol, err := store.ReadWriteProtocol(ctx, database, table)
		if err != nil {
			t.Fatal(err)
		}
		if protocol.FinalizationGate != nil {
			t.Fatalf("table %q retained gate: %+v", table.Name, protocol.FinalizationGate)
		}
	}
}

func TestBeginIndexValidationReacquiresOwnGateAndAdvancesState(t *testing.T) {
	ctx := context.Background()
	database := openFinalizationGateDatabase(t)
	service := New(database)
	table, err := service.CreateTable(ctx, model.TableDef{
		Name: "items",
		Columns: []model.ColumnDef{
			{Name: "id", Type: model.TypeInt64},
			{Name: "value", Type: model.TypeText},
		},
		PrimaryKey: []string{"id"},
	})
	if err != nil {
		t.Fatal(err)
	}
	transition, err := startIndexBuildForFinalizationTest(ctx, service, table.SchemaID, model.IndexDef{
		Name: "items_value_key", Columns: []string{"value"}, Unique: true,
	})
	if err != nil {
		t.Fatal(err)
	}
	if transition.State != model.TransitionBuilding {
		t.Fatalf("started transition state = %q, want building", transition.State)
	}

	const ownerEpoch = uint64(7)
	transition.State = model.TransitionCatchingUp
	transition.OwnerEpoch = ownerEpoch
	txn, err := database.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		t.Fatal(err)
	}
	mutation := &Mutation{view: txn}
	if err := store.SaveTransition(ctx, txn, transition); err != nil {
		_ = txn.Rollback()
		t.Fatal(err)
	}
	if err := mutation.acquireSchemaFinalizationGates(ctx, transition); err != nil {
		_ = txn.Rollback()
		t.Fatal(err)
	}
	if err := txn.Commit(ctx); err != nil {
		t.Fatal(err)
	}

	gatedTable, ok, err := store.New(database).GetTableByID(ctx, table.ID)
	if err != nil || !ok {
		t.Fatalf("get gated table: ok=%v err=%v", ok, err)
	}
	gateGeneration := gatedTable.WriteProtocolGeneration

	txn, err = database.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		t.Fatal(err)
	}
	mutation = &Mutation{view: txn}
	validating, err := mutation.BeginIndexValidation(ctx, transition.ID, ownerEpoch)
	if err != nil {
		_ = txn.Rollback()
		t.Fatal(err)
	}
	if err := txn.Commit(ctx); err != nil {
		t.Fatal(err)
	}
	if validating.State != model.TransitionValidating {
		t.Fatalf("reacquired transition state = %q, want validating", validating.State)
	}
	if validating.Generation != transition.Generation+1 {
		t.Fatalf(
			"reacquired transition generation = %d, want %d",
			validating.Generation,
			transition.Generation+1,
		)
	}

	validatedTable, ok, err := store.New(database).GetTableByID(ctx, table.ID)
	if err != nil || !ok {
		t.Fatalf("get validating table: ok=%v err=%v", ok, err)
	}
	if validatedTable.WriteProtocolGeneration != gateGeneration {
		t.Fatalf(
			"reacquisition republished write protocol: %d -> %d",
			gateGeneration,
			validatedTable.WriteProtocolGeneration,
		)
	}
	index, ok := validatedTable.Index("items_value_key")
	if !ok || index.State != model.IndexValidating {
		t.Fatalf("validating index = %+v, ok=%v", index, ok)
	}
	protocol, err := store.ReadWriteProtocol(ctx, database, validatedTable)
	if err != nil {
		t.Fatal(err)
	}
	if protocol.FinalizationGate == nil || protocol.FinalizationGate.TransitionID != transition.ID {
		t.Fatalf("reacquired finalization gate = %+v", protocol.FinalizationGate)
	}
}

func TestFinalizationGateReacquisitionRejectsIdentityDrift(t *testing.T) {
	ctx := context.Background()
	database := openFinalizationGateDatabase(t)
	service := New(database)
	table, err := service.CreateTable(ctx, model.TableDef{
		Name:       "items",
		Columns:    []model.ColumnDef{{Name: "id", Type: model.TypeInt64}},
		PrimaryKey: []string{"id"},
	})
	if err != nil {
		t.Fatal(err)
	}
	transition := model.SchemaTransition{
		ID: "tr-gate-drift", Kind: model.TransitionConstraintValidation,
		ObjectID: "constraint:1", GateTableIDs: []string{table.ID},
	}

	txn, err := database.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		t.Fatal(err)
	}
	mutation := &Mutation{view: txn}
	if err := mutation.acquireSchemaFinalizationGates(ctx, transition); err != nil {
		_ = txn.Rollback()
		t.Fatal(err)
	}
	if err := txn.Commit(ctx); err != nil {
		t.Fatal(err)
	}

	txn, err = database.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		t.Fatal(err)
	}
	table, ok, err := store.New(txn).GetTableByID(ctx, table.ID)
	if err != nil || !ok {
		_ = txn.Rollback()
		t.Fatalf("get table: found=%v err=%v", ok, err)
	}
	protocol, err := store.ReadWriteProtocol(ctx, txn, table)
	if err != nil {
		_ = txn.Rollback()
		t.Fatal(err)
	}
	protocol.Generation++
	protocol.FinalizationGate.ObjectID = "constraint:other"
	table.WriteProtocolGeneration = protocol.Generation
	if err := store.SaveTable(ctx, txn, table); err != nil {
		_ = txn.Rollback()
		t.Fatal(err)
	}
	if err := store.SaveWriteProtocol(ctx, txn, protocol); err != nil {
		_ = txn.Rollback()
		t.Fatal(err)
	}
	if err := txn.Commit(ctx); err != nil {
		t.Fatal(err)
	}

	txn, err = database.Begin(ctx, kv.SerializableSnapshot)
	if err != nil {
		t.Fatal(err)
	}
	defer txn.Rollback()
	err = (&Mutation{view: txn}).acquireSchemaFinalizationGates(ctx, transition)
	reason, marked := reject.ReasonOf(err)
	if !marked || reason != reject.ReasonCatalogDrift {
		t.Fatalf("reacquire drift error = %v, reason=%q marked=%v", err, reason, marked)
	}
}

func TestValidatingTransitionRequiresItsExactGateAtRemoval(t *testing.T) {
	transition := model.SchemaTransition{
		ID: "tr-validating", Kind: model.TransitionConstraintValidation,
		ObjectID: "constraint:1", State: model.TransitionValidating,
	}
	for _, test := range []struct {
		name      string
		gate      *model.SchemaFinalizationGate
		wantDrift bool
	}{
		{name: "exact", gate: func() *model.SchemaFinalizationGate {
			gate := schemaFinalizationGate(transition)
			return &gate
		}()},
		{name: "missing", wantDrift: true},
		{name: "same ID wrong object", gate: &model.SchemaFinalizationGate{
			TransitionID: transition.ID, ObjectID: "constraint:other", Kind: transition.Kind,
		}, wantDrift: true},
		{name: "different owner", gate: &model.SchemaFinalizationGate{
			TransitionID: "tr-other", ObjectID: "constraint:other", Kind: transition.Kind,
		}, wantDrift: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			protocol := model.WriteProtocol{FinalizationGate: test.gate}
			err := removeOwnedSchemaFinalizationGate(&protocol, transition)
			reason, marked := reject.ReasonOf(err)
			if test.wantDrift {
				if !marked || reason != reject.ReasonCatalogDrift {
					t.Fatalf("remove gate error = %v, reason=%q marked=%v", err, reason, marked)
				}
				return
			}
			if err != nil || protocol.FinalizationGate != nil {
				t.Fatalf("exact gate removal: gate=%+v err=%v", protocol.FinalizationGate, err)
			}
		})
	}
}

func TestIndexCannotPublishReadyBeforeValidation(t *testing.T) {
	for _, unique := range []bool{false, true} {
		t.Run(fmt.Sprintf("unique=%v", unique), func(t *testing.T) {
			ctx := context.Background()
			database := openFinalizationGateDatabase(t)
			service := New(database)
			table, err := service.CreateTable(ctx, model.TableDef{
				Name: "items",
				Columns: []model.ColumnDef{
					{Name: "id", Type: model.TypeInt64},
					{Name: "value", Type: model.TypeText},
				},
				PrimaryKey: []string{"id"},
			})
			if err != nil {
				t.Fatal(err)
			}
			transition, err := startIndexBuildForFinalizationTest(ctx, service, table.SchemaID, model.IndexDef{
				Name: "items_value_key", Columns: []string{"value"}, Unique: unique,
			})
			if err != nil {
				t.Fatal(err)
			}
			transition.State = model.TransitionCatchingUp
			transition.OwnerEpoch = 1

			txn, err := database.Begin(ctx, kv.SerializableSnapshot)
			if err != nil {
				t.Fatal(err)
			}
			if err := store.SaveTransition(ctx, txn, transition); err != nil {
				_ = txn.Rollback()
				t.Fatal(err)
			}
			if err := txn.Commit(ctx); err != nil {
				t.Fatal(err)
			}

			txn, err = database.Begin(ctx, kv.SerializableSnapshot)
			if err != nil {
				t.Fatal(err)
			}
			defer txn.Rollback()
			_, err = (&Mutation{view: txn}).PublishIndexReady(ctx, transition.ID, transition.OwnerEpoch)
			reason, marked := reject.ReasonOf(err)
			if !marked || reason != reject.ReasonCatalogDrift {
				t.Fatalf("premature publication = %v, reason=%q marked=%v", err, reason, marked)
			}
		})
	}
}

func startIndexBuildForFinalizationTest(
	ctx context.Context,
	service *Service,
	tableID model.SchemaID,
	definition model.IndexDef,
) (model.SchemaTransition, error) {
	var transition model.SchemaTransition
	err := service.mutate(ctx, func(mutation *Mutation) error {
		var err error
		transition, err = mutation.StartIndexBuild(ctx, tableID, definition)
		return err
	})
	return transition, err
}

func TestNonUniqueIndexValidationDoesNotRequireFinalizationGate(t *testing.T) {
	transition := model.SchemaTransition{
		ID: "tr-index", Kind: model.TransitionIndexBuild,
		State: model.TransitionValidating, Index: model.Index{Unique: false},
	}
	protocol := model.WriteProtocol{}
	if err := removeOwnedSchemaFinalizationGate(&protocol, transition); err != nil {
		t.Fatalf("non-unique validation without gate: %v", err)
	}
}
