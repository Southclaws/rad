package concurrent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"slices"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	kv "github.com/Southclaws/rad/rad/engine/01_kv"
	"github.com/Southclaws/rad/rad/engine/02_catalog/change"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

type historyKind string

const (
	historyRenameTable       historyKind = "rename_table"
	historyAddColumn         historyKind = "add_column"
	historyDeleteColumn      historyKind = "delete_column"
	historyPutRow            historyKind = "put_row"
	historyDeleteRow         historyKind = "delete_row"
	historyStartBuild        historyKind = "start_build"
	historyPublish           historyKind = "publish_ready"
	historyStartReplace      historyKind = "start_replacement"
	historyPublishReplace    historyKind = "publish_replacement"
	historyStartConstraint   historyKind = "start_constraint"
	historyPublishConstraint historyKind = "publish_constraint"
	historyObserve           historyKind = "observe"
)

type historyOperation struct {
	ID      string       `json:"id"`
	Kind    historyKind  `json:"kind"`
	Invoke  uint64       `json:"invoke"`
	Return  uint64       `json:"return"`
	Success bool         `json:"success"`
	Name    string       `json:"name,omitempty"`
	Column  string       `json:"column,omitempty"`
	RowID   int64        `json:"row_id,omitempty"`
	Value   string       `json:"value,omitempty"`
	Observe *oracleState `json:"observe,omitempty"`
}

// oracleState is intentionally logical: stable identities, catalog names,
// row contents, and lifecycle visibility. It does not model physical keys or
// Slate versions, which are checked by the quiescent invariant suite.
type oracleState struct {
	TableName          string           `json:"table_name"`
	Columns            map[string]bool  `json:"columns"`
	Rows               map[int64]string `json:"rows"`
	BuildStarted       bool             `json:"build_started"`
	IndexReady         bool             `json:"index_ready"`
	ValueType          string           `json:"value_type,omitempty"`
	ValueNullable      bool             `json:"value_nullable,omitempty"`
	ReplacementStarted bool             `json:"replacement_started,omitempty"`
	ReplacementReady   bool             `json:"replacement_ready,omitempty"`
	ConstraintStarted  bool             `json:"constraint_started,omitempty"`
	ConstraintReady    bool             `json:"constraint_ready,omitempty"`
}

type serialHistoryArtifact struct {
	Initial oracleState        `json:"initial"`
	History []historyOperation `json:"history"`
	Final   oracleState        `json:"final"`
}

