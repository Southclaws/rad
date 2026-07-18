package radclient

// The imperative catalog operations: create/update/delete for tables,
// columns, and indexes. These work only against a directly managed database;
// a schema-managed server rejects them with an invalid problem (use SchemaMigrate
// there instead). Mutations returning the updated table reflect the state
// after the change, including catalog-resolved details such as index and
// foreign key definitions.

import (
	"context"
	"fmt"
	"math"

	"github.com/Southclaws/rad/rad/api"
	"github.com/Southclaws/rad/rad/api/oas"
	"github.com/Southclaws/rad/rad/protocol"
)

// Mode reports the database's catalog management mode: "direct" (the
// catalog is mutable over this API) or "schema" (rad.schema.yaml migrations own
// the catalog).
func (c *Client) Mode(ctx context.Context) (string, error) {
	info, err := c.Info(ctx)
	return info.Mode, err
}

// Info reports the database's catalog mode and current schema version.
func (c *Client) Info(ctx context.Context) (protocol.DatabaseInfo, error) {
	res, err := c.oas.GetInfo(ctx)
	if err != nil {
		return protocol.DatabaseInfo{}, transportError(err)
	}
	info := protocol.DatabaseInfo{
		Mode:          string(res.Mode),
		SchemaVersion: uint64(res.SchemaVersion),
		SchemaHash:    res.SchemaHash,
	}
	if value, ok := res.SchemaVersionAt.Get(); ok {
		info.SchemaVersionAt = &value
	}
	return info, nil
}

// Schema returns the server's authoritative accepted schema and identity.
func (c *Client) Schema(ctx context.Context) (protocol.SchemaState, error) {
	res, err := c.oas.GetSchema(ctx)
	if err != nil {
		return protocol.SchemaState{}, transportError(err)
	}
	return api.SchemaStateFromOAS(*res), nil
}

// SchemaDiff asks the server to compute and preflight the complete catalog
// PIR transition without applying it.
func (c *Client) SchemaDiff(ctx context.Context, schemaSource string) (protocol.SchemaDiff, error) {
	res, err := c.oas.SchemaDiff(ctx, oas.NewOptSchemaRequest(oas.SchemaRequest{Schema: schemaSource}))
	if err != nil {
		return protocol.SchemaDiff{}, transportError(err)
	}
	switch value := res.(type) {
	case *oas.SchemaDiffResult:
		return protocol.SchemaDiff{
			CurrentVersion: uint64(value.CurrentVersion), CurrentHash: value.CurrentHash,
			DesiredHash: value.DesiredHash, Changes: api.SchemaChangesFromOAS(value.Changes),
			Program:     decodeRawValue(value.Program),
			Destructive: api.SchemaFindingsFromOAS(value.Destructive),
			Blocking:    api.SchemaFindingsFromOAS(value.Blocking),
		}, nil
	case *oas.Problem:
		return protocol.SchemaDiff{}, apiError(*value)
	default:
		return protocol.SchemaDiff{}, fmt.Errorf("rad: unexpected schema diff response %T", res)
	}
}

// SchemaMigrate applies a desired schema against the exact server identity
// returned by SchemaDiff, after any required data-loss consent is established.
func (c *Client) SchemaMigrate(
	ctx context.Context,
	schemaSource string,
	expected protocol.SchemaIdentity,
	acceptDataLoss bool,
) (protocol.SchemaMigration, error) {
	if expected.SchemaVersion > math.MaxInt64 {
		return protocol.SchemaMigration{}, fmt.Errorf("rad: schema version %d exceeds the wire format", expected.SchemaVersion)
	}
	request := oas.SchemaMigrateRequest{
		Schema: schemaSource, CurrentVersion: int64(expected.SchemaVersion), CurrentHash: expected.SchemaHash,
	}
	if acceptDataLoss {
		request.AcceptDataLoss = oas.NewOptBool(true)
	}
	res, err := c.oas.SchemaMigrate(ctx, oas.NewOptSchemaMigrateRequest(request))
	if err != nil {
		return protocol.SchemaMigration{}, transportError(err)
	}
	switch value := res.(type) {
	case *oas.SchemaMigrateResult:
		return protocol.SchemaMigration{
			SchemaState: protocol.SchemaState{
				SchemaVersion: uint64(value.SchemaVersion), SchemaHash: value.SchemaHash,
				Schema: api.SchemaDocumentFromOAS(value.Schema),
			},
			Changes: api.SchemaChangesFromOAS(value.Changes),
		}, nil
	case *oas.SchemaMigrateConflict:
		return protocol.SchemaMigration{}, apiError(oas.Problem(*value))
	case *oas.SchemaMigrateUnprocessableEntity:
		return protocol.SchemaMigration{}, apiError(oas.Problem(*value))
	default:
		return protocol.SchemaMigration{}, fmt.Errorf("rad: unexpected schema migrate response %T", res)
	}
}

