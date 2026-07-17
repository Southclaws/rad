package exec

import (
	"encoding/json"
	"fmt"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
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
func MarshalRow(tbl catalog.Table, row lir.Row) ([]byte, error) {
	byID := make(map[string]lir.Value, len(row))
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
func UnmarshalRow(tbl catalog.Table, raw []byte) (lir.Row, error) {
	var byID map[string]lir.Value
	if err := json.Unmarshal(raw, &byID); err != nil {
		return nil, err
	}
	row := make(lir.Row, len(tbl.Columns))
	for _, col := range tbl.Columns {
		if v, ok := byID[col.ID]; ok {
			row[col.Name] = v
			continue
		}
		if col.Default != nil && col.Default.Func == "" {
			v, err := defaultValue(col)
			if err != nil {
				return nil, err
			}
			row[col.Name] = v
		} else {
			row[col.Name] = lir.Null(col.Type)
		}
	}
	return row, nil
}
