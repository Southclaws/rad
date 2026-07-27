package codec

import (
	"cmp"
	"encoding/binary"
	"fmt"
	"math"
	"slices"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// Row storage format. One KV item per logical row, as a schema-directed binary
// body addressed by physical column ID — never the column name, so a rename is
// a pure catalog operation (rows are untouched) and a deleted column simply
// orphans its bytes.
//
// Layout:
//
//	byte     canary (rowCanary)
//	uvarint  field count
//	per field, ascending by physical column ID:
//	    uvarint  (columnDelta << 1) | nullFlag
//	    if non-null: uvarint payload length, then payload
//
// The catalog supplies each field's type; the row supplies only the framing
// needed to traverse it, so a reader skips an unrecognized (deleted) column by
// its length without knowing its type. A physical column ID has one immutable
// physical type for its lifetime and IDs are never reused, so a recognized
// field is always decoded under the right type — the reader consults only the
// live table, never revision history.
//
// A present NULL carries the null flag and no payload; a column absent from the
// body reads back its immutable historical MissingValue, else NULL. Current
// insert defaults, including generators, are never consulted on read. "Present
// NULL" and "absent" are therefore distinct: an explicit stored NULL reads
// back NULL even when the physical column has a historical missing value.
//
// Payloads: bool one byte (0x00/0x01); int64 eight bytes big-endian
// two's-complement; float64 eight bytes big-endian IEEE-754 bits; text raw
// bytes (length from the frame). Text round-trips byte for byte; UTF-8 is a
// text-contract concern enforced at write time, not by the reader.

// rowCanary opens every stored row value. A first byte that is not this means
// the value is not a Rad row or is corrupt. 0x52 is ASCII 'R' — the R of RADS,
// which is what the server port 7237 spells on a T9 phone keypad.
const rowCanary byte = 'R'

// PhysicalColumnID is a column's stable physical storage identity, distinct
// from a logical model.SchemaID. Row fields are tagged with it; it is never
// reused and its column's physical type never changes, so a stored field is
// always safe to decode against the current catalog.
type PhysicalColumnID uint64

func physicalID(col model.Column) (PhysicalColumnID, error) {
	id, err := parsePhysicalColumnID(col.ID)
	if err != nil {
		return 0, fmt.Errorf("codec: column %q has malformed physical id %q: %w", col.Name, col.ID, err)
	}
	return id, nil
}

// MarshalRow encodes a name-keyed row for storage. Every column present in the
// row is written (an explicit NULL as a present-NULL field); a column absent
// from the row is not written and takes its default-or-NULL meaning on read.
// Exported for tooling (cmd/rad); the engine handles this internally.
func MarshalRow(tbl model.Table, row lir.Row) ([]byte, error) {
	type field struct {
		id  PhysicalColumnID
		val lir.Value
	}
	fields := make([]field, 0, len(row))
	for name, v := range row {
		col, ok := tbl.Column(name)
		if !ok {
			return nil, fmt.Errorf("codec: table %q has no column %q", tbl.Name, name)
		}
		id, err := physicalID(col)
		if err != nil {
			return nil, err
		}
		fields = append(fields, field{id: id, val: v})
	}
	slices.SortFunc(fields, func(a, b field) int { return cmp.Compare(a.id, b.id) })

	buf := make([]byte, 0, 1+binary.MaxVarintLen64+len(fields)*(binary.MaxVarintLen64+8))
	buf = append(buf, rowCanary)
	buf = binary.AppendUvarint(buf, uint64(len(fields)))
	var prev PhysicalColumnID
	for _, f := range fields {
		delta := f.id - prev
		prev = f.id
		header := uint64(delta) << 1
		if f.val.Null {
			buf = binary.AppendUvarint(buf, header|1)
			continue
		}
		buf = binary.AppendUvarint(buf, header)
		payload, err := encodePayload(f.val)
		if err != nil {
			return nil, err
		}
		buf = binary.AppendUvarint(buf, uint64(len(payload)))
		buf = append(buf, payload...)
	}
	return buf, nil
}

// UnmarshalRow decodes a stored row into a name-keyed row according to the
// current table definition. Unrecognized column IDs (deleted columns) are
// skipped by their frame; column IDs absent from the body get their immutable
// historical missing value or NULL.
func UnmarshalRow(tbl model.Table, raw []byte) (lir.Row, error) {
	return UnmarshalRowColumns(tbl, tbl.Columns, raw)
}

// UnmarshalRowColumns decodes only the requested physical columns while still
// validating and traversing the complete stored frame. Callers use this after
// admitting the corresponding column-value dependencies; skipped cells need
// neither a live catalog definition nor a decoded value.
func UnmarshalRowColumns(tbl model.Table, columns []model.Column, raw []byte) (lir.Row, error) {
	if len(raw) == 0 || raw[0] != rowCanary {
		return nil, fmt.Errorf("codec: value is not a rad row (bad canary)")
	}
	buf := raw[1:]

	liveColumns := make(map[string]bool, len(tbl.Columns))
	for _, column := range tbl.Columns {
		liveColumns[column.ID] = true
	}
	columnIndex := make(map[PhysicalColumnID]int, len(columns))
	for i, col := range columns {
		if !liveColumns[col.ID] {
			return nil, fmt.Errorf("codec: column %q does not belong to table %q", col.Name, tbl.Name)
		}
		id, err := physicalID(col)
		if err != nil {
			return nil, err
		}
		if _, duplicate := columnIndex[id]; duplicate {
			return nil, fmt.Errorf("codec: column %q requested twice", col.Name)
		}
		columnIndex[id] = i
	}

	values := make([]lir.Value, len(columns))
	present := make([]bool, len(columns))

	count, n := binary.Uvarint(buf)
	if n <= 0 {
		return nil, fmt.Errorf("codec: truncated field count")
	}
	buf = buf[n:]

	var prev PhysicalColumnID
	for range count {
		header, n := binary.Uvarint(buf)
		if n <= 0 {
			return nil, fmt.Errorf("codec: truncated field header")
		}
		buf = buf[n:]
		delta := PhysicalColumnID(header >> 1)
		if delta == 0 || prev+delta < prev {
			return nil, fmt.Errorf("codec: duplicate, non-ascending, or overflowing column id")
		}
		id := prev + delta
		prev = id
		idx, known := columnIndex[id]

		if header&1 == 1 {
			if known {
				values[idx] = lir.Null(columns[idx].Type)
				present[idx] = true
			}
			continue
		}

		length, n := binary.Uvarint(buf)
		if n <= 0 {
			return nil, fmt.Errorf("codec: truncated payload length")
		}
		buf = buf[n:]
		if uint64(len(buf)) < length {
			return nil, fmt.Errorf("codec: truncated payload for column id %d", id)
		}
		payload := buf[:length]
		buf = buf[length:]
		if !known {
			continue
		}
		v, err := decodePayload(columns[idx].Type, payload)
		if err != nil {
			return nil, err
		}
		values[idx] = v
		present[idx] = true
	}
	if len(buf) != 0 {
		return nil, fmt.Errorf("codec: %d trailing bytes after %d fields", len(buf), count)
	}

	row := make(lir.Row, len(columns))
	for i, col := range columns {
		if present[i] {
			row[col.Name] = values[i]
			continue
		}
		v, err := DecodeMissingValue(col)
		if err != nil {
			return nil, err
		}
		row[col.Name] = v
	}
	return row, nil
}

func encodePayload(v lir.Value) ([]byte, error) {
	switch v.Type {
	case model.TypeText:
		return []byte(v.Text), nil
	case model.TypeInt64:
		b := make([]byte, 8)
		binary.BigEndian.PutUint64(b, uint64(v.Int64))
		return b, nil
	case model.TypeFloat64:
		b := make([]byte, 8)
		binary.BigEndian.PutUint64(b, math.Float64bits(v.Float64))
		return b, nil
	case model.TypeBool:
		if v.Bool {
			return []byte{1}, nil
		}
		return []byte{0}, nil
	default:
		return nil, fmt.Errorf("codec: cannot encode type %q", v.Type)
	}
}

func decodePayload(t model.Type, payload []byte) (lir.Value, error) {
	switch t {
	case model.TypeText:
		return lir.Text(string(payload)), nil
	case model.TypeInt64:
		if len(payload) != 8 {
			return lir.Value{}, fmt.Errorf("codec: int64 payload is %d bytes, want 8", len(payload))
		}
		return lir.Int64(int64(binary.BigEndian.Uint64(payload))), nil
	case model.TypeFloat64:
		if len(payload) != 8 {
			return lir.Value{}, fmt.Errorf("codec: float64 payload is %d bytes, want 8", len(payload))
		}
		return lir.Float64(math.Float64frombits(binary.BigEndian.Uint64(payload))), nil
	case model.TypeBool:
		if len(payload) != 1 {
			return lir.Value{}, fmt.Errorf("codec: bool payload is %d bytes, want 1", len(payload))
		}
		switch payload[0] {
		case 0:
			return lir.Bool(false), nil
		case 1:
			return lir.Bool(true), nil
		default:
			return lir.Value{}, fmt.Errorf("codec: bool payload byte is 0x%02x, want 0x00 or 0x01", payload[0])
		}
	default:
		return lir.Value{}, fmt.Errorf("codec: cannot decode type %q", t)
	}
}

// DecodeMissingValue returns the stable logical value of an absent physical
// cell. MissingValue is historical row semantics, not the current insert
// default; generator defaults are therefore invalid here.
func DecodeMissingValue(column model.Column) (lir.Value, error) {
	value := column.MissingValue
	if value == nil {
		return lir.Null(column.Type), nil
	}
	if value.Func != "" {
		return lir.Value{}, fmt.Errorf("codec: historical missing value cannot use generator %q", value.Func)
	}
	switch column.Type {
	case model.TypeText:
		return lir.Text(value.Text), nil
	case model.TypeInt64:
		return lir.Int64(value.Int64), nil
	case model.TypeFloat64:
		return lir.Float64(value.Float64), nil
	case model.TypeBool:
		return lir.Bool(value.Bool), nil
	default:
		return lir.Value{}, fmt.Errorf("codec: unsupported default type %q", column.Type)
	}
}
