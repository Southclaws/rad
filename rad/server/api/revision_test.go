package api

import (
	"context"
	"testing"

	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/protocol"
)

func TestInfoReportsDirectSchemaVersion(t *testing.T) {
	c := testServerInMode(t, catalog.ModeDirect)
	ctx := context.Background()

	info, err := c.Info(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode != "direct" || info.SchemaVersion != 0 || info.SchemaVersionAt != nil {
		t.Fatalf("fresh info = %+v", info)
	}

	if _, err := c.TableCreate(ctx, protocol.TableDef{
		Name:       "notes",
		Columns:    []protocol.ColumnDef{{Name: "id", Type: "int64"}},
		PrimaryKey: []string{"id"},
	}); err != nil {
		t.Fatal(err)
	}
	info, err = c.Info(ctx)
	if err != nil || info.SchemaVersion != 1 || info.SchemaVersionAt == nil {
		t.Fatalf("info after table create = %+v, %v", info, err)
	}

	if _, err := c.ColumnCreate(ctx, "notes", protocol.ColumnDef{Name: "body", Type: "text", Nullable: true}); err != nil {
		t.Fatal(err)
	}
	info, err = c.Info(ctx)
	if err != nil || info.SchemaVersion != 2 || info.SchemaVersionAt == nil {
		t.Fatalf("info after column create = %+v, %v", info, err)
	}
}

func TestInfoReportsOneVersionPerSchemaMigration(t *testing.T) {
	c := testServerInMode(t, catalog.ModeSchema)
	ctx := context.Background()
	v1 := `
tables:
  - name: notes
    columns:
      - { name: id, type: int64, pk: true }
  - name: tags
    columns:
      - { name: id, type: int64, pk: true }
`
	v2 := `
tables:
  - name: notes
    columns:
      - { name: id, type: int64, pk: true }
      - { name: body, type: string, nullable: true }
  - name: tags
    columns:
      - { name: id, type: int64, pk: true }
    indexes:
      - { columns: [id] }
`

	steps, err := c.Migrate(ctx, v1)
	if err != nil || len(steps) != 2 {
		t.Fatalf("v1 migration = %v, %v", steps, err)
	}
	info, err := c.Info(ctx)
	if err != nil || info.SchemaVersion != 1 {
		t.Fatalf("v1 info = %+v, %v", info, err)
	}

	steps, err = c.Migrate(ctx, v1)
	if err != nil || len(steps) != 0 {
		t.Fatalf("no-op migration = %v, %v", steps, err)
	}
	info, _ = c.Info(ctx)
	if info.SchemaVersion != 1 {
		t.Fatalf("no-op migration moved schema version to %d", info.SchemaVersion)
	}

	steps, err = c.Migrate(ctx, v2)
	if err != nil || len(steps) != 2 {
		t.Fatalf("v2 migration = %v, %v", steps, err)
	}
	info, err = c.Info(ctx)
	if err != nil || info.SchemaVersion != 2 || info.SchemaVersionAt == nil {
		t.Fatalf("v2 info = %+v, %v", info, err)
	}
}
