package resultjson

import (
	"encoding/json"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

func Datum(d lir.Datum) any {
	switch d.Kind {
	case lir.DatumScalar:
		return scalar(d.Scalar)
	case lir.DatumObject:
		out := make(map[string]any, len(d.Fields))
		for _, field := range d.Fields {
			out[field.Name] = Datum(field.Datum)
		}
		return out
	case lir.DatumArray:
		out := make([]any, len(d.Elems))
		for i, elem := range d.Elems {
			out[i] = Datum(elem)
		}
		return out
	default:
		return nil
	}
}

func Row(row lir.Row) map[string]any {
	out := make(map[string]any, len(row))
	for name, value := range row {
		out[name] = scalar(value)
	}
	return out
}

func Marshal(d lir.Datum) ([]byte, error) {
	return json.MarshalIndent(Datum(d), "", "  ")
}

func scalar(value lir.Value) any {
	if value.Null {
		return nil
	}
	switch value.Type {
	case model.TypeText:
		return value.Text
	case model.TypeInt64:
		return value.Int64
	case model.TypeFloat64:
		return value.Float64
	case model.TypeBool:
		return value.Bool
	default:
		return nil
	}
}
