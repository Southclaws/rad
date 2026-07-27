package codec

import (
	"fmt"
	"math"
	"strconv"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// ConvertColumnValue applies Rad's deterministic strict built-in replacement
// conversion. It performs no locale, collation, rounding, or lossy numeric
// coercion.
func ConvertColumnValue(
	value lir.Value,
	target model.Column,
	conversion model.ColumnConversion,
) (lir.Value, error) {
	if conversion == "" {
		conversion = model.ColumnConversionStrictBuiltin
	}
	if conversion != model.ColumnConversionStrictBuiltin {
		return lir.Value{}, fmt.Errorf("codec: unsupported column conversion %q", conversion)
	}
	if value.Null {
		if !target.Nullable {
			return lir.Value{}, fmt.Errorf("NULL cannot be converted to non-nullable %s", target.Type)
		}
		return lir.Null(target.Type), nil
	}
	if value.Type == target.Type {
		return value, nil
	}
	switch target.Type {
	case model.TypeText:
		switch value.Type {
		case model.TypeInt64:
			return lir.Text(strconv.FormatInt(value.Int64, 10)), nil
		case model.TypeFloat64:
			return lir.Text(strconv.FormatFloat(value.Float64, 'g', -1, 64)), nil
		case model.TypeBool:
			return lir.Text(strconv.FormatBool(value.Bool)), nil
		}
	case model.TypeInt64:
		switch value.Type {
		case model.TypeText:
			parsed, err := strconv.ParseInt(value.Text, 10, 64)
			if err != nil {
				return lir.Value{}, fmt.Errorf("text %q is not a base-10 int64: %w", value.Text, err)
			}
			return lir.Int64(parsed), nil
		case model.TypeFloat64:
			if math.IsNaN(value.Float64) || math.IsInf(value.Float64, 0) ||
				math.Trunc(value.Float64) != value.Float64 ||
				value.Float64 < -math.Exp2(63) || value.Float64 >= math.Exp2(63) {
				return lir.Value{}, fmt.Errorf("float64 %v is not an exact int64", value.Float64)
			}
			return lir.Int64(int64(value.Float64)), nil
		}
	case model.TypeFloat64:
		switch value.Type {
		case model.TypeText:
			parsed, err := strconv.ParseFloat(value.Text, 64)
			if err != nil {
				return lir.Value{}, fmt.Errorf("text %q is not a float64: %w", value.Text, err)
			}
			return lir.Float64(parsed), nil
		case model.TypeInt64:
			converted := float64(value.Int64)
			if int64(converted) != value.Int64 {
				return lir.Value{}, fmt.Errorf("int64 %d is not exactly representable as float64", value.Int64)
			}
			return lir.Float64(converted), nil
		}
	case model.TypeBool:
		if value.Type == model.TypeText {
			switch value.Text {
			case "true":
				return lir.Bool(true), nil
			case "false":
				return lir.Bool(false), nil
			default:
				return lir.Value{}, fmt.Errorf("text %q is not exactly \"true\" or \"false\"", value.Text)
			}
		}
	}
	return lir.Value{}, fmt.Errorf("no strict built-in conversion from %s to %s", value.Type, target.Type)
}