// CheckSchema asks the server to compare an exact generated-client identity
// with its committed catalog identity.
func (c *Client) CheckSchema(ctx context.Context, version uint64, hash string) error {
	res, err := c.oas.SchemaCompatibility(ctx, oas.NewOptSchemaCompatibilityRequest(
		oas.SchemaCompatibilityRequest{SchemaVersion: int64(version), SchemaHash: hash},
	))
	if err != nil {
		return transportError(err)
	}
	switch value := res.(type) {
	case *oas.NoContent:
		return nil
	case *oas.Problem:
		return apiError(*value)
	default:
		return fmt.Errorf("rad: unexpected schema compatibility response %T", res)
	}
}

// TableCreate creates a table: columns, primary key, and optionally indexes
// and foreign keys, in one atomic call.
func (c *Client) TableCreate(ctx context.Context, def protocol.TableDef) (protocol.TableInfo, error) {
	res, err := c.oas.TableCreate(ctx, oas.NewOptTableDef(api.TableDefToOAS(def)))
	if err != nil {
		return protocol.TableInfo{}, transportError(err)
	}
	switch v := res.(type) {
	case *oas.TableInfo:
		return api.TableFromOAS(*v), nil
	case *oas.TableCreateConflict:
		return protocol.TableInfo{}, apiError(oas.Problem(*v))
	case *oas.TableCreateUnprocessableEntity:
		return protocol.TableInfo{}, apiError(oas.Problem(*v))
	default:
		return protocol.TableInfo{}, fmt.Errorf("rad: unexpected catalog response %T", res)
	}
}

// TableDelete removes a table. Tables referenced by other tables' foreign
// keys must have their referencing tables deleted first.
func (c *Client) TableDelete(ctx context.Context, table string) error {
	res, err := c.oas.TableDelete(ctx, oas.TableDeleteParams{Table: table})
	if err != nil {
		return transportError(err)
	}
	switch v := res.(type) {
	case *oas.NoContent:
		return nil
	case *oas.TableDeleteConflict:
		return apiError(oas.Problem(*v))
	case *oas.TableDeleteUnprocessableEntity:
		return apiError(oas.Problem(*v))
	default:
		return fmt.Errorf("rad: unexpected catalog response %T", res)
	}
}

// TableUpdate updates a table's properties; the only updatable property
// today is its name.
func (c *Client) TableUpdate(ctx context.Context, table, name string) (protocol.TableInfo, error) {
	res, err := c.oas.TableUpdate(ctx,
		oas.NewOptTableUpdateProps(oas.TableUpdateProps{Name: name}),
		oas.TableUpdateParams{Table: table})
	if err != nil {
		return protocol.TableInfo{}, transportError(err)
	}
	switch v := res.(type) {
	case *oas.TableInfo:
		return api.TableFromOAS(*v), nil
	case *oas.TableUpdateConflict:
		return protocol.TableInfo{}, apiError(oas.Problem(*v))
	case *oas.TableUpdateUnprocessableEntity:
		return protocol.TableInfo{}, apiError(oas.Problem(*v))
	default:
		return protocol.TableInfo{}, fmt.Errorf("rad: unexpected catalog response %T", res)
	}
}