func TestBoundedSerialHistoryOracleAgainstConcurrentEngineHistory(t *testing.T) {
	db := newChaosDB(t)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	const tableID = model.SchemaID(51_000)
	if _, err := db.Harness.Catalog().CreateTable(ctx, model.TableDef{
		ID: tableID, Name: "oracle_items",
		Columns: []model.ColumnDef{
			{ID: 1, Name: "id", Type: model.TypeInt64},
			{ID: 2, Name: "name", Type: model.TypeText},
		},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}

	initial := oracleState{
		TableName: "oracle_items",
		Columns:   map[string]bool{"id": true, "name": true},
		Rows:      map[int64]string{},
	}
	var clock atomic.Uint64
	var historyMu sync.Mutex
	var history []historyOperation
	record := func(op historyOperation, run func() error) {
		op.Invoke = clock.Add(1)
		err := run()
		op.Return = clock.Add(1)
		op.Success = err == nil
		historyMu.Lock()
		history = append(history, op)
		historyMu.Unlock()
		if err != nil {
			t.Errorf("%s: %v", op.ID, err)
		}
	}

	start := make(chan struct{})
	var transitionMu sync.Mutex
	var transitionID string
	var actors sync.WaitGroup
	actors.Add(4)
	go func() {
		defer actors.Done()
		<-start
		record(historyOperation{ID: "write-1", Kind: historyPutRow, RowID: 1, Value: "one"}, func() error {
			return retryKVConflict(ctx, func() error {
				return db.Harness.Txn(ctx, func(tx *exec.Tx) error {
					_, tables, err := tx.CatalogSnapshot(ctx)
					if err != nil {
						return err
					}
					for _, table := range tables {
						if table.SchemaID == tableID {
							return tx.Insert(ctx, table.Name, lir.Row{"id": lir.Int64(1), "name": lir.Text("one")})
						}
					}
					return fmt.Errorf("stable table %d disappeared", tableID)
				})
			})
		})
	}()
	go func() {
		defer actors.Done()
		<-start
		record(historyOperation{ID: "rename", Kind: historyRenameTable, Name: "oracle_items_renamed"}, func() error {
			return retryKVConflict(ctx, func() error {
				return db.Harness.CatalogTxn(ctx, func(_ *exec.Tx, mutation *change.Mutation) error {
					return mutation.RenameTableBySchemaID(ctx, tableID, "oracle_items_renamed")
				})
			})
		})
	}()
	go func() {
		defer actors.Done()
		<-start
		record(historyOperation{ID: "add-scratch", Kind: historyAddColumn, Column: "scratch"}, func() error {
			return retryKVConflict(ctx, func() error {
				return db.Harness.CatalogTxn(ctx, func(_ *exec.Tx, mutation *change.Mutation) error {
					_, err := mutation.CreateColumnBySchemaID(ctx, tableID, model.ColumnDef{
						ID: 3, Name: "scratch", Type: model.TypeText, Nullable: true,
					})
					return err
				})
			})
		})
	}()
	go func() {
		defer actors.Done()
		<-start
		record(historyOperation{ID: "start-index", Kind: historyStartBuild}, func() error {
			return retryKVConflict(ctx, func() error {
				started, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.StartIndexBuild(
					"build",
					pirwire.SchemaID(tableID),
					pirwire.IndexDefinition{Name: "oracle_name_online", Columns: []string{"name"}},
				)))
				if err == nil && len(started.Statements) == 1 && started.Statements[0].Control != nil {
					transitionMu.Lock()
					transitionID = started.Statements[0].Control.TransitionID
					transitionMu.Unlock()
				} else if err == nil {
					err = errors.New("start index returned no transition control")
				}
				return err
			})
		})
	}()
	close(start)
	actors.Wait()
	if t.Failed() {
		return
	}

	transitionMu.Lock()
	startedTransitionID := transitionID
	transitionMu.Unlock()
	record(historyOperation{ID: "publish-index", Kind: historyPublish}, func() error {
		_, err := waitSchemaTransitionReady(ctx, db.Control, startedTransitionID)
		return err
	})
	record(historyOperation{ID: "delete-scratch", Kind: historyDeleteColumn, Column: "scratch"}, func() error {
		return retryKVConflict(ctx, func() error {
			return db.Harness.CatalogTxn(ctx, func(_ *exec.Tx, mutation *change.Mutation) error {
				_, err := mutation.DeleteColumnBySchemaID(ctx, tableID, 3)
				return err
			})
		})
	})

	final, err := observeOracleState(ctx, db.Harness, "oracle_items_renamed", "oracle_name_online")
	if err != nil {
		t.Fatal(err)
	}
	observation := historyOperation{
		ID: "final-observation", Kind: historyObserve, Success: true,
		Invoke: clock.Add(1), Observe: &final,
	}
	observation.Return = clock.Add(1)
	history = append(history, observation)

	artifact := serialHistoryArtifact{Initial: initial, History: history, Final: final}
	raw, err := json.Marshal(artifact)
	if err != nil {
		t.Fatal(err)
	}
	var decoded serialHistoryArtifact
	if err := json.Unmarshal(raw, &decoded); err != nil {
		t.Fatal(err)
	}
	order, ok := boundedSerialOrder(decoded.Initial, decoded.History, decoded.Final)
	if !ok {
		t.Fatalf("no legal serial history for artifact: %s", raw)
	}
	if len(order) != len(history) {
		t.Fatalf("serial order has %d operations, want %d: %v", len(order), len(history), order)
	}
}

func TestBoundedSerialHistoryOracleRejectsImpossibleOutcomes(t *testing.T) {
	initial := oracleState{
		TableName: "items", Columns: map[string]bool{"id": true, "name": true}, Rows: map[int64]string{},
	}
	history := []historyOperation{
		{ID: "write", Kind: historyPutRow, Invoke: 1, Return: 6, Success: true, RowID: 1, Value: "one"},
		{ID: "rename", Kind: historyRenameTable, Invoke: 2, Return: 3, Success: true, Name: "renamed"},
		{ID: "start", Kind: historyStartBuild, Invoke: 4, Return: 5, Success: true},
		{ID: "publish", Kind: historyPublish, Invoke: 7, Return: 8, Success: true},
	}
	valid := oracleState{
		TableName: "renamed", Columns: map[string]bool{"id": true, "name": true},
		Rows: map[int64]string{1: "one"}, BuildStarted: true, IndexReady: true,
	}
	if _, ok := boundedSerialOrder(initial, history, valid); !ok {
		t.Fatal("oracle rejected a legal overlapping history")
	}

	cases := map[string]oracleState{
		"lost committed write": {
			TableName: "renamed", Columns: maps.Clone(valid.Columns), Rows: map[int64]string{},
			BuildStarted: true, IndexReady: true,
		},
		"partial ready publication": {
			TableName: "renamed", Columns: maps.Clone(valid.Columns), Rows: maps.Clone(valid.Rows),
			BuildStarted: false, IndexReady: true,
		},
		"stale catalog name": {
			TableName: "items", Columns: maps.Clone(valid.Columns), Rows: maps.Clone(valid.Rows),
			BuildStarted: true, IndexReady: true,
		},
	}
	for name, final := range cases {
		t.Run(name, func(t *testing.T) {
			if order, ok := boundedSerialOrder(initial, history, final); ok {
				t.Fatalf("oracle accepted impossible outcome with order %v", order)
			}
		})
	}
}

