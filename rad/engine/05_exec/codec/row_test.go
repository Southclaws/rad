package codec

import (
	"encoding/binary"
	"math"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func col(id, name string, t model.Type, nullable bool) model.Column {
	return model.Column{ID: id, Name: name, Type: t, Nullable: nullable}
}

func table(cols ...model.Column) model.Table {
	return model.Table{Name: "t", Columns: cols}
}

func roundTrip(t *testing.T, tbl model.Table, row lir.Row) lir.Row {
	t.Helper()
	raw, err := MarshalRow(tbl, row)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	if len(raw) == 0 || raw[0] != rowCanary {
		t.Fatalf("value does not open with the canary byte")
	}
	out, err := UnmarshalRow(tbl, raw)
	if err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	return out
}

func assertValue(t *testing.T, row lir.Row, name string, want lir.Value) {
	t.Helper()
	got := row[name]
	if got != want {
		t.Fatalf("column %q: got %+v, want %+v", name, got, want)
	}
}

func TestRoundTripAllTypes(t *testing.T) {
	tbl := table(
		col("c1", "s", model.TypeText, true),
		col("c2", "i", model.TypeInt64, true),
		col("c3", "f", model.TypeFloat64, true),
		col("c4", "b", model.TypeBool, true),
	)

	cases := []struct {
		name string
		row  lir.Row
	}{
		{"text empty", lir.Row{"s": lir.Text(""), "i": lir.Int64(0), "f": lir.Float64(0), "b": lir.Bool(false)}},
		{"text ascii", lir.Row{"s": lir.Text("Radlabs"), "i": lir.Int64(1), "f": lir.Float64(1), "b": lir.Bool(true)}},
		{"text utf8", lir.Row{"s": lir.Text("café — 日本語 🦀"), "i": lir.Int64(-1), "f": lir.Float64(-0.0), "b": lir.Bool(false)}},
		{"int extremes", lir.Row{"s": lir.Text("x"), "i": lir.Int64(math.MinInt64), "f": lir.Float64(math.MaxFloat64), "b": lir.Bool(true)}},
		{"int max", lir.Row{"s": lir.Text("y"), "i": lir.Int64(math.MaxInt64), "f": lir.Float64(-math.MaxFloat64), "b": lir.Bool(false)}},
		{"float frac", lir.Row{"s": lir.Text("z"), "i": lir.Int64(42), "f": lir.Float64(3.14159265358979), "b": lir.Bool(true)}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			out := roundTrip(t, tbl, tc.row)
			for name, want := range tc.row {
				assertValue(t, out, name, want)
			}
		})
	}
}

func TestRoundTripExplicitNull(t *testing.T) {
	tbl := table(
		col("c1", "id", model.TypeText, false),
		col("c2", "note", model.TypeText, true),
	)
	out := roundTrip(t, tbl, lir.Row{"id": lir.Text("a"), "note": lir.Null(model.TypeText)})
	assertValue(t, out, "note", lir.Null(model.TypeText))
	if !out["note"].Null {
		t.Fatalf("explicit NULL did not round-trip as NULL")
	}
}

// A present explicit NULL and an absent field are distinct: an explicit NULL
// reads back NULL even when the column carries a literal default, while an
// absent field reads back that default.
func TestPresentNullVsAbsent(t *testing.T) {
	withDefault := col("c2", "status", model.TypeText, true)
	withDefault.Default = &model.Default{Text: "active"}
	tbl := table(col("c1", "id", model.TypeText, false), withDefault)

	explicitNull := roundTrip(t, tbl, lir.Row{"id": lir.Text("a"), "status": lir.Null(model.TypeText)})
	if !explicitNull["status"].Null {
		t.Fatalf("explicit NULL on a defaulted column read back %+v, want NULL", explicitNull["status"])
	}

	// Absent field: encode a row that predates the column, decode under the
	// current table.
	older := table(col("c1", "id", model.TypeText, false))
	raw, err := MarshalRow(older, lir.Row{"id": lir.Text("a")})
	if err != nil {
		t.Fatalf("marshal older row: %v", err)
	}
	absent, err := UnmarshalRow(tbl, raw)
	if err != nil {
		t.Fatalf("unmarshal under current table: %v", err)
	}
	assertValue(t, absent, "status", lir.Text("active"))
}

// An added nullable column with no default, absent from an old row, reads back
// NULL.
func TestAbsentNoDefaultIsNull(t *testing.T) {
	older := table(col("c1", "id", model.TypeText, false))
	raw, err := MarshalRow(older, lir.Row{"id": lir.Text("a")})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	tbl := table(col("c1", "id", model.TypeText, false), col("c5", "added", model.TypeInt64, true))
	out, err := UnmarshalRow(tbl, raw)
	if err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	assertValue(t, out, "added", lir.Null(model.TypeInt64))
}

