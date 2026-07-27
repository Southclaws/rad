package concurrent

import (
	"context"
	"encoding/json"
	"fmt"
	"slices"
	"strings"
	"time"

	radclient "github.com/Southclaws/rad/rad/client"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/protocol"
	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

func allItemsQuery() lirwire.Query {
	return lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"scan":  lirwire.Scan("items", "i"),
			"order": lirwire.Order("scan", []lirwire.OrderTerm{{Expr: lirwire.Col("i", "id")}}),
		},
		Root: lirwire.Root{Node: "order", Cardinality: "many"},
	}
}

func validateRecord(record protocol.Record) error {
	id, err := int64Field(record, "id")
	if err != nil {
		return err
	}
	generation, err := int64Field(record, "generation")
	if err != nil {
		return err
	}
	value, ok := record["value"].(string)
	if !ok {
		return fmt.Errorf("value is %T, want string", record["value"])
	}
	if want := rowValue(id, generation); value != want {
		return fmt.Errorf("torn row id=%d has value=%q generation=%d, want value=%q", id, value, generation, want)
	}
	bucket, ok := record["bucket"].(string)
	if !ok {
		return fmt.Errorf("bucket is %T, want string", record["bucket"])
	}
	if want := makeExpectedRow(id, generation).Bucket; bucket != want {
		return fmt.Errorf("torn row id=%d has bucket=%q generation=%d, want bucket=%q", id, bucket, generation, want)
	}
	return nil
}

func int64Field(record protocol.Record, name string) (int64, error) {
	value, ok := record[name]
	if !ok {
		return 0, fmt.Errorf("missing %q", name)
	}
	switch value := value.(type) {
	case json.Number:
		result, err := value.Int64()
		if err != nil {
			return 0, fmt.Errorf("%s: %w", name, err)
		}
		return result, nil
	case int64:
		return value, nil
	default:
		return 0, fmt.Errorf("%s is %T, want integer", name, value)
	}
}

func normalizeIndexRows(rows []lir.Row) (map[int64]expectedRow, error) {
	result := make(map[int64]expectedRow, len(rows))
	for _, row := range rows {
		id, idOK := row["id"]
		value, valueOK := row["value"]
		generation, generationOK := row["generation"]
		bucket, bucketOK := row["bucket"]
		if !idOK || !valueOK || !generationOK || !bucketOK || id.Null || value.Null || generation.Null || bucket.Null {
			return nil, fmt.Errorf("malformed index row: %v", row)
		}
		if _, duplicate := result[id.Int64]; duplicate {
			return nil, fmt.Errorf("index returned duplicate primary key %d", id.Int64)
		}
		result[id.Int64] = expectedRow{
			ID: id.Int64, Value: value.Text, Generation: generation.Int64, Bucket: bucket.Text,
		}
	}
	return result, nil
}

func planText(plan any) (string, error) {
	root, ok := plan.(map[string]any)
	if !ok {
		return "", fmt.Errorf("plan is %T", plan)
	}
	statements, ok := root["statements"].([]any)
	if !ok || len(statements) != 1 {
		return "", fmt.Errorf("plan has unexpected statements: %v", root)
	}
	statement, ok := statements[0].(map[string]any)
	if !ok {
		return "", fmt.Errorf("plan statement is %T", statements[0])
	}
	text, ok := statement["text"].(string)
	if !ok {
		return "", fmt.Errorf("plan text is %T", statement["text"])
	}
	return text, nil
}

func equalityRelation(row expectedRow) (pirwire.Relation, error) {
	query := lirwire.Query{
		Nodes: map[string]lirwire.Node{
			"scan": lirwire.Scan("items", "i"),
			"match": lirwire.Filter("scan", lirwire.Binary(
				"eq", lirwire.Col("i", "bucket"), lirwire.Lit(lirwire.Text(row.Bucket)),
			)),
			"order": lirwire.Order("match", []lirwire.OrderTerm{{Expr: lirwire.Col("i", "id")}}),
		},
		Root: lirwire.Root{Node: "order", Cardinality: "many"},
	}
	raw, err := json.Marshal(query)
	return pirwire.Relation(raw), err
}

func (w *workload) awaitScheduler(ctx context.Context) error {
	for {
		transition, err := w.db.Control.SchemaTransition(ctx, w.transitionID)
		if err != nil {
			return err
		}
		if transition.State == radclient.TransitionReady {
			return nil
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(time.Millisecond):
		}
	}
}

