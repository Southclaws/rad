package exec

import (
	"encoding/json"
	"fmt"

	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
)

// Row storage format: rows persist as JSON objects keyed by *column ID*, not
// column name. Renaming a column is therefore a pure catalog operation — no
// row rewrite — and dropping a column simply orphans its field (ignored on
// read).
//
// Reading a row written before a column was added yields, for the missing
// column: its literal default if one is defined (deterministic), otherwise
// NULL. Generator defaults (uuid, now_ms) are never fabricated on read.

// MarshalRow encodes a name-keyed row for storage. Exported for tooling
// (cmd/rad); the engine handles this internally.
func MarshalRow(tbl catalog.Table, row qir.Row) ([]byte, error) {
	byID := make(map[string]qir.Value, len(row))
	for name, v := range row {
		col, ok := tbl.Column(name)
		if !ok {
			return nil, fmt.Errorf("exec: table %q has no column %q", tbl.Name, name)
		}
		byID[col.ID] = v
	}
	return json.Marshal(byID)
}

// UnmarshalRow decodes a stored row into a name-keyed row according to the
// current table definition. Unknown column IDs (dropped columns) are
// discarded; missing columns get their literal default or NULL.
func UnmarshalRow(tbl catalog.Table, raw []byte) (qir.Row, error) {
	var byID map[string]qir.Value
	if err := json.Unmarshal(raw, &byID); err != nil {
		return nil, err
	}
	row := make(qir.Row, len(tbl.Columns))
	for _, col := range tbl.Columns {
		if v, ok := byID[col.ID]; ok {
			row[col.Name] = v
			continue
		}
		row[col.Name] = missingColumnValue(col)
	}
	return row, nil
}

func missingColumnValue(col catalog.Column) qir.Value {
	if d := col.Default; d != nil && d.Func == "" {
		v, err := defaultValue(col)
		if err == nil {
			return v
		}
	}
	return qir.Null(col.Type)
}