// A field for a since-dropped column is skipped by its frame, and the
// surrounding fields still decode.
func TestDroppedColumnSkipped(t *testing.T) {
	wide := table(
		col("c1", "id", model.TypeText, false),
		col("c2", "gone", model.TypeText, true),
		col("c3", "keep", model.TypeInt64, true),
	)
	raw, err := MarshalRow(wide, lir.Row{
		"id":   lir.Text("a"),
		"gone": lir.Text("dropme"),
		"keep": lir.Int64(99),
	})
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	narrow := table(
		col("c1", "id", model.TypeText, false),
		col("c3", "keep", model.TypeInt64, true),
	)
	out, err := UnmarshalRow(narrow, raw)
	if err != nil {
		t.Fatalf("unmarshal after drop: %v", err)
	}
	assertValue(t, out, "id", lir.Text("a"))
	assertValue(t, out, "keep", lir.Int64(99))
	if _, ok := out["gone"]; ok {
		t.Fatalf("dropped column should not appear in the decoded row")
	}
}

func TestRejectBadCanary(t *testing.T) {
	tbl := table(col("c1", "id", model.TypeText, false))
	raw, _ := MarshalRow(tbl, lir.Row{"id": lir.Text("a")})
	raw[0] = 'X'
	if _, err := UnmarshalRow(tbl, raw); err == nil {
		t.Fatalf("expected a canary rejection")
	}
	if _, err := UnmarshalRow(tbl, nil); err == nil {
		t.Fatalf("expected empty value to be rejected")
	}
}

func TestRejectTrailingBytes(t *testing.T) {
	tbl := table(col("c1", "id", model.TypeText, false))
	raw, _ := MarshalRow(tbl, lir.Row{"id": lir.Text("a")})
	raw = append(raw, 0xFF)
	if _, err := UnmarshalRow(tbl, raw); err == nil {
		t.Fatalf("expected trailing-byte rejection")
	}
}

func TestRejectTruncated(t *testing.T) {
	tbl := table(col("c1", "s", model.TypeText, true), col("c2", "i", model.TypeInt64, true))
	raw, _ := MarshalRow(tbl, lir.Row{"s": lir.Text("hello"), "i": lir.Int64(7)})
	if _, err := UnmarshalRow(tbl, raw[:len(raw)-3]); err == nil {
		t.Fatalf("expected truncation rejection")
	}
}

func TestRejectWrongFixedLength(t *testing.T) {
	// A body whose int64 field is framed with the wrong length must be
	// rejected rather than silently misread.
	tbl := table(col("c1", "i", model.TypeInt64, true))
	buf := []byte{rowCanary}
	buf = binary.AppendUvarint(buf, 1)       // one field
	buf = binary.AppendUvarint(buf, 1<<1)    // delta 1, not null
	buf = binary.AppendUvarint(buf, 4)       // wrong length: 4, not 8
	buf = append(buf, 0, 0, 0, 0)     // 4 payload bytes
	if _, err := UnmarshalRow(tbl, buf); err == nil {
		t.Fatalf("expected wrong-length rejection")
	}
}

func TestRejectBadBool(t *testing.T) {
	tbl := table(col("c1", "b", model.TypeBool, true))
	buf := []byte{rowCanary}
	buf = binary.AppendUvarint(buf, 1)
	buf = binary.AppendUvarint(buf, 1<<1)
	buf = binary.AppendUvarint(buf, 1)
	buf = append(buf, 0x02) // not 0x00 or 0x01
	if _, err := UnmarshalRow(tbl, buf); err == nil {
		t.Fatalf("expected bad-bool rejection")
	}
}

func TestRejectDuplicateColumn(t *testing.T) {
	tbl := table(col("c1", "s", model.TypeText, true))
	buf := []byte{rowCanary}
	buf = binary.AppendUvarint(buf, 2)
	buf = binary.AppendUvarint(buf, 1<<1) // delta 1 -> id 1
	buf = binary.AppendUvarint(buf, 1)
	buf = append(buf, 'a')
	buf = binary.AppendUvarint(buf, 0<<1) // delta 0 -> duplicate/non-ascending
	buf = binary.AppendUvarint(buf, 1)
	buf = append(buf, 'b')
	if _, err := UnmarshalRow(tbl, buf); err == nil {
		t.Fatalf("expected duplicate/non-ascending rejection")
	}
}
