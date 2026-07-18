package api

import (
	"context"

	radclient "github.com/Southclaws/rad/rad/client"
	"github.com/Southclaws/rad/rad/protocol"
)

func migrateSchema(ctx context.Context, client *radclient.Client, source string) (protocol.SchemaMigration, error) {
	diff, err := client.SchemaDiff(ctx, source)
	if err != nil {
		return protocol.SchemaMigration{}, err
	}
	return client.SchemaMigrate(ctx, source, protocol.SchemaIdentity{
		SchemaVersion: diff.CurrentVersion, SchemaHash: diff.CurrentHash,
	}, false)
}