func TestBoundedSerialHistoryOracleCoversSlice8Transitions(t *testing.T) {
	db := newChaosDB(t, exec.WithSchemaJobConfig(exec.SchemaJobConfig{
		IndexBatchSize: 1, BatchesBeforeYield: 1, YieldInterval: time.Millisecond,
	}))
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	const tableID = model.SchemaID(52_000)
	table, err := db.Harness.Catalog().CreateTable(ctx, model.TableDef{
		ID: tableID, Name: "slice8_oracle_items",
		Columns: []model.ColumnDef{
			{ID: 1, Name: "id", Type: model.TypeInt64},
			{ID: 2, Name: "name", Type: model.TypeInt64, Nullable: true},
		},
		PrimaryKey: []string{"id"},
	})
	if err != nil {
		t.Fatal(err)
	}
	nameColumn, ok := table.Column("name")
	if !ok {
		t.Fatal("slice 8 oracle column is missing")
	}
	initial := oracleState{
		TableName: "slice8_oracle_items",
		Columns:   map[string]bool{"id": true, "name": true},
		Rows:      map[int64]string{},
		ValueType: string(model.TypeInt64), ValueNullable: true,
	}
	var clock atomic.Uint64
	var historyMu sync.Mutex
	var history []historyOperation
	record := func(operation historyOperation, run func() error, expectSuccess bool) {
		operation.Invoke = clock.Add(1)
		err := run()
		operation.Return = clock.Add(1)
		operation.Success = err == nil
		historyMu.Lock()
		history = append(history, operation)
		historyMu.Unlock()
		if operation.Success != expectSuccess {
			t.Errorf("%s success=%v err=%v, want success=%v", operation.ID, operation.Success, err, expectSuccess)
		}
	}

	start := make(chan struct{})
	var transitionMu sync.Mutex
	var replacementID string
	var actors sync.WaitGroup
	actors.Add(2)
	go func() {
		defer actors.Done()
		<-start
		record(
			historyOperation{ID: "write-int", Kind: historyPutRow, RowID: 1, Value: "1"},
			func() error {
				return retryKVConflict(ctx, func() error {
					return db.Harness.Insert(ctx, table.Name, lir.Row{
						"id": lir.Int64(1), "name": lir.Int64(1),
					})
				})
			},
			true,
		)
	}()
	go func() {
		defer actors.Done()
		<-start
		record(
			historyOperation{ID: "start-replacement", Kind: historyStartReplace},
			func() error {
				return retryKVConflict(ctx, func() error {
					started, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.StartColumnReplacement(
						"replace",
						pirwire.SchemaID(table.SchemaID),
						pirwire.SchemaID(nameColumn.SchemaID),
						pirwire.ColumnReplacementDefinition{Type: pirwire.ColumnTypeText, Nullable: true},
					)))
					if err == nil && len(started.Statements) == 1 && started.Statements[0].Control != nil {
						transitionMu.Lock()
						replacementID = started.Statements[0].Control.TransitionID
						transitionMu.Unlock()
					} else if err == nil {
						err = errors.New("start replacement returned no transition control")
					}
					return err
				})
			},
			true,
		)
	}()
	close(start)
	actors.Wait()
	if t.Failed() {
		return
	}
	transitionMu.Lock()
	startedReplacementID := replacementID
	transitionMu.Unlock()
	record(
		historyOperation{
			ID: "publish-replacement", Kind: historyPublishReplace,
			Value: string(model.TypeText),
		},
		func() error {
			_, err := waitSchemaTransitionReady(ctx, db.Control, startedReplacementID)
			return err
		},
		true,
	)

	published, ok, err := db.Harness.Catalog().GetTable(ctx, table.Name)
	if err != nil || !ok {
		t.Fatalf("get replaced table: found=%v err=%v", ok, err)
	}
	nameColumn, ok = published.Column("name")
	if !ok {
		t.Fatal("published name column is missing")
	}
	var constraintID string
	record(
		historyOperation{ID: "start-constraint", Kind: historyStartConstraint},
		func() error {
			started, err := db.Control.Execute(ctx, pirwire.Prog("", pirwire.StartConstraintValidation(
				"validate",
				pirwire.SchemaID(published.SchemaID),
				pirwire.ConstraintValidationDefinition{
					Name: "slice8_name_required",
					Kind: "not_null", ColumnID: pirwire.SchemaID(nameColumn.SchemaID),
				},
			)))
			if err == nil && len(started.Statements) == 1 && started.Statements[0].Control != nil {
				constraintID = started.Statements[0].Control.TransitionID
			} else if err == nil {
				err = errors.New("start constraint returned no transition control")
			}
			return err
		},
		true,
	)
	record(
		historyOperation{ID: "write-null", Kind: historyPutRow, RowID: 2, Value: "<null>"},
		func() error {
			return db.Harness.Insert(ctx, table.Name, lir.Row{"id": lir.Int64(2)})
		},
		false,
	)
	record(
		historyOperation{ID: "write-text", Kind: historyPutRow, RowID: 2, Value: "2"},
		func() error {
			return db.Harness.Insert(ctx, table.Name, lir.Row{
				"id": lir.Int64(2), "name": lir.Text("2"),
			})
		},
		true,
	)
	record(
		historyOperation{ID: "publish-constraint", Kind: historyPublishConstraint},
		func() error {
			_, err := waitSchemaTransitionReady(ctx, db.Control, constraintID)
			return err
		},
		true,
	)
	if t.Failed() {
		return
	}

	final, err := observeOracleState(ctx, db.Harness, table.Name, "")
	if err != nil {
		t.Fatal(err)
	}
	final.ValueType = string(model.TypeText)
	final.ValueNullable = false
	final.ReplacementStarted = true
	final.ReplacementReady = true
	final.ConstraintStarted = true
	final.ConstraintReady = true
	observation := historyOperation{
		ID: "slice8-observation", Kind: historyObserve, Success: true,
		Invoke: clock.Add(1), Observe: &final,
	}
	observation.Return = clock.Add(1)
	history = append(history, observation)

	if order, ok := boundedSerialOrder(initial, history, final); !ok {
		raw, _ := json.Marshal(serialHistoryArtifact{
			Initial: initial, History: history, Final: final,
		})
		t.Fatalf("no legal Slice 8 serial history: %s", raw)
	} else if len(order) != len(history) {
		t.Fatalf("Slice 8 serial order has %d operations, want %d: %v", len(order), len(history), order)
	}

	impossible := cloneOracleState(final)
	impossible.ReplacementStarted = false
	if order, ok := boundedSerialOrder(initial, history, impossible); ok {
		t.Fatalf("oracle accepted ready replacement without its start: %v", order)
	}
	impossible = cloneOracleState(final)
	impossible.ConstraintReady = false
	impossible.ValueNullable = false
	if order, ok := boundedSerialOrder(initial, history, impossible); ok {
		t.Fatalf("oracle accepted canonical not-null publication without a valid constraint: %v", order)
	}
	impossibleHistory := slices.Clone(history)
	for index := range impossibleHistory {
		if impossibleHistory[index].ID == "write-null" {
			impossibleHistory[index].Success = true
		}
	}
	if order, ok := boundedSerialOrder(initial, impossibleHistory, final); ok {
		t.Fatalf("oracle accepted a NULL write after foreground enforcement: %v", order)
	}
}

