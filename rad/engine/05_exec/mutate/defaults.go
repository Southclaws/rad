package mutate

import (
	"crypto/rand"
	"fmt"
	"maps"
	"time"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/reject"
)

// applyDefaults returns row with catalog defaults filled in for columns the
// caller omitted. Explicit values — including explicit NULLs — win over
// defaults.
func applyDefaults(tbl model.Table, row lir.Row) (lir.Row, error) {
	out := make(lir.Row, len(tbl.Columns))
	maps.Copy(out, row)
	for _, col := range tbl.Columns {
		if _, ok := out[col.Name]; ok || col.InsertDefault == nil {
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

func prepare(tbl model.Table, row lir.Row) (lir.Row, error) {
	withDefaults, err := applyDefaults(tbl, row)
	if err != nil {
		return nil, err
	}
	return normalize(tbl, withDefaults)
}

func normalize(tbl model.Table, row lir.Row) (lir.Row, error) {
	for name := range row {
		if _, ok := tbl.Column(name); !ok {
			return nil, reject.Inputf("exec: table %q has no column %q", tbl.Name, name)
		}
	}
	out := make(lir.Row, len(tbl.Columns))
	for _, col := range tbl.Columns {
		v, ok := row[col.Name]
		if !ok || v.Null {
			if !col.Nullable {
				return nil, reject.Inputf("exec: column %q is not nullable", col.Name)
			}
			out[col.Name] = lir.Null(col.Type)
			continue
		}
		if v.Type != col.Type {
			return nil, reject.Inputf("exec: column %q expects %s, got %s", col.Name, col.Type, v.Type)
		}
		out[col.Name] = v
	}
	return out, nil
}

func defaultValue(col model.Column) (lir.Value, error) {
	d := col.InsertDefault
	switch d.Func {
	case model.DefaultUUID:
		return lir.Text(newUUID()), nil
	case model.DefaultNowMS:
		return lir.Int64(time.Now().UnixMilli()), nil
	case "":
	default:
		return lir.Value{}, reject.Inputf("exec: column %q: unknown default function %q", col.Name, d.Func)
	}
	switch col.Type {
	case model.TypeText:
		return lir.Text(d.Text), nil
	case model.TypeInt64:
		return lir.Int64(d.Int64), nil
	case model.TypeFloat64:
		return lir.Float64(d.Float64), nil
	case model.TypeBool:
		return lir.Bool(d.Bool), nil
	}
	return lir.Value{}, reject.Inputf("exec: column %q: cannot default type %q", col.Name, col.Type)
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
