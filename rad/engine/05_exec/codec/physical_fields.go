package codec

import (
	"encoding/binary"
	"fmt"
	"slices"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
)

type physicalField struct {
	id      PhysicalColumnID
	null    bool
	payload []byte
}

func decodePhysicalFields(raw []byte) ([]physicalField, error) {
	if len(raw) == 0 || raw[0] != rowCanary {
		return nil, fmt.Errorf("codec: value is not a rad row (bad canary)")
	}
	buf := raw[1:]
	count, n := binary.Uvarint(buf)
	if n <= 0 {
		return nil, fmt.Errorf("codec: truncated field count")
	}
	buf = buf[n:]
	fields := make([]physicalField, 0, count)
	var previous PhysicalColumnID
	for range count {
		header, n := binary.Uvarint(buf)
		if n <= 0 {
			return nil, fmt.Errorf("codec: truncated field header")
		}
		buf = buf[n:]
		delta := PhysicalColumnID(header >> 1)
		if delta == 0 || previous+delta < previous {
			return nil, fmt.Errorf("codec: duplicate, non-ascending, or overflowing column id")
		}
		field := physicalField{id: previous + delta, null: header&1 == 1}
		previous = field.id
		if !field.null {
			length, n := binary.Uvarint(buf)
			if n <= 0 {
				return nil, fmt.Errorf("codec: truncated payload length")
			}
			buf = buf[n:]
			if uint64(len(buf)) < length {
				return nil, fmt.Errorf("codec: truncated payload for column id %d", field.id)
			}
			field.payload = slices.Clone(buf[:length])
			buf = buf[length:]
		}
		fields = append(fields, field)
	}
	if len(buf) != 0 {
		return nil, fmt.Errorf("codec: %d trailing bytes after %d fields", len(buf), count)
	}
	return fields, nil
}

func encodePhysicalFields(fields []physicalField) []byte {
	out := make([]byte, 0, 1+binary.MaxVarintLen64+len(fields)*(binary.MaxVarintLen64+8))
	out = append(out, rowCanary)
	out = binary.AppendUvarint(out, uint64(len(fields)))
	var previous PhysicalColumnID
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
	return out
}

// ReadColumnValue decodes one immutable physical column representation. An
// absent cell uses that representation's historical literal-default-or-null
// rule, independent from the current logical table definition.
func ReadColumnValue(raw []byte, column model.Column) (lir.Value, error) {
	target, err := parsePhysicalColumnID(column.ID)
	if err != nil {
		return lir.Value{}, err
	}
	fields, err := decodePhysicalFields(raw)
	if err != nil {
		return lir.Value{}, err
	}
	for _, field := range fields {
		if field.id != target {
			continue
		}
		if field.null {
			return lir.Null(column.Type), nil
		}
		return decodePayload(column.Type, field.payload)
	}
	return DecodeMissingValue(column)
}

// SetColumnValue adds or replaces one physical cell while preserving every
// other framed representation byte-for-byte.
func SetColumnValue(raw []byte, column model.Column, value lir.Value) ([]byte, error) {
	target, err := parsePhysicalColumnID(column.ID)
	if err != nil {
		return nil, err
	}
	if !value.Null && value.Type != column.Type {
		return nil, fmt.Errorf("codec: physical column %q expects %s, got %s", column.ID, column.Type, value.Type)
	}
	fields, err := decodePhysicalFields(raw)
	if err != nil {
		return nil, err
	}
	replacement := physicalField{id: target, null: value.Null}
	if !value.Null {
		replacement.payload, err = encodePayload(value)
		if err != nil {
			return nil, err
		}
	}
	position, found := slices.BinarySearchFunc(fields, target, func(field physicalField, id PhysicalColumnID) int {
		switch {
		case field.id < id:
			return -1
		case field.id > id:
			return 1
		default:
			return 0
		}
	})
	if found {
		fields[position] = replacement
	} else {
		fields = slices.Insert(fields, position, replacement)
	}
	return encodePhysicalFields(fields), nil
}
