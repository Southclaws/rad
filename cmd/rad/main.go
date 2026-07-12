// rad is the Rad command-line tool: database server, devtool, and codegen
// in one binary. The database engine lives under ./rad/engine and the HTTP
// implementation under ./rad/server; this package is only the CLI shell.
//
//	rad migrate  -d ./data -f schema.rad     reconcile the database with a schema
//	rad generate -f schema.rad -o ./gen      emit the typed Go client
//	rad serve    -d ./data                   devtool server (REST API + web UI)
package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/spf13/cobra"

	radclient "github.com/Southclaws/rad/rad/client"
	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
)

// version is stamped by the release build via -ldflags "-X main.version=…".
var version = "dev"

func main() {
	// Keep our own command order (workflow, not alphabetical).
	cobra.EnableCommandSorting = false

	root := &cobra.Command{
		Use:           "rad",
		Short:         "Rad — an ORM-native relational database on an ordered KV store",
		Version:       version,
		SilenceUsage:  true,
		SilenceErrors: false,
	}
	root.CompletionOptions.HiddenDefaultCmd = true
	root.AddCommand(serveCmd(), validateCmd(), migrateCmd(), generateCmd())

	// Bare `rad` prints the splash; keep the default help for subcommands.
	root.Run = func(cmd *cobra.Command, args []string) { fmt.Print(splash(root)) }
	defaultHelp := root.HelpFunc()
	root.SetHelpFunc(func(cmd *cobra.Command, args []string) {
		if cmd == root {
			fmt.Print(splash(root))
			return
		}
		defaultHelp(cmd, args)
	})

	if err := root.Execute(); err != nil {
		os.Exit(1)
	}
}

// splash is the bare-`rad` command menu: a prompt line, then each command with
// its one-line summary, in registration order.
func splash(root *cobra.Command) string {
	var b strings.Builder
	fmt.Fprintf(&b, "\n%s %s  %s\n\n", cGreen("$"), cBold(cGreen("rad")), cDim(version))
	for _, c := range root.Commands() {
		if !c.IsAvailableCommand() || c.Name() == "help" {
			continue
		}
		fmt.Fprintf(&b, "  %s %s\n", cGreen(fmt.Sprintf("%-10s", c.Name())), cDim(c.Short))
	}
	fmt.Fprintf(&b, "\n  %s\n", cDim("run 'rad <command> --help' for details"))
	return b.String()
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
	var db, file, url string
	cmd := &cobra.Command{
		Use:   "migrate",
		Short: "Apply schema.rad changes to a database (diff + reconcile)",
		Long: `Apply schema.rad changes to a database.

With --url (or RAD_URL), migrates a running Rad server over the wire — the
normal mode, since a live server owns its store exclusively. With --db, opens
a local store directory directly (the server must not be running on it).`,
		Args: cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			src, err := os.ReadFile(file)
			if err != nil {
				return err
			}
			if url == "" {
				url = os.Getenv("RAD_URL")
			}

			var steps []string
			var target string
			if url != "" {
				c, err := radclient.Dial(url)
				if err != nil {
					return err
				}
				steps, err = c.Migrate(cmd.Context(), string(src))
				if err != nil {
					return err
				}
				target = url
			} else {
				store, dir, err := openStore(db)
				if err != nil {
					return err
				}
				defer store.Close()
				applied, err := frontend.Open(store).MigrateFile(cmd.Context(), file, src)
				if err != nil {
					return err
				}
				for _, s := range applied {
					steps = append(steps, s.String())
				}
				target = dir
			}

			if len(steps) == 0 {
				fmt.Printf("%s: database at %s is up to date\n", file, target)
				return nil
			}
			fmt.Printf("%s: applied %d steps to %s:\n", file, len(steps), target)
			for _, s := range steps {
				fmt.Printf("  • %s\n", s)
			}
			return nil
		},
	}
	cmd.Flags().StringVarP(&url, "url", "u", "", "Rad server URL, e.g. rad://localhost (default RAD_URL; falls back to --db)")
	cmd.Flags().StringVarP(&db, "db", "d", "data", "local store directory (offline mode)")
	cmd.Flags().StringVarP(&file, "file", "f", "schema.rad", "schema file")
	return cmd
}
