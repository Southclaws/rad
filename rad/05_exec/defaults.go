package exec

import (
	"crypto/rand"
	"fmt"
	"maps"
	"time"

	catalog "rad/rad/02_catalog"
	lir "rad/rad/03_lir"
)

// applyDefaults returns row with catalog defaults filled in for columns the
// caller omitted. Explicit values — including explicit NULLs — win over
// defaults.
func applyDefaults(tbl catalog.Table, row lir.Row) (lir.Row, error) {
	out := make(lir.Row, len(tbl.Columns))
	maps.Copy(out, row)
	for _, col := range tbl.Columns {
		if _, ok := out[col.Name]; ok || col.Default == nil {
			continue
		}
		v, err := defaultValue(col)
		if err != nil {
			return nil, err
		}
		out[col.Name] = v
	}
	return out, nil
}

func defaultValue(col catalog.Column) (lir.Value, error) {
	d := col.Default
	switch d.Func {
	case catalog.DefaultUUID:
		return lir.Text(newUUID()), nil
	case catalog.DefaultNowMS:
		return lir.Int64(time.Now().UnixMilli()), nil
	case "":
	default:
		return lir.Value{}, fmt.Errorf("exec: column %q: unknown default function %q", col.Name, d.Func)
	}
	switch col.Type {
	case catalog.TypeText:
		return lir.Text(d.Text), nil
	case catalog.TypeInt64:
		return lir.Int64(d.Int64), nil
	case catalog.TypeFloat64:
		return lir.Float64(d.Float64), nil
	case catalog.TypeBool:
		return lir.Bool(d.Bool), nil
	}
	return lir.Value{}, fmt.Errorf("exec: column %q: cannot default type %q", col.Name, col.Type)
}

// newUUID returns a random RFC 4122 version-4 UUID string.
func newUUID() string {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		panic(err) // crypto/rand does not fail on supported platforms
	}
	b[6] = (b[6] & 0x0f) | 0x40
	b[8] = (b[8] & 0x3f) | 0x80
	return fmt.Sprintf("%x-%x-%x-%x-%x", b[0:4], b[4:6], b[6:8], b[8:10], b[10:16])
}
