package codec

import (
	"encoding/binary"
	"fmt"
	"math"
	"slices"
	"strconv"
	"strings"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

// Row storage format. One KV item per logical row, as a schema-directed binary
// body addressed by physical column ID — never the column name, so a rename is
// a pure catalog operation (rows are untouched) and a dropped column simply
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
// needed to traverse it, so a reader skips an unrecognized (dropped) column by
// its length without knowing its type. A physical column ID has one immutable
// physical type for its lifetime and IDs are never reused, so a recognized
// field is always decoded under the right type — the reader consults only the
// live table, never revision history.
//
// A present NULL carries the null flag and no payload; a column absent from the
// body reads back its literal default, else NULL. Generator defaults (uuid,
// now_ms) are never fabricated on read. "Present NULL" and "absent" are
// therefore distinct: an explicit stored NULL reads back NULL even when the
// column has a literal default.
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
	rest, ok := strings.CutPrefix(col.ID, "c")
	if !ok {
		return 0, fmt.Errorf("codec: column %q has malformed physical id %q", col.Name, col.ID)
	}
	n, err := strconv.ParseUint(rest, 10, 64)
	if err != nil {
		return 0, fmt.Errorf("codec: column %q has malformed physical id %q: %w", col.Name, col.ID, err)
	}
	return PhysicalColumnID(n), nil
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
	slices.SortFunc(fields, func(a, b field) int { return int(a.id) - int(b.id) })

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
// current table definition. Unrecognized column IDs (dropped columns) are
// skipped by their frame; column IDs absent from the body get their literal
// default or NULL.
func UnmarshalRow(tbl model.Table, raw []byte) (lir.Row, error) {
	if len(raw) == 0 || raw[0] != rowCanary {
		return nil, fmt.Errorf("codec: value is not a rad row (bad canary)")
	}
	buf := raw[1:]

	columnIndex := make(map[PhysicalColumnID]int, len(tbl.Columns))
	for i, col := range tbl.Columns {
		id, err := physicalID(col)
		if err != nil {
			return nil, err
		}
		columnIndex[id] = i
	}

	values := make([]lir.Value, len(tbl.Columns))
	present := make([]bool, len(tbl.Columns))

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
		if delta == 0 {
			return nil, fmt.Errorf("codec: duplicate or non-ascending column id")
		}
		id := prev + delta
		prev = id
		idx, known := columnIndex[id]

		if header&1 == 1 {
			if known {
				values[idx] = lir.Null(tbl.Columns[idx].Type)
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
		v, err := decodePayload(tbl.Columns[idx].Type, payload)
		if err != nil {
			return nil, err
		}
		values[idx] = v
		present[idx] = true
	}
	if len(buf) != 0 {
		return nil, fmt.Errorf("codec: %d trailing bytes after %d fields", len(buf), count)
	}

	row := make(lir.Row, len(tbl.Columns))
	for i, col := range tbl.Columns {
		if present[i] {
			row[col.Name] = values[i]
			continue
		}
		if col.Default != nil && col.Default.Func == "" {
			v, err := literalDefault(col)
			if err != nil {
				return nil, err
			}
			row[col.Name] = v
		} else {
			row[col.Name] = lir.Null(col.Type)
		}
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

func literalDefault(column model.Column) (lir.Value, error) {
	value := column.Default
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
