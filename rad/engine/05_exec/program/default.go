package program

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

// DefaultInput preserves a PIR default expression until catalog preflight has
// resolved the target column's type. Typed is used by in-process catalog
// adapters; Func and Literal are the transport-neutral PIR forms.
type DefaultInput struct {
	Typed   *model.Default
	Type    model.Type
	Func    model.DefaultFunc
	Literal []byte
}

func TypedDefault(value *model.Default) *DefaultInput {
	if value == nil {
		return nil
	}
	copy := *value
	return &DefaultInput{Typed: &copy}
}

func TypedDefaultForType(value *model.Default, typ model.Type) *DefaultInput {
	input := TypedDefault(value)
	if input != nil {
		input.Type = typ
	}
	return input
}

func (d DefaultInput) Resolve(column model.Column) (*model.Default, error) {
	switch {
	case d.Typed != nil && (d.Func != "" || d.Literal != nil):
		return nil, fmt.Errorf("default input mixes typed and wire forms")
	case d.Func != "" && d.Literal != nil:
		return nil, fmt.Errorf("default input sets both generator and literal")
	case d.Typed != nil:
		if d.Type != "" && d.Type != column.Type {
			return nil, fmt.Errorf(
				"default was typed as %s but column %q is %s",
				d.Type,
				column.Name,
				column.Type,
			)
		}
		copy := *d.Typed
		return &copy, nil
	case d.Func != "":
		return &model.Default{Func: d.Func}, nil
	case d.Literal == nil:
		return nil, fmt.Errorf("default input is empty")
	}

	decoder := json.NewDecoder(bytes.NewReader(d.Literal))
	decoder.UseNumber()
	var literal any
	if err := decoder.Decode(&literal); err != nil {
		return nil, fmt.Errorf("decode literal default: %w", err)
	}
	var trailing any
	if err := decoder.Decode(&trailing); err != io.EOF {
		if err == nil {
			return nil, fmt.Errorf("decode literal default: trailing JSON value")
		}
		return nil, fmt.Errorf("decode literal default: %w", err)
	}
	switch column.Type {
	case model.TypeText:
		value, ok := literal.(string)
		if !ok {
			return nil, fmt.Errorf("column %q default expects a string, got %T", column.Name, literal)
		}
		return &model.Default{Text: value}, nil
	case model.TypeInt64:
		value, ok := literal.(json.Number)
		if !ok {
			return nil, fmt.Errorf("column %q default expects a number, got %T", column.Name, literal)
		}
		integer, err := value.Int64()
		if err != nil {
			return nil, fmt.Errorf("column %q default expects an integer, got %v", column.Name, value)
		}
		return &model.Default{Int64: integer}, nil
	case model.TypeFloat64:
		value, ok := literal.(json.Number)
		if !ok {
			return nil, fmt.Errorf("column %q default expects a number, got %T", column.Name, literal)
		}
		floating, err := value.Float64()
		if err != nil {
			return nil, fmt.Errorf("column %q default expects a float, got %v", column.Name, value)
		}
		return &model.Default{Float64: floating}, nil
	case model.TypeBool:
		value, ok := literal.(bool)
		if !ok {
			return nil, fmt.Errorf("column %q default expects a boolean, got %T", column.Name, literal)
		}
		return &model.Default{Bool: value}, nil
	default:
		return nil, fmt.Errorf("column %q has unsupported type %q", column.Name, column.Type)
	}
}
