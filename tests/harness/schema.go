package harness

import "github.com/Southclaws/rad/rad/protocol"

// SchemaPlan asks the running server to plan and preflight source without
// changing the database.
func (d *DB) SchemaPlan(source string) protocol.SchemaDiff {
	d.T.Helper()
	diff, err := d.Client.SchemaDiff(d.T.Context(), source)
	if err != nil {
		d.T.Fatalf("harness: plan schema migration: %v", err)
	}
	return diff
}

// SchemaApply submits a previously observed schema identity. Errors are
// returned so battle tests can assert conflicts and data blockers.
func (d *DB) SchemaApply(
	source string,
	diff protocol.SchemaDiff,
	acceptDataLoss bool,
) (protocol.SchemaMigration, error) {
	d.T.Helper()
	return d.Client.SchemaMigrate(d.T.Context(), source, protocol.SchemaIdentity{
		SchemaVersion: diff.CurrentVersion,
		SchemaHash:    diff.CurrentHash,
	}, acceptDataLoss)
}

// SchemaMigrate plans and applies source, failing the current test on error.
// Online work may still be converging when this method returns.
func (d *DB) SchemaMigrate(source string, acceptDataLoss bool) protocol.SchemaMigration {
	d.T.Helper()
	diff := d.SchemaPlan(source)
	migration, err := d.SchemaApply(source, diff, acceptDataLoss)
	if err != nil {
		d.T.Fatalf("harness: apply schema migration: %v", err)
	}
	return migration
}

// SchemaMigrateReady plans, applies, and waits until the server's canonical
// schema reaches source's desired hash.
func (d *DB) SchemaMigrateReady(source string, acceptDataLoss bool) protocol.SchemaMigration {
	d.T.Helper()
	migration := d.SchemaMigrate(source, acceptDataLoss)
	ready, err := d.Client.WaitSchemaMigration(d.T.Context(), migration)
	if err != nil {
		d.T.Fatalf("harness: wait for schema migration: %v", err)
	}
	if ready.State != protocol.SchemaMigrationReady || ready.SchemaHash != ready.DesiredHash {
		d.T.Fatalf("harness: migration did not reach desired schema: %#v", ready)
	}
	return ready
}