func (w *workload) assertFinal(ctx context.Context) error {
	records, err := w.db.Control.Query(ctx, allItemsQuery())
	if err != nil {
		return fmt.Errorf("final HTTP scan: %w", err)
	}
	actual := make(map[int64]expectedRow, len(records))
	for _, record := range records {
		if err := validateRecord(record); err != nil {
			return err
		}
		id, _ := int64Field(record, "id")
		generation, _ := int64Field(record, "generation")
		actual[id] = expectedRow{
			ID: id, Value: record["value"].(string), Generation: generation, Bucket: record["bucket"].(string),
		}
	}
	if diff := compareRows(w.expected, actual); diff != "" {
		return fmt.Errorf("final table diverged from successful actor model:\n%s", diff)
	}

	indexRows, err := w.db.Auditor.ScanIndex(ctx, "items", indexName, nil)
	if err != nil {
		return fmt.Errorf("scan ready index: %w", err)
	}
	indexed, err := normalizeIndexRows(indexRows)
	if err != nil {
		return err
	}
	if diff := compareRows(actual, indexed); diff != "" {
		return fmt.Errorf("online index differs from base table:\n%s", diff)
	}

	ids := make([]int64, 0, len(w.expected))
	for id := range w.expected {
		ids = append(ids, id)
	}
	slices.Sort(ids)
	relation, err := equalityRelation(w.expected[ids[0]])
	if err != nil {
		return err
	}
	planned, err := w.db.Control.Execute(ctx, pirwire.Prog("", pirwire.Query("lookup", relation)), radclient.WithPlan())
	if err != nil {
		return fmt.Errorf("planned indexed lookup: %w", err)
	}
	text, err := planText(planned.Plan)
	if err != nil {
		return err
	}
	if !strings.Contains(text, "IndexRangeScan") || !strings.Contains(text, indexName) {
		return fmt.Errorf("ready online index was not planner-visible:\n%s", text)
	}

	tables, err := w.db.Control.Tables(ctx)
	if err != nil {
		return err
	}
	wantTable := fmt.Sprintf("catalog_probe_r%03d", w.scenario.Rounds-1)
	wantColumn := fmt.Sprintf("payload_r%03d", w.scenario.Rounds-1)
	for _, table := range tables {
		if table.Name != wantTable {
			continue
		}
		if len(table.Columns) != 2+w.scenario.Rounds*w.scenario.MetadataAdds {
			return fmt.Errorf("probe has %d columns, want %d", len(table.Columns), 2+w.scenario.Rounds*w.scenario.MetadataAdds)
		}
		columns := make(map[string]bool, len(table.Columns))
		for _, column := range table.Columns {
			columns[column.Name] = true
		}
		if !columns[wantColumn] {
			return fmt.Errorf("probe table lacks final renamed column %q", wantColumn)
		}
		for round := 0; round < w.scenario.Rounds; round++ {
			for actor := 0; actor < w.scenario.MetadataAdds; actor++ {
				name := fmt.Sprintf("extra_r%03d_a%02d", round, actor)
				if !columns[name] {
					return fmt.Errorf("probe table lacks concurrently added column %q", name)
				}
			}
		}
		probeRows, err := w.db.Control.Query(ctx, lirwire.Query{
			Nodes: map[string]lirwire.Node{"probe": lirwire.Scan(wantTable, "p")},
			Root:  lirwire.Root{Node: "probe", Cardinality: "exactly_one"},
		})
		if err != nil {
			return fmt.Errorf("read sparse-row probe: %w", err)
		}
		if len(probeRows) != 1 || probeRows[0][wantColumn] != "stable" {
			return fmt.Errorf("sparse-row probe = %#v", probeRows)
		}
		for name := range columns {
			if strings.HasPrefix(name, "extra_") && probeRows[0][name] != nil {
				return fmt.Errorf("historically missing column %q read as %#v, want NULL", name, probeRows[0][name])
			}
		}
		control, err := w.db.Control.SchemaTransition(ctx, w.transitionID)
		if err != nil {
			return fmt.Errorf("inspect final transition: %w", err)
		}
		if control.State != radclient.TransitionState(model.TransitionReady) || control.DeltaLag != 0 {
			return fmt.Errorf("final transition control = %+v", control)
		}
		return nil
	}
	return fmt.Errorf("catalog lacks final renamed table %q", wantTable)
}

func compareRows(want, got map[int64]expectedRow) string {
	var differences []string
	wantIDs := make([]int64, 0, len(want))
	for id := range want {
		wantIDs = append(wantIDs, id)
	}
	slices.Sort(wantIDs)
	for _, id := range wantIDs {
		expected := want[id]
		actual, ok := got[id]
		if !ok {
			differences = append(differences, fmt.Sprintf("missing id=%d", id))
			continue
		}
		if actual != expected {
			differences = append(differences, fmt.Sprintf("id=%d got=%+v want=%+v", id, actual, expected))
		}
	}
	gotIDs := make([]int64, 0, len(got))
	for id := range got {
		gotIDs = append(gotIDs, id)
	}
	slices.Sort(gotIDs)
	for _, id := range gotIDs {
		if _, ok := want[id]; !ok {
			differences = append(differences, fmt.Sprintf("unexpected id=%d", id))
		}
	}
	if len(differences) > 24 {
		differences = append(differences[:24], fmt.Sprintf("... %d more", len(differences)-24))
	}
	return strings.Join(differences, "\n")
}
