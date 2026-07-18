package api

import (
	"math"
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

func ptrStr(s string) *string { return &s }

func TestDecodeScalar(t *testing.T) {
	big := "9223372036854775807"    // max int64
	small := "-9223372036854775808" // min int64
	over := "9223372036854775808"   // max int64 + 1

	cases := []struct {
		name   string
		kind   lirwire.ScalarType
		lexeme string
		want   any
		errSub string // non-empty => expect an error containing this
	}{
		{"text passthrough", lirwire.ScalarTypeText, "hello", "hello", ""},
		{"text empty", lirwire.ScalarTypeText, "", "", ""},
		{"int64", lirwire.ScalarTypeInt64, "56", int64(56), ""},
		{"int64 negative", lirwire.ScalarTypeInt64, "-56", int64(-56), ""},
		{"int64 max", lirwire.ScalarTypeInt64, big, int64(math.MaxInt64), ""},
		{"int64 min", lirwire.ScalarTypeInt64, small, int64(math.MinInt64), ""},
		{"int64 over range", lirwire.ScalarTypeInt64, over, nil, "out of the signed 64-bit range"},
		{"int64 fractional", lirwire.ScalarTypeInt64, "5.5", nil, "malformed"},
		{"int64 empty", lirwire.ScalarTypeInt64, "", nil, "malformed"},
		{"float64", lirwire.ScalarTypeFloat64, "0.333312", 0.333312, ""},
		{"float64 integer form", lirwire.ScalarTypeFloat64, "56", float64(56), ""},
		{"float64 NaN rejected", lirwire.ScalarTypeFloat64, "NaN", nil, "not finite"},
		{"float64 Inf rejected", lirwire.ScalarTypeFloat64, "Inf", nil, "not finite"},
		{"float64 malformed", lirwire.ScalarTypeFloat64, "abc", nil, "malformed"},
		{"bool true", lirwire.ScalarTypeBool, "true", true, ""},
		{"bool false", lirwire.ScalarTypeBool, "false", false, ""},
		{"bool rejects 1", lirwire.ScalarTypeBool, "1", nil, `must be "true" or "false"`},
		{"bool rejects True", lirwire.ScalarTypeBool, "True", nil, `must be "true" or "false"`},
		{"unknown kind", lirwire.ScalarType("decimal"), "1", nil, "unknown scalar type"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got, err := decodeScalar(tc.kind, tc.lexeme)
			if tc.errSub != "" {
				if err == nil {
					t.Fatalf("expected error containing %q, got value %#v", tc.errSub, got)
				}
				if !strings.Contains(err.Error(), tc.errSub) {
					t.Fatalf("error %q should contain %q", err, tc.errSub)
				}
				return
			}
			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if got != tc.want {
				t.Fatalf("got %#v (%T), want %#v (%T)", got, got, tc.want, tc.want)
			}
		})
	}
}

// A nil cell is a NULL regardless of column type; a present cell decodes
// against that column's declared type.
func TestDecodeCell(t *testing.T) {
	if v, err := decodeCell(lirwire.ScalarTypeInt64, nil); err != nil || v != nil {
		t.Fatalf("nil int64 cell = (%#v, %v), want (nil, nil)", v, err)
	}
	if v, err := decodeCell(lirwire.ScalarTypeText, nil); err != nil || v != nil {
		t.Fatalf("nil text cell = (%#v, %v), want (nil, nil)", v, err)
	}
	v, err := decodeCell(lirwire.ScalarTypeInt64, ptrStr("42"))
	if err != nil || v != int64(42) {
		t.Fatalf("int64 cell \"42\" = (%#v, %v), want (int64(42), nil)", v, err)
	}
	// A bool cell is the string "true"/"false", decoded against the column.
	v, err = decodeCell(lirwire.ScalarTypeBool, ptrStr("true"))
	if err != nil || v != true {
		t.Fatalf("bool cell \"true\" = (%#v, %v), want (true, nil)", v, err)
	}
	if _, err := decodeCell(lirwire.ScalarTypeBool, ptrStr("yes")); err == nil {
		t.Fatal("bool cell \"yes\" should be rejected")
	}
}

// A self-describing literal decodes by its variant; an absent payload is a NULL.
func TestDecodeValue(t *testing.T) {
	cases := []struct {
		name string
		v    lirwire.Value
		want any
	}{
		{"text", lirwire.Value{ValueUnion: &lirwire.TextValue{Type: "text", Value: ptrStr("x")}}, "x"},
		{"int64", lirwire.Value{ValueUnion: &lirwire.Int64Value{Type: "int64", Value: ptrStr("56")}}, int64(56)},
		{"float64", lirwire.Value{ValueUnion: &lirwire.Float64Value{Type: "float64", Value: ptrStr("1.5")}}, 1.5},
		{"bool", lirwire.Value{ValueUnion: &lirwire.BoolValue{Type: "bool", Value: ptrBool(true)}}, true},
		{"int64 null", lirwire.Value{ValueUnion: &lirwire.Int64Value{Type: "int64"}}, nil},
		{"text null", lirwire.Value{ValueUnion: &lirwire.TextValue{Type: "text"}}, nil},
		{"bool null", lirwire.Value{ValueUnion: &lirwire.BoolValue{Type: "bool"}}, nil},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			got, err := decodeValue(tc.v)
			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if got != tc.want {
				t.Fatalf("got %#v (%T), want %#v", got, got, tc.want)
			}
		})
	}

	// int64 range and float finiteness are enforced through the variant too.
	if _, err := decodeValue(lirwire.Value{ValueUnion: &lirwire.Int64Value{Type: "int64", Value: ptrStr("9223372036854775808")}}); err == nil {
		t.Fatal("over-range int64 literal should be rejected")
	}
	// An empty union (no variant) is not a value.
	if _, err := decodeValue(lirwire.Value{}); err == nil {
		t.Fatal("empty Value should be rejected")
	}
}