func retryKVConflict(ctx context.Context, fn func() error) error {
	_, err := retryAttempts(ctx, 1_000, 0, func(err error) bool {
		return errors.Is(err, kv.ErrConflict)
	}, nil, func(int) (struct{}, error) {
		return struct{}{}, fn()
	})
	return err
}

func observeOracleState(ctx context.Context, engine *exec.Engine, tableName, indexName string) (oracleState, error) {
	table, ok, err := engine.Catalog().GetTable(ctx, tableName)
	if err != nil || !ok {
		return oracleState{}, fmt.Errorf("observe table %q: found=%v err=%v", tableName, ok, err)
	}
	state := oracleState{TableName: table.Name, Columns: map[string]bool{}, Rows: map[int64]string{}}
	for _, column := range table.Columns {
		state.Columns[column.Name] = true
	}
	index, ok := table.Index(indexName)
	state.BuildStarted = ok
	state.IndexReady = ok && index.Ready()
	it, err := engine.ScanTable(ctx, tableName)
	if err != nil {
		return oracleState{}, err
	}
	defer it.Close()
	for {
		row, ok, err := it.Next()
		if err != nil {
			return oracleState{}, err
		}
		if !ok {
			break
		}
		state.Rows[row["id"].Int64] = row["name"].Text
	}
	return state, nil
}

