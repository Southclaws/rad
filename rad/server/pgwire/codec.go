package pgwire

import (
	"encoding/hex"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/jackc/pgx/v5/pgtype"
	wire "github.com/jeroenrinzema/psql-wire"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/sql"
)

// oidFor maps a compiler result type to the Postgres type OID the wire
// declares. The stored engine scalar differs for bridged types: timestamps
// are int64 microseconds, JSON and bytea are text.
func oidFor(scalar lirwire.ScalarType, format string) uint32 {
	switch format {
	case sql.FormatTimestampTZ:
		return pgtype.TimestamptzOID
	case sql.FormatTimestamp:
		return pgtype.TimestampOID
	case sql.FormatDate:
		return pgtype.DateOID
	case sql.FormatJSONB:
		return pgtype.JSONBOID
	case sql.FormatJSON:
		return pgtype.JSONOID
	case sql.FormatBytea:
		return pgtype.ByteaOID
	case sql.FormatUUID:
		return pgtype.UUIDOID
	}
	switch scalar {
	case lirwire.ScalarTypeText:
		return pgtype.TextOID
	case lirwire.ScalarTypeInt64:
		return pgtype.Int8OID
	case lirwire.ScalarTypeFloat64:
		return pgtype.Float8OID
	case lirwire.ScalarTypeBool:
		return pgtype.BoolOID
	}
	return pgtype.TextOID
}

func wireColumns(cols []sql.ResultColumn) wire.Columns {
	out := make(wire.Columns, len(cols))
	for i, c := range cols {
		out[i] = wire.Column{
			Table: 0,
			Name:  c.Name,
			Oid:   oidFor(c.Scalar, c.Format),
			Width: -1,
		}
	}
	return out
}

func paramOIDs(params []sql.ParamType) []uint32 {
	out := make([]uint32, len(params))
	for i, p := range params {
		out[i] = oidFor(p.Scalar, p.Format)
	}
	return out
}

// decodeArgs converts wire parameters into typed literal values, bridging
// wire-native Go types back to the engine's storage encodings.
func decodeArgs(types []sql.ParamType, params []wire.Parameter) ([]lirwire.Value, error) {
	if len(params) < len(types) {
		return nil, fmt.Errorf("expected %d parameters, got %d", len(types), len(params))
	}
	out := make([]lirwire.Value, len(params))
	for i, param := range params {
		pt := sql.ParamType{Scalar: lirwire.ScalarTypeText}
		if i < len(types) {
			pt = types[i]
		}
		if param.Value() == nil {
			out[i] = lirwire.Null(pt.Scalar)
			continue
		}
		decoded, err := param.Scan(oidFor(pt.Scalar, pt.Format))
		if err != nil {
			return nil, fmt.Errorf("parameter $%d: %w", i+1, err)
		}
		value, err := bridgeValue(pt, decoded)
		if err != nil {
			return nil, fmt.Errorf("parameter $%d: %w", i+1, err)
		}
		out[i] = value
	}
	return out, nil
}

