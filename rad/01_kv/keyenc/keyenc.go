// Package keyenc implements order-preserving byte encodings for primitive
// values, in the style of CockroachDB's key encoding (see
// references/cockroachdb-encoding-tech-note.md).
//
// Every encoded value starts with a type tag byte so sequences of mixed
// types order deterministically and decode unambiguously. Within a type, the
// encoded bytes sort lexicographically in the same order as the values sort
// semantically. Encodings are self-delimiting, so encoded values can be
// concatenated into tuples with no separator.
//
// This package is storage vocabulary: it knows bytes and Go primitives,
// nothing about rows, columns, or the IR's value model. Higher layers
// (05_exec) compose these primitives into row-key tuples.
package keyenc

import (
	"encoding/binary"
	"math"
)

// Type tags, in sort order: NULL sorts before every non-null value, and
// mixed-type sequences order by tag first.
const (
	TagNull    byte = 0x01
	TagBool    byte = 0x02
	TagInt64   byte = 0x03
	TagFloat64 byte = 0x04
	TagText    byte = 0x05
)

// String terminator and escape, CockroachDB-style: a 0x00 byte inside the
// string is escaped as {0x00, 0xFF}; the string ends with {0x00, 0x01}.
// Because 0x01 < 0xFF, a string is always ordered before any of its
// extensions ("app" < "apple").
const (
	escape     byte = 0x00
	escapedFF  byte = 0xFF
	terminator byte = 0x01
)

// EncodeString returns an order-preserving encoding of s.
func EncodeString(s string) []byte {
	buf := make([]byte, 0, len(s)+3)
	buf = append(buf, TagText)
	for i := 0; i < len(s); i++ {
		if s[i] == escape {
			buf = append(buf, escape, escapedFF)
		} else {
			buf = append(buf, s[i])
		}
	}
	return append(buf, escape, terminator)
}

// EncodeInt64 encodes i so that lexicographic byte order matches numeric
// order: flip the sign bit and store big-endian.
func EncodeInt64(i int64) []byte {
	buf := make([]byte, 9)
	buf[0] = TagInt64
	binary.BigEndian.PutUint64(buf[1:], uint64(i)^(1<<63))
	return buf
}

// EncodeFloat64 encodes f so that lexicographic byte order matches numeric
// order: for non-negative floats flip the sign bit, for negative floats flip
// all bits. Callers must reject NaN; its ordering is undefined.
func EncodeFloat64(f float64) []byte {
	bits := math.Float64bits(f)
	if bits&(1<<63) != 0 {
		bits = ^bits
	} else {
		bits |= 1 << 63
	}
	buf := make([]byte, 9)
	buf[0] = TagFloat64
	binary.BigEndian.PutUint64(buf[1:], bits)
	return buf
}

// EncodeNull encodes the NULL sentinel, which sorts before every non-null
// value.
func EncodeNull() []byte {
	return []byte{TagNull}
}

// EncodeBool encodes b with false sorting before true.
func EncodeBool(b bool) []byte {
	if b {
		return []byte{TagBool, 0x01}
	}
	return []byte{TagBool, 0x00}
}

// PrefixEnd returns the smallest key that is greater than every key with the
// given prefix, for use as the exclusive end of a range scan. It returns nil
// if no such key exists (the prefix is empty or all 0xFF), meaning
// "unbounded".
func PrefixEnd(prefix []byte) []byte {
	end := make([]byte, len(prefix))
	copy(end, prefix)
	for i := len(end) - 1; i >= 0; i-- {
		if end[i] != 0xFF {
			end[i]++
			return end[:i+1]
		}
	}
	return nil
}
