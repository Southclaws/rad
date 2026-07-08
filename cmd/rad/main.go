// rad is the RAD command-line tool: database server, devtool, and codegen
// in one binary. The database engine itself lives under ./rad; this package
// is only the CLI shell around it.
//
//	rad migrate  -d ./data -f schema.rad     reconcile the database with a schema
//	rad generate -f schema.rad -o ./gen      emit the typed Go client
//	rad serve    -d ./data                   devtool server (REST API + web UI)
package main

import (
	"fmt"
	"os"
	"path/filepath"

	"github.com/spf13/cobra"

	"rad/rad/01_kv/kvslate"
	frontend "rad/rad/06_frontend"
)

func main() {
	root := &cobra.Command{
		Use:           "rad",
		Short:         "RAD — an ORM-native relational database on an ordered KV store",
		SilenceUsage:  true,
		SilenceErrors: false,
	}
	root.AddCommand(migrateCmd(), generateCmd(), serveCmd())
	if err := root.Execute(); err != nil {
		os.Exit(1)
	}
}

// openStore opens (creating if needed) the SlateDB store at dir.
func openStore(db string) (*kvslate.Store, string, error) {
	dir, err := filepath.Abs(db)
	if err != nil {
		return nil, "", err
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return nil, "", err
	}
	store, err := kvslate.Open(dir, "file://")
	if err != nil {
		return nil, "", fmt.Errorf("open slatedb at %s: %w", dir, err)
	}
	return store, dir, nil
}

func migrateCmd() *cobra.Command {
	var db, file string
	cmd := &cobra.Command{
		Use:   "migrate",
		Short: "Apply schema.rad changes to the database (diff + reconcile)",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			src, err := os.ReadFile(file)
			if err != nil {
				return err
			}
			store, dir, err := openStore(db)
			if err != nil {
				return err
			}
			defer store.Close()

			steps, err := frontend.Open(store).MigrateFile(cmd.Context(), file, src)
			if err != nil {
				return err
			}
			if len(steps) == 0 {
				fmt.Printf("%s: database at %s is up to date\n", file, dir)
				return nil
			}
			fmt.Printf("%s: applied %d steps to %s:\n", file, len(steps), dir)
			for _, s := range steps {
				fmt.Printf("  • %s\n", s)
			}
			return nil
		},
	}
	cmd.Flags().StringVarP(&db, "db", "d", "data", "path to the SlateDB store directory")
	cmd.Flags().StringVarP(&file, "file", "f", "schema.rad", "schema file")
	return cmd
}