// bridgeValue converts one pgtype-decoded Go value into the engine literal
// for its parameter type.
func bridgeValue(pt sql.ParamType, v any) (lirwire.Value, error) {
	if v == nil {
		return lirwire.Null(pt.Scalar), nil
	}
	if sql.IsTimeFormat(pt.Format) {
		switch t := v.(type) {
		case time.Time:
			return lirwire.Int64(t.UTC().UnixMicro()), nil
		case string:
			us, err := sql.ParseTimestamp(t)
			if err != nil {
				return lirwire.Value{}, err
			}
			return lirwire.Int64(us), nil
		}
		return lirwire.Value{}, fmt.Errorf("cannot bridge %T to %s", v, pt.Format)
	}
	switch pt.Format {
	case sql.FormatJSONB, sql.FormatJSON:
		switch t := v.(type) {
		case []byte:
			return lirwire.Text(string(t)), nil
		case string:
			return lirwire.Text(t), nil
		default:
			raw, err := json.Marshal(t)
			if err != nil {
				return lirwire.Value{}, err
			}
			return lirwire.Text(string(raw)), nil
		}
	case sql.FormatBytea:
		switch t := v.(type) {
		case []byte:
			return lirwire.Text(`\x` + hex.EncodeToString(t)), nil
		case string:
			return lirwire.Text(t), nil
		}
		return lirwire.Value{}, fmt.Errorf("cannot bridge %T to bytea", v)
	case sql.FormatUUID:
		switch t := v.(type) {
		case string:
			return lirwire.Text(t), nil
		case [16]byte:
			u := pgtype.UUID{Bytes: t, Valid: true}
			s, err := u.Value()
			if err != nil {
				return lirwire.Value{}, err
			}
			return lirwire.Text(fmt.Sprint(s)), nil
		}
	}
	switch pt.Scalar {
	case lirwire.ScalarTypeText:
		switch t := v.(type) {
		case string:
			return lirwire.Text(t), nil
		case []byte:
			return lirwire.Text(string(t)), nil
		}
	case lirwire.ScalarTypeInt64:
		switch t := v.(type) {
		case int64:
			return lirwire.Int64(t), nil
		case int32:
			return lirwire.Int64(int64(t)), nil
		case int16:
			return lirwire.Int64(int64(t)), nil
		case int:
			return lirwire.Int64(int64(t)), nil
		}
	case lirwire.ScalarTypeFloat64:
		switch t := v.(type) {
		case float64:
			return lirwire.Float64(t), nil
		case float32:
			return lirwire.Float64(float64(t)), nil
		case int64:
			return lirwire.Float64(float64(t)), nil
		}
	case lirwire.ScalarTypeBool:
		if b, ok := v.(bool); ok {
			return lirwire.Bool(b), nil
		}
	}
	return lirwire.Value{}, fmt.Errorf("cannot bridge %T to %s", v, pt.Scalar)
}

// encodeRow projects one result object onto the declared columns, bridging
// stored scalars back to wire-native Go values for pgtype to encode.
func encodeRow(cols []sql.ResultColumn, row map[string]any) ([]any, error) {
	out := make([]any, len(cols))
	for i, col := range cols {
		v, ok := row[col.Key]
		if !ok || v == nil {
			out[i] = nil
			continue
		}
		encoded, err := encodeValue(col, v)
		if err != nil {
			return nil, fmt.Errorf("column %s: %w", col.Name, err)
		}
		out[i] = encoded
	}
	return out, nil
}

func encodeValue(col sql.ResultColumn, v any) (any, error) {
	if sql.IsTimeFormat(col.Format) {
		us, ok := asInt64(v)
		if !ok {
			return nil, fmt.Errorf("expected microsecond int64, got %T", v)
		}
		return sql.FormatMicros(us), nil
	}
	switch col.Format {
	case sql.FormatJSONB, sql.FormatJSON:
		s, ok := v.(string)
		if !ok {
			return nil, fmt.Errorf("expected JSON text, got %T", v)
		}
		return []byte(s), nil
	case sql.FormatBytea:
		s, ok := v.(string)
		if !ok {
			return nil, fmt.Errorf("expected bytea text, got %T", v)
		}
		raw, err := hex.DecodeString(strings.TrimPrefix(s, `\x`))
		if err != nil {
			return nil, err
		}
		return raw, nil
	}
	switch t := v.(type) {
	case string, int64, float64, bool:
		return t, nil
	case map[string]any, []any:
		// Nested crossing output renders as JSON text.
		raw, err := json.Marshal(t)
		if err != nil {
			return nil, err
		}
		return string(raw), nil
	}
	return v, nil
}

func asInt64(v any) (int64, bool) {
	switch t := v.(type) {
	case int64:
		return t, true
	case float64:
		return int64(t), true
	case json.Number:
		i, err := t.Int64()
		return i, err == nil
	}
	return 0, false
}
