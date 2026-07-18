// Package emit turns a failing generative differential case into a permanent,
// human-readable tests/e2e fixture: schema, seed data, a query program, and a
// BUG.md. The generator builds the engine's nested IR (lir.Query); the
// data-driven e2e runner replays the flat wire form, so emission lowers the
// query with api.WireQuery and serialises it alongside the schema and rows.
package emit

import (
	"context"
	"encoding/json"
	"fmt"
	"hash/fnv"
	"os"
	"path/filepath"
	"strings"

	kvslate "github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	generative "github.com/Southclaws/rad/rad/engine/05_exec/generative"
	refexec "github.com/Southclaws/rad/rad/engine/05_exec/refexec"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
	protocol "github.com/Southclaws/rad/rad/protocol"
	pirwire "github.com/Southclaws/rad/rad/protocol/pirwire"
	api "github.com/Southclaws/rad/rad/server/api"
)

// Case is a minimal failing differential case, captured for emission. Spec and
// Data are the shrunk catalog and rows; Query is the failing query. SchemaSrc,
// when set, is the original rad.schema.yaml for a schema-directed case — copied
// verbatim rather than re-serialised from Spec. Mode and Detail describe the
// failure for the BUG.md record.
type Case struct {
	Spec      *generative.Catalog
	Data      map[string][]lir.Row
	Query     lir.Query
	Ordered   bool
	SchemaSrc []byte
	Mode      string
	Detail    string
}

// Fixture writes c as a permanent e2e fixture under dir and returns the fixture
// directory. The expected result is the reference interpreter's — the trusted
// oracle — so the fixture is red against the buggy engine and green once fixed,
// like the hand-authored bug_* fixtures. It rebuilds a database from the case
// to recompute the engine and interpreter results for the record; because
// execution is deterministic, the rebuild reproduces the divergence.
func Fixture(ctx context.Context, dir string, c Case) (string, error) {
	rel, err := protocol.MarshalQuery(api.WireQuery(c.Query))
	if err != nil {
		return "", fmt.Errorf("marshal query: %w", err)
	}
	prog, err := protocol.MarshalProgram(pirwire.Prog("", pirwire.Query("q", rel)))
	if err != nil {
		return "", fmt.Errorf("marshal program: %w", err)
	}

	engineVal, oracleVal, err := reproduce(ctx, c)
	if err != nil {
		return "", err
	}

	h := fnv.New32a()
	_, _ = h.Write(prog)
	name := fmt.Sprintf("bug_gen_%08x", h.Sum32())
	fixDir := filepath.Join(dir, name)
	if err := os.MkdirAll(fixDir, 0o755); err != nil {
		return "", err
	}

	schema := c.SchemaSrc
	if schema == nil {
		schema = []byte(SchemaYAML(c.Spec))
	}
	if err := os.WriteFile(filepath.Join(fixDir, "rad.schema.yaml"), schema, 0o644); err != nil {
		return "", err
	}

	// Omit seed.json entirely when there are no rows — the e2e runner treats an
	// absent seed as an empty start, which reads cleaner than a `null` file.
	if groups := seedGroups(c.Spec, c.Data); len(groups) > 0 {
		seed, err := json.MarshalIndent(groups, "", "  ")
		if err != nil {
			return "", err
		}
		if err := os.WriteFile(filepath.Join(fixDir, "seed.json"), append(seed, '\n'), 0o644); err != nil {
			return "", err
		}
	}

	var progAny any
	_ = json.Unmarshal(prog, &progAny)
	test, err := json.MarshalIndent(map[string]any{
		"program": progAny,
		"result":  frontend.DatumJSON(oracleVal),
	}, "", "  ")
	if err != nil {
		return "", err
	}
	if err := os.WriteFile(filepath.Join(fixDir, "test_"+name+".json"), append(test, '\n'), 0o644); err != nil {
		return "", err
	}

	if err := os.WriteFile(filepath.Join(fixDir, "BUG.md"), []byte(bugMD(c, engineVal, oracleVal)), 0o644); err != nil {
		return "", err
	}
	return fixDir, nil
}

// reproduce rebuilds a database from the case, inserts its rows, and runs the
// query through the engine and the reference interpreter, returning both
// results.
func reproduce(ctx context.Context, c Case) (engine, oracle lir.Datum, err error) {
	store, err := kvslate.Open("emit", "memory:///")
	if err != nil {
		return lir.Datum{}, lir.Datum{}, err
	}
	defer store.Close()
	db := frontend.Open(store)

	if c.SchemaSrc != nil {
		if _, err := db.MigrateFile(ctx, "rad.schema.yaml", c.SchemaSrc); err != nil {
			return lir.Datum{}, lir.Datum{}, fmt.Errorf("migrate: %w", err)
		}
	} else {
		for _, def := range generative.TableDefs(c.Spec) {
			if _, err := db.CreateTable(ctx, def); err != nil {
				return lir.Datum{}, lir.Datum{}, fmt.Errorf("create table %q: %w", def.Name, err)
			}
		}
	}
	for _, tbl := range c.Spec.Tables {
		for _, row := range c.Data[tbl.Name] {
			if err := db.Insert(ctx, tbl.Name, row); err != nil {
				return lir.Datum{}, lir.Datum{}, fmt.Errorf("insert into %q: %w", tbl.Name, err)
			}
		}
	}
	scan := func(_ context.Context, t catalog.Table) ([]lir.Row, error) { return c.Data[t.Name], nil }

	engine, err = db.Execute(ctx, c.Query)
	if err != nil {
		return lir.Datum{}, lir.Datum{}, fmt.Errorf("engine execute: %w", err)
	}
	oracle, err = refexec.InterpretQuery(ctx, db.Catalog(), scan, c.Query)
	if err != nil {
		return lir.Datum{}, lir.Datum{}, fmt.Errorf("interpret: %w", err)
	}
	return engine, oracle, nil
}

