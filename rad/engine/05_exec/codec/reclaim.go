package codec

import (
	"encoding/binary"
	"fmt"
	"math"
	"strconv"
	"strings"
)

// RemoveColumn removes one physical field from an encoded sparse row without
// decoding any values or consulting a historical type definition. It is used
// by bounded column reclamation after the column's value fence has advanced.
func RemoveColumn(raw []byte, columnID string) ([]byte, bool, error) {
	target, err := parsePhysicalColumnID(columnID)
	if err != nil {
		return nil, false, err
	}
	if len(raw) == 0 || raw[0] != rowCanary {
		return nil, false, fmt.Errorf("codec: value is not a rad row (bad canary)")
	}
	buf := raw[1:]
	count, n := binary.Uvarint(buf)
	if n <= 0 {
		return nil, false, fmt.Errorf("codec: truncated field count")
	}
	buf = buf[n:]

	type framedField struct {
		id      PhysicalColumnID
		null    bool
		payload []byte
	}
	fields := make([]framedField, 0, count)
	var previous PhysicalColumnID
	found := false
	for range count {
		header, n := binary.Uvarint(buf)
		if n <= 0 {
			return nil, false, fmt.Errorf("codec: truncated field header")
		}
		buf = buf[n:]
		delta := PhysicalColumnID(header >> 1)
		if delta == 0 || previous+delta < previous {
			return nil, false, fmt.Errorf("codec: duplicate, non-ascending, or overflowing column id")
		}
		id := previous + delta
		previous = id
		field := framedField{id: id, null: header&1 == 1}
		if !field.null {
			length, n := binary.Uvarint(buf)
			if n <= 0 {
				return nil, false, fmt.Errorf("codec: truncated payload length")
			}
			buf = buf[n:]
			if uint64(len(buf)) < length {
				return nil, false, fmt.Errorf("codec: truncated payload for column id %d", id)
			}
			field.payload = buf[:length]
			buf = buf[length:]
		}
		if id == target {
			found = true
			continue
		}
		fields = append(fields, field)
	}
	if len(buf) != 0 {
		return nil, false, fmt.Errorf("codec: %d trailing bytes after %d fields", len(buf), count)
	}
	if !found {
		return raw, false, nil
	}

	out := make([]byte, 0, len(raw))
	out = append(out, rowCanary)
	out = binary.AppendUvarint(out, uint64(len(fields)))
	previous = 0
	for _, field := range fields {
		delta := field.id - previous
		previous = field.id
		header := uint64(delta) << 1
		if field.null {
			out = binary.AppendUvarint(out, header|1)
			continue
		}
		out = binary.AppendUvarint(out, header)
		out = binary.AppendUvarint(out, uint64(len(field.payload)))
		out = append(out, field.payload...)
	}
	return out, true, nil
}

func parsePhysicalColumnID(id string) (PhysicalColumnID, error) {
	rest, ok := strings.CutPrefix(id, "c")
	if !ok {
		return 0, fmt.Errorf("codec: malformed physical column id %q", id)
	}
	value, err := strconv.ParseUint(rest, 10, 64)
	// One bit in the row-field header is reserved for NULL, so physical IDs
	// must fit in the remaining 63 bits. Rejecting larger IDs here prevents a
	// left shift from silently producing an undecodable row frame.
	if err != nil || value == 0 || value > math.MaxInt64 {
		return 0, fmt.Errorf("codec: malformed physical column id %q", id)
	}
	return PhysicalColumnID(value), nil
}
