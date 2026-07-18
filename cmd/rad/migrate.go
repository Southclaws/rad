package main

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"

	radclient "github.com/Southclaws/rad/rad/client"
)

const defaultMigrateURL = "rad://localhost"

func migrateCmd() *cobra.Command {
	var file, url string
	cmd := &cobra.Command{
		Use:   "migrate",
		Short: "Apply schema.rad changes to a database (diff + reconcile)",
		Long: `Apply schema.rad changes to a running Rad server.

The schema file is read locally and sent to the server over HTTP using a rad:// URL.
Only rad serve opens database storage; migrate never accesses SlateDB directly.`,
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			src, err := os.ReadFile(file)
			if err != nil {
				return err
			}

			target := migrateURL(url)
			client, err := radclient.Dial(target)
			if err != nil {
				return err
			}
			steps, err := client.Migrate(cmd.Context(), string(src))
			if err != nil {
				return err
			}

			if len(steps) == 0 {
				fmt.Fprintf(cmd.OutOrStdout(), "%s: database at %s is up to date\n", file, target)
				return nil
			}
			fmt.Fprintf(cmd.OutOrStdout(), "%s: applied %d steps to %s:\n", file, len(steps), target)
			for _, step := range steps {
				fmt.Fprintf(cmd.OutOrStdout(), "  • %s\n", step)
			}
			return nil
		},
	}
	cmd.Flags().StringVarP(&url, "url", "u", "", "Rad server URL (default RAD_URL or rad://localhost)")
	cmd.Flags().StringVarP(&file, "file", "f", "schema.rad", "schema file")
	return cmd
}

func migrateURL(flag string) string {
	if flag != "" {
		return flag
	}
	if env := os.Getenv("RAD_URL"); env != "" {
		return env
	}
	return defaultMigrateURL
}
