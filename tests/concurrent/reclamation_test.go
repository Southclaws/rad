package concurrent

import (
	"context"
	"encoding/json"
	"fmt"
	"sync"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	catalogstore "github.com/Southclaws/rad/rad/engine/02_catalog/store"
	"github.com/Southclaws/rad/rad/engine/05_exec/codec"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

func TestAutomaticReclamationDoesNotGateUnrelatedTraffic(t *testing.T) {
	db := newChaosDB(t)
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	liveID, deadID := pirwire.SchemaID(20), pirwire.SchemaID(21)
	idColumn, valueColumn := pirwire.SchemaID(1), pirwire.SchemaID(2)
	if _, err := db.Control.Execute(ctx, pirwire.Prog("",
		pirwire.CreateTable("live", pirwire.TableDefinition{
			ID: &liveID, Name: "reclaim_live",
			Columns: []pirwire.ColumnDefinition{
				{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
				{ID: &valueColumn, Name: "value", Type: pirwire.ColumnTypeText},
			},
			PrimaryKey: []string{"id"},
		}),
		pirwire.CreateTable("dead", pirwire.TableDefinition{
			ID: &deadID, Name: "reclaim_dead",
			Columns: []pirwire.ColumnDefinition{
				{ID: &idColumn, Name: "id", Type: pirwire.ColumnTypeInt64},
				{ID: &valueColumn, Name: "value", Type: pirwire.ColumnTypeText},
			},
			PrimaryKey: []string{"id"},
			Indexes:    []pirwire.IndexDefinition{{Name: "reclaim_dead_value_idx", Columns: []string{"value"}}},
		}),
	)); err != nil {
		t.Fatal(err)
	}
	relation := twoColumnRows(t, 320)
	if _, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.Create("seed", "reclaim_dead", relation))); err != nil {
		t.Fatal(err)
	}
	dead, ok, err := db.Catalog.GetTable(ctx, "reclaim_dead")
	if err != nil || !ok {
		t.Fatalf("dead table: ok=%v err=%v", ok, err)
	}
	deadIndex, _ := dead.Index("reclaim_dead_value_idx")
	oldSnapshot, err := db.store.Begin(ctx, kv.Snapshot)
	if err != nil {
		t.Fatal(err)
	}
	defer oldSnapshot.Rollback()

	writers := make([]*pgx.Conn, 4)
	for i := range writers {
		writers[i] = db.postgresClient(t)
	}
	readers := []*pgx.Conn{db.postgresClient(t), db.postgresClient(t)}
	start := make(chan struct{})
	errs := make(chan error, len(writers)+len(readers)+1)
	var wg sync.WaitGroup
	for actor, conn := range writers {
		wg.Add(1)
		go func(actor int, conn *pgx.Conn) {
			defer wg.Done()
			<-start
			for ordinal := range 20 {
				id := int64(actor*10_000 + ordinal + 1)
				if _, err := conn.Exec(ctx, `INSERT INTO reclaim_live (id, value) VALUES ($1, $2)`, id, fmt.Sprintf("writer-%d", actor)); err != nil {
					errs <- fmt.Errorf("writer %d/%d: %w", actor, ordinal, err)
					return
				}
			}
		}(actor, conn)
	}
	for actor, conn := range readers {
		wg.Add(1)
		go func(actor int, conn *pgx.Conn) {
			defer wg.Done()
			<-start
			for ordinal := range 30 {
				var count int64
				if err := conn.QueryRow(ctx, `SELECT count(*) FROM reclaim_live`).Scan(&count); err != nil {
					errs <- fmt.Errorf("reader %d/%d: %w", actor, ordinal, err)
					return
				}
				if count < 0 || count > 80 {
					errs <- fmt.Errorf("reader %d observed impossible count %d", actor, count)
					return
				}
			}
		}(actor, conn)
	}
	wg.Add(1)
	go func() {
		defer wg.Done()
		<-start
		if _, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.DeleteTable("delete", deadID))); err != nil {
			errs <- fmt.Errorf("delete: %w", err)
		}
	}()
	close(start)
	wg.Wait()
	close(errs)
	for err := range errs {
		if err != nil {
			t.Fatal(err)
		}
	}
	var liveCount int64
	if err := writers[0].QueryRow(ctx, `SELECT count(*) FROM reclaim_live`).Scan(&liveCount); err != nil || liveCount != 80 {
		t.Fatalf("live count=%d err=%v", liveCount, err)
	}

	id := catalogstore.TableReclamationID(dead.ID)
	for {
		reclamation, ok, err := catalogstore.GetReclamation(ctx, db.store, id)
		if err != nil {
			t.Fatal(err)
		}
		if ok && reclamation.State == model.ReclamationReclaimed {
			break
		}
		if err := ctx.Err(); err != nil {
			t.Fatalf("wait for reclamation: %v; state=%+v", err, reclamation)
		}
		time.Sleep(time.Millisecond)
	}
	if got := countRange(t, ctx, db.store, codec.DataPrefix(dead.ID), nil); got != 0 {
		t.Fatalf("current table data keys=%d, want 0", got)
	}
	if got := countRange(t, ctx, db.store, codec.IndexPrefix(dead.ID, deadIndex.ID), nil); got != 0 {
		t.Fatalf("current index keys=%d, want 0", got)
	}
	if got := countRange(t, ctx, oldSnapshot, codec.DataPrefix(dead.ID), nil); got != 320 {
		t.Fatalf("pre-delete snapshot data keys=%d, want 320", got)
	}
	if got := countRange(t, ctx, oldSnapshot, codec.IndexPrefix(dead.ID, deadIndex.ID), nil); got != 320 {
		t.Fatalf("pre-delete snapshot index keys=%d, want 320", got)
	}
}

func twoColumnRows(t *testing.T, count int) pirwire.Relation {
	t.Helper()
	rows := make([][]lirwire.Cell, count)
	for i := range count {
		id, err := lirwire.MakeCell(lirwire.ScalarTypeInt64, int64(i+1))
		if err != nil {
			t.Fatal(err)
		}
		value, err := lirwire.MakeCell(lirwire.ScalarTypeText, fmt.Sprintf("dead-%d", i%13))
		if err != nil {
			t.Fatal(err)
		}
		rows[i] = []lirwire.Cell{id, value}
	}
	query := lirwire.Query{
		Nodes: map[string]lirwire.Node{"rows": lirwire.Rows("r", []lirwire.RowsColumn{
			{Name: "id", Type: lirwire.ScalarTypeInt64},
			{Name: "value", Type: lirwire.ScalarTypeText},
		}, rows)},
		Root: lirwire.Root{Node: "rows", Cardinality: "many"},
	}
	raw, err := json.Marshal(query)
	if err != nil {
		t.Fatal(err)
	}
	return pirwire.Relation(raw)
}

func countRange(t *testing.T, ctx context.Context, view kv.KV, start, end []byte) int {
	t.Helper()
	if end == nil {
		end = keyenc.PrefixEnd(start)
	}
	it, err := view.Scan(ctx, start, end)
	if err != nil {
		t.Fatal(err)
	}
	defer it.Close()
	count := 0
	for it.Next() {
		count++
	}
	if err := it.Err(); err != nil {
		t.Fatal(err)
	}
	return count
}
