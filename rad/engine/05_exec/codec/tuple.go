package codec

import (
	"fmt"
	"math"

	keyenc "github.com/Southclaws/rad/rad/engine/01_kv/keyenc"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// Tuple codec: maps IR values onto the storage layer's order-preserving
// primitive encodings. This is the boundary where semantic values become key
// bytes, so it lives in the executor, not in keyenc (which knows nothing
// about the IR) and not in the IR (which knows nothing about storage).

// encodeValue encodes a single value.
func EncodeValue(v lir.Value) ([]byte, error) {
	if v.Null {
		return keyenc.EncodeNull(), nil
	}
	switch v.Type {
	case model.TypeText:
		return keyenc.EncodeString(v.Text), nil
	case model.TypeInt64:
		return keyenc.EncodeInt64(v.Int64), nil
	case model.TypeBool:
		return keyenc.EncodeBool(v.Bool), nil
	case model.TypeFloat64:
		if math.IsNaN(v.Float64) {
			return nil, fmt.Errorf("exec: cannot encode NaN in a key")
		}
		return keyenc.EncodeFloat64(v.Float64), nil
	default:
		return nil, fmt.Errorf("exec: unsupported type %q in a key", v.Type)
	}
}

// EncodeTuple encodes values in order into a single key fragment. Tuples
// compare element-wise because each element's encoding is self-delimiting.
func EncodeTuple(values []lir.Value) ([]byte, error) {
	var buf []byte
	for _, v := range values {
		enc, err := EncodeValue(v)
		if err != nil {
			return nil, err
		}
		buf = append(buf, enc...)
	}
	return buf, nil
}

// DecodeTuple decodes every value in buf. It fails unless buf consists of
// exactly whole encoded values.
func DecodeTuple(buf []byte) ([]lir.Value, error) {
	var vals []lir.Value
	for len(buf) > 0 {
		v, n, err := decodeValue(buf)
		if err != nil {
			return nil, err
		}
		vals = append(vals, v)
		buf = buf[n:]
	}
	return vals, nil
}

// DecodeValue decodes a single value from the front of buf and returns it
// with the number of bytes consumed.
func DecodeValue(buf []byte) (lir.Value, int, error) {
	return decodeValue(buf)
}

func decodeValue(buf []byte) (lir.Value, int, error) {
	tag, err := keyenc.Peek(buf)
	if err != nil {
		return lir.Value{}, 0, err
	}
	switch tag {
	case keyenc.TagNull:
		n, err := keyenc.DecodeNull(buf)
		return lir.Value{Null: true}, n, err
	case keyenc.TagBool:
		b, n, err := keyenc.DecodeBool(buf)
		return lir.Bool(b), n, err
	case keyenc.TagInt64:
		i, n, err := keyenc.DecodeInt64(buf)
		return lir.Int64(i), n, err
	case keyenc.TagFloat64:
		f, n, err := keyenc.DecodeFloat64(buf)
		return lir.Float64(f), n, err
	case keyenc.TagText:
		s, n, err := keyenc.DecodeString(buf)
		return lir.Text(s), n, err
	}
	return lir.Value{}, 0, fmt.Errorf("exec: unknown tag 0x%02x", tag)
}

// encodeRowTuple encodes the named columns of row, in order, as a key tuple.
func EncodeRowTuple(row lir.Row, columns []string) ([]byte, error) {
	vals := make([]lir.Value, len(columns))
	for i, name := range columns {
		v, ok := row[name]
		if !ok {
			return nil, fmt.Errorf("exec: missing value for column %q", name)
		}
		vals[i] = v
	}
	return EncodeTuple(vals)
}
