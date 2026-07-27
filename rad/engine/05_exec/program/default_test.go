package program

import (
	"math"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

func TestDefaultInputResolvesAfterColumnTypeIsKnown(t *testing.T) {
	cases := []struct {
		name   string
		column model.Column
		raw    string
		want   model.Default
	}{
		{
			name: "text", column: model.Column{Name: "v", Type: model.TypeText},
			raw: `"hello"`, want: model.Default{Text: "hello"},
		},
		{
			name:   "int64 beyond float precision",
			column: model.Column{Name: "v", Type: model.TypeInt64},
			raw:    "9007199254740993", want: model.Default{Int64: 9007199254740993},
		},
		{
			name:   "float",
			column: model.Column{Name: "v", Type: model.TypeFloat64},
			raw:    "1.25", want: model.Default{Float64: 1.25},
		},
		{
			name:   "bool false",
			column: model.Column{Name: "v", Type: model.TypeBool},
			raw:    "false", want: model.Default{Bool: false},
		},
	}
	for _, test := range cases {
		t.Run(test.name, func(t *testing.T) {
			got, err := (DefaultInput{Literal: []byte(test.raw)}).Resolve(test.column)
			if err != nil {
				t.Fatal(err)
			}
			if got == nil || *got != test.want {
				t.Fatalf("resolved = %+v, want %+v", got, test.want)
			}
		})
	}
}

func TestDefaultInputRejectsAmbiguousOrInvalidForms(t *testing.T) {
	column := model.Column{Name: "v", Type: model.TypeInt64}
	cases := []DefaultInput{
		{},
		{Literal: []byte(`"not an integer"`)},
		{Literal: []byte("1.5")},
		{Literal: []byte("1 2")},
		{Literal: []byte("null")},
		{Literal: []byte("9223372036854775808")},
		{Typed: &model.Default{Int64: math.MaxInt64}, Literal: []byte("1")},
		{Func: model.DefaultNowMS, Literal: []byte("1")},
	}
	for i, input := range cases {
		if value, err := input.Resolve(column); err == nil {
			t.Fatalf("case %d resolved invalid input as %+v", i, value)
		}
	}
}
