package main

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"

	radclient "github.com/Southclaws/rad/rad/client"
)

func migrateCmd() *cobra.Command {
	var file, configFile string
	cmd := &cobra.Command{
		Use:   "migrate",
		Short: "Apply rad.schema.yaml changes to a database (diff + reconcile)",
		Long: `Apply rad.schema.yaml changes to a running Rad server.

The schema and rad.config.yaml files are read locally. The config's database_url
selects the server; only rad serve opens database storage, and migrate never
accesses SlateDB directly.`,
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			config, err := loadProjectConfig(configFile)
			if err != nil {
				return err
			}
			src, err := os.ReadFile(file)
			if err != nil {
				return err
			}

			client, err := radclient.Dial(config.DatabaseURL)
			if err != nil {
				return err
			}
			steps, err := client.Migrate(cmd.Context(), string(src))
			if err != nil {
				return err
			}

			if len(steps) == 0 {
				fmt.Fprintf(cmd.OutOrStdout(), "%s: database at %s is up to date\n", file, config.DatabaseURL)
				return nil
			}
			fmt.Fprintf(cmd.OutOrStdout(), "%s: applied %d steps to %s:\n", file, len(steps), config.DatabaseURL)
			for _, step := range steps {
				fmt.Fprintf(cmd.OutOrStdout(), "  • %s\n", step)
			}
			return nil
		},
	}
	cmd.Flags().StringVarP(&file, "file", "f", defaultSchemaFile, "schema file")
	cmd.Flags().StringVar(&configFile, "config", defaultConfigFile, "project config file")
	return cmd
}
