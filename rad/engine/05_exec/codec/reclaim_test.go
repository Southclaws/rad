package codec

import (
	"bytes"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func TestRemoveColumnPreservesSparseRowFraming(t *testing.T) {
	table := model.Table{Name: "events", Columns: []model.Column{
		{ID: "c1", Name: "first", Type: model.TypeText},
		{ID: "c5", Name: "middle", Type: model.TypeInt64, Nullable: true},
		{ID: "c99", Name: "last", Type: model.TypeText},
	}}
	for _, test := range []struct {
		name   string
		row    lir.Row
		remove string
	}{
		{name: "first", row: lir.Row{"first": lir.Text("a"), "middle": lir.Int64(7), "last": lir.Text("z")}, remove: "c1"},
		{name: "middle value", row: lir.Row{"first": lir.Text("a"), "middle": lir.Int64(7), "last": lir.Text("z")}, remove: "c5"},
		{name: "middle null", row: lir.Row{"first": lir.Text("a"), "middle": lir.Null(model.TypeInt64), "last": lir.Text("z")}, remove: "c5"},
		{name: "last", row: lir.Row{"first": lir.Text("a"), "middle": lir.Int64(7), "last": lir.Text("z")}, remove: "c99"},
	} {
		t.Run(test.name, func(t *testing.T) {
			raw, err := MarshalRow(table, test.row)
			if err != nil {
				t.Fatal(err)
			}
			cleaned, changed, err := RemoveColumn(raw, test.remove)
			if err != nil || !changed {
				t.Fatalf("changed=%v err=%v", changed, err)
			}
			narrow := table
			narrow.Columns = nil
			for _, column := range table.Columns {
				if column.ID != test.remove {
					narrow.Columns = append(narrow.Columns, column)
				}
			}
			got, err := UnmarshalRow(narrow, cleaned)
			if err != nil {
				t.Fatal(err)
			}
			for _, column := range narrow.Columns {
				if !got[column.Name].Equal(test.row[column.Name]) {
					t.Fatalf("%s = %v, want %v", column.Name, got[column.Name], test.row[column.Name])
				}
			}
		})
	}
}

func TestRemoveColumnAbsentIsByteStable(t *testing.T) {
	table := model.Table{Name: "events", Columns: []model.Column{{ID: "c1", Name: "value", Type: model.TypeText}}}
	raw, err := MarshalRow(table, lir.Row{"value": lir.Text("unchanged")})
	if err != nil {
		t.Fatal(err)
	}
	got, changed, err := RemoveColumn(raw, "c2")
	if err != nil || changed || !bytes.Equal(got, raw) {
		t.Fatalf("got=%x changed=%v err=%v", got, changed, err)
	}
}

func TestRemoveColumnRejectsMalformedInput(t *testing.T) {
	for _, test := range []struct {
		name, column string
		raw          []byte
	}{
		{name: "bad physical id", column: "wat", raw: []byte{'R', 0}},
		{name: "zero physical id", column: "c0", raw: []byte{'R', 0}},
		{name: "bad canary", column: "c1", raw: []byte{0, 0}},
		{name: "truncated count", column: "c1", raw: []byte{'R', 0x80}},
		{name: "zero delta", column: "c1", raw: []byte{'R', 1, 0}},
		{name: "truncated payload", column: "c1", raw: []byte{'R', 1, 2, 5, 'x'}},
		{name: "trailing bytes", column: "c1", raw: []byte{'R', 0, 1}},
	} {
		t.Run(test.name, func(t *testing.T) {
			if _, _, err := RemoveColumn(test.raw, test.column); err == nil {
				t.Fatal("malformed row was accepted")
			}
		})
	}
}
