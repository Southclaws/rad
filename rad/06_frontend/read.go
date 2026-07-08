package frontend

import (
	"context"
	"encoding/json"

	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
	exec "rad/rad/05_exec"
)

// Record is one nested result row (re-exported so generated clients only
// import the frontend).
type Record = exec.Record

// Read executes a shaped read and returns nested records.
func (db *DB) Read(ctx context.Context, r lir.Read) ([]*exec.Record, error) {
	return db.eng.Read(ctx, r)
}

// Read inside a transaction runs against its snapshot plus its own writes.
func (tx *Tx) Read(ctx context.Context, r lir.Read) ([]*exec.Record, error) {
	return tx.tx.Read(ctx, r)
}

// ReadJSON executes a shaped read and returns plain JSON-ready values:
// scalars become string/int64/float64/bool/nil, parent includes become
// objects (or null), children includes become arrays. This is the
// database's result shape — nested, never a flattened join.
func (db *DB) ReadJSON(ctx context.Context, r lir.Read) ([]map[string]any, error) {
	recs, err := db.eng.Read(ctx, r)
	if err != nil {
		return nil, err
	}
	return RecordsJSON(recs), nil
}

// RecordsJSON converts records to JSON-ready maps.
func RecordsJSON(recs []*exec.Record) []map[string]any {
	out := make([]map[string]any, len(recs))
	for i, r := range recs {
		out[i] = recordJSON(r)
	}
	return out
}

func recordJSON(rec *exec.Record) map[string]any {
	m := make(map[string]any, len(rec.Columns)+len(rec.Parents)+len(rec.Children))
	for name, v := range rec.Columns {
		m[name] = plainValue(v)
	}
	for as, parent := range rec.Parents {
		if parent == nil {
			m[as] = nil
			continue
		}
		m[as] = recordJSON(parent)
	}
	for as, kids := range rec.Children {
		arr := make([]map[string]any, len(kids))
		for i, k := range kids {
			arr[i] = recordJSON(k)
		}
		m[as] = arr
	}
	for as, scalars := range rec.Scalars {
		obj := make(map[string]any, len(scalars))
		for name, v := range scalars {
			obj[name] = plainValue(v)
		}
		m[as] = obj
	}
	return m
}

func plainValue(v lir.Value) any {
	if v.Null {
		return nil
	}
	switch v.Type {
	case catalog.TypeText:
		return v.Text
	case catalog.TypeInt64:
		return v.Int64
	case catalog.TypeFloat64:
		return v.Float64
	case catalog.TypeBool:
		return v.Bool
	}
	return nil
}

// MarshalRecords renders records as indented JSON (object keys sorted, so
// output is deterministic).
func MarshalRecords(recs []*exec.Record) ([]byte, error) {
	return json.MarshalIndent(RecordsJSON(recs), "", "  ")
}
