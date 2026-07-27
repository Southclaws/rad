package codec

import (
	"math"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func TestPhysicalReplacementCellsCoexistAndDecodeUnderOwnTypes(t *testing.T) {
	source := model.Column{ID: "c2", SchemaID: 7, Name: "value", Type: model.TypeText, Nullable: true}
	target := model.Column{ID: "c9", SchemaID: 7, Name: "value", Type: model.TypeInt64, Nullable: true}
	tbl := table(col("c1", "id", model.TypeInt64, false), source)
	raw, err := MarshalRow(tbl, lir.Row{"id": lir.Int64(1), "value": lir.Text("42")})
	if err != nil {
		t.Fatal(err)
	}
	raw, err = SetColumnValue(raw, target, lir.Int64(42))
	if err != nil {
		t.Fatal(err)
	}
	oldValue, err := ReadColumnValue(raw, source)
	if err != nil || !oldValue.Equal(lir.Text("42")) {
		t.Fatalf("source representation = %+v err=%v", oldValue, err)
	}
	newValue, err := ReadColumnValue(raw, target)
	if err != nil || !newValue.Equal(lir.Int64(42)) {
		t.Fatalf("target representation = %+v err=%v", newValue, err)
	}

	newTable := table(col("c1", "id", model.TypeInt64, false), target)
	decoded, err := UnmarshalRow(newTable, raw)
	if err != nil || !decoded["value"].Equal(lir.Int64(42)) {
		t.Fatalf("logical switchover decode = %v err=%v", decoded, err)
	}
}

func TestSetColumnValuePreservesUnknownFramedCells(t *testing.T) {
	wide := table(
		col("c1", "id", model.TypeInt64, false),
		col("c4", "unknown", model.TypeText, true),
	)
	raw, err := MarshalRow(wide, lir.Row{"id": lir.Int64(1), "unknown": lir.Text("preserve")})
	if err != nil {
		t.Fatal(err)
	}
	target := model.Column{ID: "c3", Name: "replacement", Type: model.TypeBool, Nullable: false}
	updated, err := SetColumnValue(raw, target, lir.Bool(true))
	if err != nil {
		t.Fatal(err)
	}
	unknown, err := ReadColumnValue(updated, wide.Columns[1])
	if err != nil || !unknown.Equal(lir.Text("preserve")) {
		t.Fatalf("unknown physical cell = %+v err=%v", unknown, err)
	}
	got, err := ReadColumnValue(updated, target)
	if err != nil || !got.Equal(lir.Bool(true)) {
		t.Fatalf("inserted physical cell = %+v err=%v", got, err)
	}
}

func TestStrictBuiltinColumnConversionRejectsLossAndLocaleGuessing(t *testing.T) {
	intTarget := model.Column{Type: model.TypeInt64, Nullable: false}
	floatTarget := model.Column{Type: model.TypeFloat64, Nullable: false}
	boolTarget := model.Column{Type: model.TypeBool, Nullable: false}
	nullableText := model.Column{Type: model.TypeText, Nullable: true}

	assertConverted := func(in lir.Value, target model.Column, want lir.Value) {
		t.Helper()
		got, err := ConvertColumnValue(in, target, model.ColumnConversionStrictBuiltin)
		if err != nil || got != want {
			t.Fatalf("convert %+v to %s = %+v err=%v, want %+v", in, target.Type, got, err, want)
		}
	}
	assertRejected := func(in lir.Value, target model.Column) {
		t.Helper()
		if got, err := ConvertColumnValue(in, target, model.ColumnConversionStrictBuiltin); err == nil {
			t.Fatalf("lossy conversion %+v to %s returned %+v", in, target.Type, got)
		}
	}

	assertConverted(lir.Text("-42"), intTarget, lir.Int64(-42))
	assertConverted(lir.Float64(42), intTarget, lir.Int64(42))
	assertConverted(lir.Int64(1<<53), floatTarget, lir.Float64(1<<53))
	assertConverted(lir.Text("true"), boolTarget, lir.Bool(true))
	assertConverted(lir.Null(model.TypeInt64), nullableText, lir.Null(model.TypeText))

	assertRejected(lir.Text("1,000"), intTarget)
	assertRejected(lir.Float64(1.5), intTarget)
	assertRejected(lir.Float64(math.NaN()), intTarget)
	assertRejected(lir.Int64(1<<53+1), floatTarget)
	assertRejected(lir.Text("TRUE"), boolTarget)
	assertRejected(lir.Null(model.TypeText), intTarget)
	assertRejected(lir.Bool(true), intTarget)
}
