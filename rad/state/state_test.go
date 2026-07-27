package state_test

import (
	"errors"
	"os"
	"path/filepath"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/protocol"
	projectstate "github.com/Southclaws/rad/rad/state"
)

func TestSnapshotName(t *testing.T) {
	if got := projectstate.SnapshotName(4); got != "00000004.rad.schema.yaml" {
		t.Fatalf("SnapshotName(4) = %q", got)
	}
	if got := projectstate.SnapshotName(42); got != "00000042.rad.schema.yaml" {
		t.Fatalf("SnapshotName(42) = %q", got)
	}
}

func TestWriteAcceptedCreatesPureSnapshotAndLock(t *testing.T) {
	store := projectstate.New(filepath.Join(t.TempDir(), "rad.state"))
	server := schemaState(t, 4, "users")
	accepted, err := store.WriteAccepted(server)
	if err != nil {
		t.Fatal(err)
	}
	if accepted.Lock.Snapshot != "changelog/00000004.rad.schema.yaml" {
		t.Fatalf("snapshot = %q", accepted.Lock.Snapshot)
	}
	source, err := os.ReadFile(store.SnapshotPath(4))
	if err != nil {
		t.Fatal(err)
	}
	if _, _, err := projectstate.ParseSnapshot(store.SnapshotPath(4), source); err != nil {
		t.Fatalf("snapshot is not a pure schema: %v", err)
	}
	loaded, err := store.Load()
	if err != nil {
		t.Fatal(err)
	}
	if loaded.Lock.SchemaVersion != 4 || loaded.Lock.SchemaHash != server.SchemaHash {
		t.Fatalf("loaded lock = %+v", loaded.Lock)
	}
}

func TestWriteSnapshotReusesMatchAndRejectsCollision(t *testing.T) {
	store := projectstate.New(filepath.Join(t.TempDir(), "rad.state"))
	users := schemaState(t, 5, "users")
	first, err := store.WriteSnapshot(users)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.WriteSnapshot(users); err != nil {
		t.Fatalf("matching snapshot was not reused: %v", err)
	}
	accounts := schemaState(t, 5, "accounts")
	_, err = store.WriteSnapshot(accounts)
	var divergence *projectstate.DivergenceError
	if !errors.As(err, &divergence) {
		t.Fatalf("collision error = %v, want DivergenceError", err)
	}
	unchanged, err := os.ReadFile(store.SnapshotPath(5))
	if err != nil {
		t.Fatal(err)
	}
	if string(unchanged) != string(first.Source) {
		t.Fatal("conflicting snapshot was overwritten")
	}
}

func TestLoadRejectsMissingAndCorruptSnapshot(t *testing.T) {
	store := projectstate.New(filepath.Join(t.TempDir(), "rad.state"))
	server := schemaState(t, 3, "users")
	if _, err := store.WriteAccepted(server); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(store.SnapshotPath(3)); err != nil {
		t.Fatal(err)
	}
	if _, err := store.Load(); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("missing snapshot error = %v", err)
	}
	if _, err := store.WriteAccepted(server); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(store.SnapshotPath(3), []byte("tables: nope\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := store.Load(); err == nil {
		t.Fatal("corrupt snapshot was accepted")
	}
}

func schemaState(t *testing.T, version uint64, tableName string) protocol.SchemaState {
	t.Helper()
	document := protocol.SchemaDocument{Tables: []protocol.TableDef{{
		ID: 1, Name: tableName,
		Columns:    []protocol.ColumnDef{{ID: 1, Name: "id", Type: "text"}},
		PrimaryKey: []string{"id"},
	}}}
	state := protocol.SchemaState{SchemaVersion: version, Schema: document}
	hash, err := model.SchemaFromDefinitions([]model.TableDef{{
		ID: 1, Name: tableName,
		Columns:    []model.ColumnDef{{ID: 1, Name: "id", Type: model.TypeText}},
		PrimaryKey: []string{"id"},
	}}).Hash()
	if err != nil {
		t.Fatal(err)
	}
	state.SchemaHash = hash
	return state
}
