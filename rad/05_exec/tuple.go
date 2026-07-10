package exec

import (
	"fmt"
	"math"

	keyenc "rad/rad/01_kv/keyenc"
	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
)

// Tuple codec: maps IR values onto the storage layer's order-preserving
// primitive encodings. This is the boundary where semantic values become key
// bytes, so it lives in the executor, not in keyenc (which knows nothing
// about the IR) and not in the IR (which knows nothing about storage).

// encodeValue encodes a single value.
func encodeValue(v qir.Value) ([]byte, error) {
	if v.Null {
		return keyenc.EncodeNull(), nil
	}
	switch v.Type {
	case catalog.TypeText:
		return keyenc.EncodeString(v.Text), nil
	case catalog.TypeInt64:
		return keyenc.EncodeInt64(v.Int64), nil
	case catalog.TypeBool:
		return keyenc.EncodeBool(v.Bool), nil
	case catalog.TypeFloat64:
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
func EncodeTuple(values []qir.Value) ([]byte, error) {
	var buf []byte
	for _, v := range values {
		enc, err := encodeValue(v)
		if err != nil {
			return nil, err
		}
		buf = append(buf, enc...)
	}
	return buf, nil
}

// DecodeTuple decodes every value in buf. It fails unless buf consists of
// exactly whole encoded values.
func DecodeTuple(buf []byte) ([]qir.Value, error) {
	var vals []qir.Value
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
func DecodeValue(buf []byte) (qir.Value, int, error) {
	return decodeValue(buf)
}

func decodeValue(buf []byte) (qir.Value, int, error) {
	tag, err := keyenc.Peek(buf)
	if err != nil {
		return qir.Value{}, 0, err
	}
	switch tag {
	case keyenc.TagNull:
		n, err := keyenc.DecodeNull(buf)
		return qir.Value{Null: true}, n, err
	case keyenc.TagBool:
		b, n, err := keyenc.DecodeBool(buf)
		return qir.Bool(b), n, err
	case keyenc.TagInt64:
		i, n, err := keyenc.DecodeInt64(buf)
		return qir.Int64(i), n, err
	case keyenc.TagFloat64:
		f, n, err := keyenc.DecodeFloat64(buf)
		return qir.Float64(f), n, err
	case keyenc.TagText:
		s, n, err := keyenc.DecodeString(buf)
		return qir.Text(s), n, err
	}
	return qir.Value{}, 0, fmt.Errorf("exec: unknown tag 0x%02x", tag)
}

// encodeRowTuple encodes the named columns of row, in order, as a key tuple.
func encodeRowTuple(row qir.Row, columns []string) ([]byte, error) {
	vals := make([]qir.Value, len(columns))
	for i, name := range columns {
		v, ok := row[name]
		if !ok {
			return nil, fmt.Errorf("exec: missing value for column %q", name)
		}
		vals[i] = v
	}
	return EncodeTuple(vals)
}
