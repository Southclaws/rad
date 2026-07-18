package main

import (
	"bufio"
	"fmt"
	"strings"

	"github.com/spf13/cobra"

	"github.com/Southclaws/rad/rad/protocol"
	projectstate "github.com/Southclaws/rad/rad/state"
)

func schemaMigrateCmd(options *schemaOptions) *cobra.Command {
	var acceptDataLoss, noGenerate bool
	cmd := &cobra.Command{
		Use:   "migrate",
		Short: "Apply rad.schema.yaml transactionally",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			project, client, err := openSchemaProject(options)
			if err != nil {
				return err
			}
			source, err := readDesired(project)
			if err != nil {
				return err
			}
			diff, err := client.SchemaDiff(cmd.Context(), string(source))
			if err != nil {
				return err
			}
			if len(diff.Blocking) > 0 {
				fmt.Fprintln(cmd.OutOrStdout(), "Migration cannot be applied.")
				printFindings(cmd, "Cannot apply", diff.Blocking)
				return fmt.Errorf("target schema constraints are not satisfied")
			}
			if len(diff.Destructive) > 0 && !acceptDataLoss {
				confirmed, err := confirmDataLoss(cmd, diff.Destructive)
				if err != nil {
					return err
				}
				if !confirmed {
					return fmt.Errorf("migration cancelled")
				}
				acceptDataLoss = true
			}
			if len(diff.Changes) > 0 {
				fmt.Fprintln(cmd.OutOrStdout(), "Applying schema changes...")
			}
			migration, err := client.SchemaMigrate(cmd.Context(), string(source), protocol.SchemaIdentity{
				SchemaVersion: diff.CurrentVersion, SchemaHash: diff.CurrentHash,
			}, acceptDataLoss)
			if err != nil {
				return err
			}

			accepted, err := projectstate.New(project.StateDir).WriteAccepted(migration.SchemaState)
			if err != nil {
				return fmt.Errorf("schema version %d committed to the database, but local accepted state could not be updated: %w", migration.SchemaVersion, err)
			}
			fmt.Fprintf(cmd.OutOrStdout(), "Schema version %d committed.\n", migration.SchemaVersion)
			fmt.Fprintf(cmd.OutOrStdout(), "Snapshot written:\n  %s\n", projectstate.New(project.StateDir).SnapshotPath(migration.SchemaVersion))
			fmt.Fprintf(cmd.OutOrStdout(), "Lockfile updated:\n  %s\n", projectstate.New(project.StateDir).LockPath())
			if noGenerate {
				return nil
			}
			if err := generateProject(cmd, project, accepted); err != nil {
				return fmt.Errorf("schema version %d was committed and local accepted state was updated, but client generation failed: %w\nrun `rad generate` after fixing the generation error", migration.SchemaVersion, err)
			}
			return nil
		},
	}
	cmd.Flags().BoolVar(&acceptDataLoss, "accept-data-loss", false, "permit explicitly reported destructive changes")
	cmd.Flags().BoolVar(&noGenerate, "no-generate", false, "do not regenerate configured clients")
	return cmd
}

func confirmDataLoss(cmd *cobra.Command, findings []protocol.SchemaFinding) (bool, error) {
	out := cmd.OutOrStdout()
	fmt.Fprintln(out, "WARNING: This migration will permanently delete data.")
	for _, finding := range findings {
		fmt.Fprintf(out, "\n- %s\n", finding.Summary)
	}
	fmt.Fprint(out, "\nContinue? [y/N] ")
	line, err := bufio.NewReader(cmd.InOrStdin()).ReadString('\n')
	if err != nil && len(line) == 0 {
		return false, nil
	}
	answer := strings.ToLower(strings.TrimSpace(line))
	return answer == "y" || answer == "yes", nil
}
