package sql

import (
	"fmt"
	"strings"
	"time"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// exprType is the compiler's static view of an expression: the LIR scalar it
// evaluates to, an optional format tag describing the Postgres-facing
// rendering of that scalar, and nullability (used to replicate Postgres NULL
// ordering, which defaults opposite to LIR's).
type exprType struct {
	scalar   lirwire.ScalarType
	format   string
	nullable bool
}

// ResultColumn describes one output column of a compiled statement. Name is
// the SQL-visible column name; Key is the field name in the result datum
// (they differ when sanitizing or uniquifying renamed the LIR field).
type ResultColumn struct {
	Name   string
	Key    string
	Scalar lirwire.ScalarType
	Format string
}

// ParamType is the inferred type of one $N parameter.
type ParamType struct {
	Scalar lirwire.ScalarType
	Format string
}

// Format tags bridge Postgres types the engine has no scalar for. A tagged
// column stores a canonical scalar encoding (timestamps as unix microseconds
// in int64, JSON and hex-escaped bytea as text) and the tag tells the wire
// layer how to render and parse it.
const (
	FormatTimestampTZ = "timestamptz"
	FormatTimestamp   = "timestamp"
	FormatDate        = "date"
	FormatJSONB       = "jsonb"
	FormatJSON        = "json"
	FormatBytea       = "bytea"
	FormatUUID        = "uuid"
)

// IsTimeFormat reports whether the format stores unix microseconds.
func IsTimeFormat(format string) bool {
	switch format {
	case FormatTimestampTZ, FormatTimestamp, FormatDate:
		return true
	}
	return false
}

// pgTypeName resolves a Postgres type name (the canonical short name the
// grammar produces, e.g. "varchar", "timestamptz", "int8") to the rad
// scalar + format pair.
func pgTypeName(name string) (lirwire.ScalarType, string, error) {
	switch strings.ToLower(name) {
	case "text", "varchar", "bpchar", "char", "character", "name", "citext":
		return lirwire.ScalarTypeText, "", nil
	case "uuid":
		return lirwire.ScalarTypeText, FormatUUID, nil
	case "int8", "bigint", "int4", "int", "integer", "int2", "smallint",
		"serial", "bigserial", "smallserial", "serial2", "serial4", "serial8", "oid":
		return lirwire.ScalarTypeInt64, "", nil
	case "bool", "boolean":
		return lirwire.ScalarTypeBool, "", nil
	case "float8", "float4", "real", "numeric", "decimal", "money":
		return lirwire.ScalarTypeFloat64, "", nil
	case "timestamptz":
		return lirwire.ScalarTypeInt64, FormatTimestampTZ, nil
	case "timestamp":
		return lirwire.ScalarTypeInt64, FormatTimestamp, nil
	case "date":
		return lirwire.ScalarTypeInt64, FormatDate, nil
	case "jsonb":
		return lirwire.ScalarTypeText, FormatJSONB, nil
	case "json":
		return lirwire.ScalarTypeText, FormatJSON, nil
	case "bytea":
		return lirwire.ScalarTypeText, FormatBytea, nil
	}
	return "", "", unsupportedf("type %q", name)
}

// timestampLayouts covers the text forms Postgres clients emit. All parse
// into UTC microseconds.
var timestampLayouts = []string{
	"2006-01-02 15:04:05.999999999Z07:00",
	"2006-01-02 15:04:05.999999999Z07",
	"2006-01-02 15:04:05.999999999",
	time.RFC3339Nano,
	"2006-01-02T15:04:05.999999999",
	"2006-01-02",
}

// ParseTimestamp parses a Postgres timestamp text literal to unix
// microseconds.
func ParseTimestamp(s string) (int64, error) {
	for _, layout := range timestampLayouts {
		if t, err := time.Parse(layout, s); err == nil {
			return t.UTC().UnixMicro(), nil
		}
	}
	return 0, fmt.Errorf("cannot parse timestamp literal %q", s)
}

// FormatMicros renders unix microseconds as a UTC time.Time for wire
// encoding.
func FormatMicros(us int64) time.Time {
	return time.UnixMicro(us).UTC()
}