// seedGroups renders the rows as e2e seed groups — {table, rows} — in catalog
// order so foreign keys resolve on insert.
func seedGroups(spec *generative.Catalog, data map[string][]lir.Row) []map[string]any {
	var groups []map[string]any
	for _, tbl := range spec.Tables {
		rows := data[tbl.Name]
		if len(rows) == 0 {
			continue
		}
		jsonRows := make([]map[string]any, len(rows))
		for i, r := range rows {
			jsonRows[i] = frontend.RowJSON(r)
		}
		groups = append(groups, map[string]any{"table": tbl.Name, "rows": jsonRows})
	}
	return groups
}

// SchemaYAML serialises a synthetic spec as rad.schema.yaml. It handles the shapes the
// synthesiser produces (single or composite keys, single-column foreign keys
// and indexes); a schema-directed case carries its original source instead.
func SchemaYAML(spec *generative.Catalog) string {
	var b strings.Builder
	b.WriteString("tables:\n")
	for tableIndex, t := range spec.Tables {
		fmt.Fprintf(&b, "  - id: %d\n", tableIndex+1)
		fmt.Fprintf(&b, "    name: %s\n", t.Name)
		b.WriteString("    columns:\n")
		pk := nameSet(t.PrimaryKey)
		refs := fkRefs(t)
		for columnIndex, c := range t.Columns {
			fmt.Fprintf(&b, "      - { id: %d, name: %s, type: %s", columnIndex+1, c.Name, radType(c.Type))
			if len(t.PrimaryKey) == 1 && pk[c.Name] {
				b.WriteString(", pk: true")
			}
			if c.Nullable {
				b.WriteString(", nullable: true")
			}
			if ref, ok := refs[c.Name]; ok {
				fmt.Fprintf(&b, ", ref: %s", ref)
			}
			b.WriteString(" }\n")
		}
		if len(t.PrimaryKey) > 1 {
			fmt.Fprintf(&b, "    primary_key: [%s]\n", strings.Join(t.PrimaryKey, ", "))
		}
		if len(t.Uniques) > 0 || len(t.Indexes) > 0 {
			b.WriteString("    indexes:\n")
			for _, u := range t.Uniques {
				fmt.Fprintf(&b, "      - { columns: [%s], unique: true }\n", strings.Join(u, ", "))
			}
			for _, idx := range t.Indexes {
				fmt.Fprintf(&b, "      - { columns: [%s] }\n", strings.Join(idx, ", "))
			}
		}
	}
	return b.String()
}

// fkRefs maps a single-column foreign key's column to its "parent.column"
// reference, the form a column-level `ref` takes.
func fkRefs(t generative.Table) map[string]string {
	refs := map[string]string{}
	for _, fk := range t.FKs {
		if len(fk.Cols) == 1 && len(fk.ParentCols) == 1 {
			refs[fk.Cols[0]] = fk.Parent + "." + fk.ParentCols[0]
		}
	}
	return refs
}

func radType(t catalog.Type) string {
	switch t {
	case catalog.TypeText:
		return "string"
	case catalog.TypeInt64:
		return "int64"
	case catalog.TypeFloat64:
		return "float64"
	default:
		return "bool"
	}
}

func nameSet(cols []string) map[string]bool {
	s := make(map[string]bool, len(cols))
	for _, c := range cols {
		s[c] = true
	}
	return s
}

func bugMD(c Case, engine, oracle lir.Datum) string {
	eng, _ := json.MarshalIndent(frontend.DatumJSON(engine), "", "  ")
	orc, _ := json.MarshalIndent(frontend.DatumJSON(oracle), "", "  ")
	var b strings.Builder
	fmt.Fprintf(&b, "# Generated regression: %s\n\n", c.Mode)
	fmt.Fprintf(&b, "The generative differential found a divergence and shrank it to this case.\n\n")
	fmt.Fprintf(&b, "Divergence: %s\n\n", c.Detail)
	fmt.Fprintf(&b, "## Engine result\n\n```json\n%s\n```\n\n", eng)
	fmt.Fprintf(&b, "## Reference interpreter result (expected in the fixture)\n\n```json\n%s\n```\n\n", orc)
	b.WriteString("The interpreter is trusted by construction but not infallible: the bug is ")
	b.WriteString("*presumed* in the engine, but confirm which side is wrong before trusting the ")
	b.WriteString("pinned expectation. Both results are shown above so review is one glance.\n")
	return b.String()
}