// ColumnCreate appends a column to a table. The column must be nullable or
// carry a literal default, since existing rows need a value.
func (c *Client) ColumnCreate(ctx context.Context, table string, col protocol.ColumnDef) (protocol.TableInfo, error) {
	res, err := c.oas.ColumnCreate(ctx,
		oas.NewOptColumnDef(api.ColumnDefToOAS(col)),
		oas.ColumnCreateParams{Table: table})
	if err != nil {
		return protocol.TableInfo{}, transportError(err)
	}
	switch v := res.(type) {
	case *oas.TableInfo:
		return api.TableFromOAS(*v), nil
	case *oas.ColumnCreateConflict:
		return protocol.TableInfo{}, apiError(oas.Problem(*v))
	case *oas.ColumnCreateUnprocessableEntity:
		return protocol.TableInfo{}, apiError(oas.Problem(*v))
	default:
		return protocol.TableInfo{}, fmt.Errorf("rad: unexpected catalog response %T", res)
	}
}

// ColumnDelete removes a column not used by the primary key, an index, or a
// foreign key.
func (c *Client) ColumnDelete(ctx context.Context, table, column string) (protocol.TableInfo, error) {
	res, err := c.oas.ColumnDelete(ctx, oas.ColumnDeleteParams{Table: table, Column: column})
	if err != nil {
		return protocol.TableInfo{}, transportError(err)
	}
	switch v := res.(type) {
	case *oas.TableInfo:
		return api.TableFromOAS(*v), nil
	case *oas.ColumnDeleteConflict:
		return protocol.TableInfo{}, apiError(oas.Problem(*v))
	case *oas.ColumnDeleteUnprocessableEntity:
		return protocol.TableInfo{}, apiError(oas.Problem(*v))
	default:
		return protocol.TableInfo{}, fmt.Errorf("rad: unexpected catalog response %T", res)
	}
}

// ColumnUpdate updates a column's properties; the only updatable property
// today is its name.
func (c *Client) ColumnUpdate(ctx context.Context, table, column, name string) (protocol.TableInfo, error) {
	res, err := c.oas.ColumnUpdate(ctx,
		oas.NewOptColumnUpdateProps(oas.ColumnUpdateProps{Name: name}),
		oas.ColumnUpdateParams{Table: table, Column: column})
	if err != nil {
		return protocol.TableInfo{}, transportError(err)
	}
	switch v := res.(type) {
	case *oas.TableInfo:
		return api.TableFromOAS(*v), nil
	case *oas.ColumnUpdateConflict:
		return protocol.TableInfo{}, apiError(oas.Problem(*v))
	case *oas.ColumnUpdateUnprocessableEntity:
		return protocol.TableInfo{}, apiError(oas.Problem(*v))
	default:
		return protocol.TableInfo{}, fmt.Errorf("rad: unexpected catalog response %T", res)
	}
}

// IndexCreate registers an index and backfills entries for existing rows
// atomically; backfilling a unique index over duplicate data fails with a
// conflict and nothing is registered.
func (c *Client) IndexCreate(ctx context.Context, table string, idx protocol.IndexDef) (protocol.TableInfo, error) {
	res, err := c.oas.IndexCreate(ctx,
		oas.NewOptIndexInfo(api.IndexToOAS(idx)),
		oas.IndexCreateParams{Table: table})
	if err != nil {
		return protocol.TableInfo{}, transportError(err)
	}
	switch v := res.(type) {
	case *oas.TableInfo:
		return api.TableFromOAS(*v), nil
	case *oas.IndexCreateConflict:
		return protocol.TableInfo{}, apiError(oas.Problem(*v))
	case *oas.IndexCreateUnprocessableEntity:
		return protocol.TableInfo{}, apiError(oas.Problem(*v))
	default:
		return protocol.TableInfo{}, fmt.Errorf("rad: unexpected catalog response %T", res)
	}
}

// IndexDelete removes an index; queries fall back to other access paths.
func (c *Client) IndexDelete(ctx context.Context, table, index string) (protocol.TableInfo, error) {
	res, err := c.oas.IndexDelete(ctx, oas.IndexDeleteParams{Table: table, Index: index})
	if err != nil {
		return protocol.TableInfo{}, transportError(err)
	}
	switch v := res.(type) {
	case *oas.TableInfo:
		return api.TableFromOAS(*v), nil
	case *oas.IndexDeleteConflict:
		return protocol.TableInfo{}, apiError(oas.Problem(*v))
	case *oas.IndexDeleteUnprocessableEntity:
		return protocol.TableInfo{}, apiError(oas.Problem(*v))
	default:
		return protocol.TableInfo{}, fmt.Errorf("rad: unexpected catalog response %T", res)
	}
}
