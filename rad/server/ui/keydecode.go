package ui

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"unicode"

	"github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
)

// keyDecoder renders raw keys and values human-readably, resolving table and
// index IDs to names via a catalog snapshot taken per request.
type keyDecoder struct {
	tables map[string]catalog.Table // by table ID
}

func (s *devtool) newKeyDecoder(ctx context.Context) *keyDecoder {
	dec := &keyDecoder{tables: map[string]catalog.Table{}}
	tables, err := s.tables(ctx)
	if err != nil {
		return dec // degrade to unlabeled decoding
	}
	for _, t := range tables {
		dec.tables[t.ID] = t
	}
	return dec
}

// key renders a raw key like:
//
//	/rad/data/t2[users]/primary/(1)
//	/rad/index/t3[orders]/i6[orders_user_id_idx]/(1)+(100)
func (d *keyDecoder) key(key []byte) string {
	k := string(key)

	if rest, ok := strings.CutPrefix(k, "/rad/data/"); ok {
		tableID, tuple, ok := strings.Cut(rest, "/primary/")
		if ok {
			return "/rad/data/" + d.tableLabel(tableID) + "/primary/" + renderTuple([]byte(tuple))
		}
	}

	if rest, ok := strings.CutPrefix(k, "/rad/index/"); ok {
		if tableID, rest, ok := strings.Cut(rest, "/"); ok {
			if indexID, tuple, ok := strings.Cut(rest, "/"); ok {
				return "/rad/index/" + d.tableLabel(tableID) + "/" + d.indexLabel(tableID, indexID) +
					"/" + d.renderIndexTuple(tableID, indexID, []byte(tuple))
			}
		}
	}

	return printable(key)
}

// value renders a raw value according to what kind of key it sits under.
func (d *keyDecoder) value(key, value []byte) string {
	k := string(key)
	switch {
	case strings.HasPrefix(k, "/rad/index/"):
		// Index values hold the primary key tuple.
		return renderTuple(value)
	case strings.HasPrefix(k, "/rad/data/"):
		// Row values are keyed by column ID; render with names when the
		// table is known.
		if rest, ok := strings.CutPrefix(k, "/rad/data/"); ok {
			if tableID, _, ok := strings.Cut(rest, "/primary/"); ok {
				if tbl, found := d.tables[tableID]; found {
					if s, err := renderRow(tbl, value); err == nil {
						return s
					}
				}
			}
		}
		return printable(value)
	case json.Valid(value) && len(value) > 0 && (value[0] == '{' || value[0] == '['):
		return compactJSON(value)
	default:
		return printable(value)
	}
}

func (d *keyDecoder) tableLabel(tableID string) string {
	if t, ok := d.tables[tableID]; ok {
		return fmt.Sprintf("%s[%s]", tableID, t.Name)
	}
	return tableID
}

func (d *keyDecoder) indexLabel(tableID, indexID string) string {
	if t, ok := d.tables[tableID]; ok {
		for _, idx := range t.Indexes {
			if idx.ID == indexID {
				return fmt.Sprintf("%s[%s]", indexID, idx.Name)
			}
		}
	}
	return indexID
}

// renderIndexTuple splits the concatenated indexed-values + primary-key
// tuples using the index's column count: (indexed)+(pk).
func (d *keyDecoder) renderIndexTuple(tableID, indexID string, buf []byte) string {
	t, ok := d.tables[tableID]
	if !ok {
		return renderTuple(buf)
	}
	var idx *catalog.Index
	for i := range t.Indexes {
		if t.Indexes[i].ID == indexID {
			idx = &t.Indexes[i]
			break
		}
	}
	if idx == nil {
		return renderTuple(buf)
	}

	var indexed []lir.Value
	rest := buf
	for range idx.Columns {
		v, n, err := exec.DecodeValue(rest)
		if err != nil {
			return renderTuple(buf)
		}
		indexed = append(indexed, v)
		rest = rest[n:]
	}
	if len(rest) == 0 {
		return renderValues(indexed)
	}
	return renderValues(indexed) + "+" + renderTuple(rest)
}

// renderRow renders a stored row (column-ID keyed JSON) with column names,
// in table-definition order.
func renderRow(tbl catalog.Table, raw []byte) (string, error) {
	row, err := exec.UnmarshalRow(tbl, raw)
	if err != nil {
		return "", err
	}
	var sb strings.Builder
	sb.WriteString("{")
	for i, col := range tbl.Columns {
		if i > 0 {
			sb.WriteString(", ")
		}
		fmt.Fprintf(&sb, "%s: %s", col.Name, row[col.Name])
	}
	sb.WriteString("}")
	return sb.String(), nil
}

func renderTuple(buf []byte) string {
	if len(buf) == 0 {
		return "()"
	}
	vals, err := exec.DecodeTuple(buf)
	if err != nil {
		return printable(buf)
	}
	return renderValues(vals)
}

func renderValues(vals []lir.Value) string {
	parts := make([]string, len(vals))
	for i, v := range vals {
		parts[i] = v.String()
	}
	return "(" + strings.Join(parts, ", ") + ")"
}

// printable keeps ASCII-printable bytes and hex-escapes the rest.
func printable(b []byte) string {
	var sb strings.Builder
	for _, c := range b {
		if c < unicode.MaxASCII && (unicode.IsPrint(rune(c)) || c == ' ') {
			sb.WriteByte(c)
		} else {
			fmt.Fprintf(&sb, `\x%02x`, c)
		}
	}
	return sb.String()
}

func compactJSON(raw []byte) string {
	var buf bytes.Buffer
	if err := json.Compact(&buf, raw); err != nil {
		return printable(raw)
	}
	return buf.String()
}
