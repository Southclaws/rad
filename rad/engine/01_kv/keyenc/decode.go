package keyenc

import (
	"encoding/binary"
	"fmt"
	"math"
)

// Peek returns the type tag of the next encoded value in buf.
func Peek(buf []byte) (byte, error) {
	if len(buf) == 0 {
		return 0, fmt.Errorf("keyenc: empty buffer")
	}
	switch buf[0] {
	case TagNull, TagBool, TagInt64, TagFloat64, TagText:
		return buf[0], nil
	}
	return 0, fmt.Errorf("keyenc: unknown tag 0x%02x", buf[0])
}

// DecodeBool decodes a bool from the front of buf and returns it with the
// number of bytes consumed.
func DecodeBool(buf []byte) (bool, int, error) {
	if len(buf) == 0 || buf[0] != TagBool {
		return false, 0, fmt.Errorf("keyenc: not a bool")
	}
	if len(buf) < 2 {
		return false, 0, fmt.Errorf("keyenc: truncated bool")
	}
	switch buf[1] {
	case 0x00:
		return false, 2, nil
	case 0x01:
		return true, 2, nil
	}
	return false, 0, fmt.Errorf("keyenc: invalid bool byte 0x%02x", buf[1])
}

// DecodeNull consumes a NULL from the front of buf and returns the number of
// bytes consumed.
func DecodeNull(buf []byte) (int, error) {
	if len(buf) == 0 || buf[0] != TagNull {
		return 0, fmt.Errorf("keyenc: not a null")
	}
	return 1, nil
}

// DecodeInt64 decodes an int64 from the front of buf and returns it with the
// number of bytes consumed.
func DecodeInt64(buf []byte) (int64, int, error) {
	if len(buf) == 0 || buf[0] != TagInt64 {
		return 0, 0, fmt.Errorf("keyenc: not an int64")
	}
	if len(buf) < 9 {
		return 0, 0, fmt.Errorf("keyenc: truncated int64")
	}
	u := binary.BigEndian.Uint64(buf[1:9])
	return int64(u ^ (1 << 63)), 9, nil
}

// DecodeFloat64 decodes a float64 from the front of buf and returns it with
// the number of bytes consumed.
func DecodeFloat64(buf []byte) (float64, int, error) {
	if len(buf) == 0 || buf[0] != TagFloat64 {
		return 0, 0, fmt.Errorf("keyenc: not a float64")
	}
	if len(buf) < 9 {
		return 0, 0, fmt.Errorf("keyenc: truncated float64")
	}
	bits := binary.BigEndian.Uint64(buf[1:9])
	if bits&(1<<63) != 0 {
		bits ^= 1 << 63
	} else {
		bits = ^bits
	}
	return math.Float64frombits(bits), 9, nil
}

// DecodeString decodes a string from the front of buf and returns it with
// the number of bytes consumed.
func DecodeString(buf []byte) (string, int, error) {
	if len(buf) == 0 || buf[0] != TagText {
		return "", 0, fmt.Errorf("keyenc: not a string")
	}
	out := make([]byte, 0, len(buf))
	i := 1
	for {
		if i >= len(buf) {
			return "", 0, fmt.Errorf("keyenc: unterminated string")
		}
		if buf[i] != escape {
			out = append(out, buf[i])
			i++
			continue
		}
		if i+1 >= len(buf) {
			return "", 0, fmt.Errorf("keyenc: truncated escape")
		}
		switch buf[i+1] {
		case terminator:
			return string(out), i + 2, nil
		case escapedFF:
			out = append(out, escape)
			i += 2
		default:
			return "", 0, fmt.Errorf("keyenc: invalid escape byte 0x%02x", buf[i+1])
		}
	}
}
