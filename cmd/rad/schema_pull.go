package main

import (
	"errors"
	"fmt"
	"os"
	"time"

	"github.com/spf13/cobra"

	projectstate "github.com/Southclaws/rad/rad/state"
)

func schemaPullCmd(options *schemaOptions) *cobra.Command {
	var force, noGenerate bool
	cmd := &cobra.Command{
		Use:   "pull",
		Short: "Recover the accepted schema from the database",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			project, client, err := openSchemaProject(options)
			if err != nil {
				return err
			}
			store := projectstate.New(project.StateDir)
			server, err := client.Schema(cmd.Context())
			if err != nil {
				return err
			}
			modified, localVersion, localHash, err := localSchemaModified(project, store, server.SchemaHash)
			if err != nil {
				return err
			}
			if modified && !force {
				return fmt.Errorf("%s contains local schema changes\n\nLocal base:\n  version %d\n  hash %s\n\nServer:\n  version %d\n  hash %s\n\nPull would overwrite the local schema file.\nUse `rad schema pull --force` to replace it", project.SchemaFile, localVersion, localHash, server.SchemaVersion, server.SchemaHash)
			}
			if modified && force {
				backup, err := store.BackupDesired(project.SchemaFile, time.Now())
				if err != nil {
					return err
				}
				fmt.Fprintf(cmd.OutOrStdout(), "Local schema backed up:\n  %s\n", backup)
			}
			accepted, err := store.WriteSnapshot(server)
			if err != nil {
				return err
			}
			if err := store.WriteDesired(project.SchemaFile, accepted.Source); err != nil {
				return fmt.Errorf("server schema snapshot was recorded, but %s could not be updated: %w", project.SchemaFile, err)
			}
			if err := store.WriteLock(accepted.Lock); err != nil {
				return fmt.Errorf("server schema and desired schema were written, but lockfile update failed: %w", err)
			}
			fmt.Fprintf(cmd.OutOrStdout(), "Pulled schema version %d.\n", server.SchemaVersion)
			fmt.Fprintf(cmd.OutOrStdout(), "Updated:\n  %s\n  %s\n  %s\n", project.SchemaFile, store.LockPath(), store.SnapshotPath(server.SchemaVersion))
			if noGenerate {
				return nil
			}
			if err := generateProject(cmd, project, accepted); err != nil {
				return fmt.Errorf("schema version %d was pulled and local accepted state was updated, but client generation failed: %w\nrun `rad generate` after fixing the generation error", server.SchemaVersion, err)
			}
			return nil
		},
	}
	cmd.Flags().BoolVar(&force, "force", false, "back up and replace a locally modified schema")
	cmd.Flags().BoolVar(&noGenerate, "no-generate", false, "do not regenerate configured clients")
	return cmd
}

func localSchemaModified(project project, store projectstate.Store, serverHash string) (bool, uint64, string, error) {
	desired, err := os.ReadFile(project.SchemaFile)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return false, 0, "missing", nil
		}
		return false, 0, "", err
	}
	accepted, err := store.Load()
	if err == nil {
		matches, _, matchErr := projectstate.MatchesAccepted(project.SchemaFile, desired, accepted)
		return !matches, accepted.Lock.SchemaVersion, accepted.Lock.SchemaHash, matchErr
	}
	if !errors.Is(err, os.ErrNotExist) {
		return false, 0, "", err
	}
	_, desiredHash, parseErr := projectstate.ParseSnapshot(project.SchemaFile, desired)
	if parseErr != nil {
		return true, 0, "unavailable", nil
	}
	return desiredHash != serverHash, 0, "missing", nil
}