func boundedSerialOrder(initial oracleState, history []historyOperation, final oracleState) ([]string, bool) {
	if len(history) > 63 {
		return nil, false
	}
	predecessors := make([]uint64, len(history))
	for i := range history {
		for j := range history {
			if i != j && history[j].Return < history[i].Invoke {
				predecessors[i] |= uint64(1) << j
			}
		}
	}
	all := uint64(1)<<len(history) - 1
	seen := make(map[string]bool)
	var search func(oracleState, uint64, []string) ([]string, bool)
	search = func(state oracleState, placed uint64, order []string) ([]string, bool) {
		if placed == all {
			return order, oracleStatesEqual(state, final)
		}
		memo := oracleMemoKey(state, placed)
		if seen[memo] {
			return nil, false
		}
		seen[memo] = true
		for i, operation := range history {
			bit := uint64(1) << i
			if placed&bit != 0 || predecessors[i]&^placed != 0 {
				continue
			}
			next, ok := applyOracleOperation(state, operation)
			if !ok {
				continue
			}
			if result, ok := search(next, placed|bit, append(slices.Clone(order), operation.ID)); ok {
				return result, true
			}
		}
		return nil, false
	}
	return search(cloneOracleState(initial), 0, nil)
}

func applyOracleOperation(state oracleState, operation historyOperation) (oracleState, bool) {
	next := cloneOracleState(state)
	if !operation.Success {
		return next, true
	}
	switch operation.Kind {
	case historyRenameTable:
		if operation.Name == "" {
			return oracleState{}, false
		}
		next.TableName = operation.Name
	case historyAddColumn:
		if operation.Column == "" || next.Columns[operation.Column] {
			return oracleState{}, false
		}
		next.Columns[operation.Column] = true
	case historyDeleteColumn:
		if !next.Columns[operation.Column] {
			return oracleState{}, false
		}
		delete(next.Columns, operation.Column)
	case historyPutRow:
		if operation.Value == "<null>" &&
			(next.ConstraintStarted || !next.ValueNullable) {
			return oracleState{}, false
		}
		next.Rows[operation.RowID] = operation.Value
	case historyDeleteRow:
		delete(next.Rows, operation.RowID)
	case historyStartBuild:
		if next.BuildStarted || next.IndexReady {
			return oracleState{}, false
		}
		next.BuildStarted = true
	case historyPublish:
		if !next.BuildStarted || next.IndexReady {
			return oracleState{}, false
		}
		next.IndexReady = true
	case historyStartReplace:
		if next.ReplacementStarted || next.ReplacementReady {
			return oracleState{}, false
		}
		next.ReplacementStarted = true
	case historyPublishReplace:
		if !next.ReplacementStarted || next.ReplacementReady || operation.Value == "" {
			return oracleState{}, false
		}
		next.ReplacementReady = true
		next.ValueType = operation.Value
	case historyStartConstraint:
		if next.ConstraintStarted || next.ConstraintReady || !next.ValueNullable {
			return oracleState{}, false
		}
		next.ConstraintStarted = true
	case historyPublishConstraint:
		if !next.ConstraintStarted || next.ConstraintReady {
			return oracleState{}, false
		}
		next.ConstraintReady = true
		next.ValueNullable = false
	case historyObserve:
		if operation.Observe == nil || !oracleStatesEqual(next, *operation.Observe) {
			return oracleState{}, false
		}
	default:
		return oracleState{}, false
	}
	return next, true
}

func cloneOracleState(state oracleState) oracleState {
	state.Columns = maps.Clone(state.Columns)
	state.Rows = maps.Clone(state.Rows)
	return state
}

func oracleStatesEqual(a, b oracleState) bool {
	return a.TableName == b.TableName && a.BuildStarted == b.BuildStarted && a.IndexReady == b.IndexReady &&
		a.ValueType == b.ValueType && a.ValueNullable == b.ValueNullable &&
		a.ReplacementStarted == b.ReplacementStarted && a.ReplacementReady == b.ReplacementReady &&
		a.ConstraintStarted == b.ConstraintStarted && a.ConstraintReady == b.ConstraintReady &&
		maps.Equal(a.Columns, b.Columns) && maps.Equal(a.Rows, b.Rows)
}

func oracleMemoKey(state oracleState, placed uint64) string {
	raw, _ := json.Marshal(struct {
		State  oracleState `json:"state"`
		Placed uint64      `json:"placed"`
	}{State: state, Placed: placed})
	return string(raw)
}
