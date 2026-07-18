// rad is the Rad command-line tool: database server, devtool, and codegen
// in one binary. The database engine lives under ./rad/engine and the HTTP
// implementation under ./rad/server; this package is only the CLI shell.
//
//	rad migrate                         reconcile the configured database
//	rad generate -o ./gen               emit the typed Go client
//	rad serve -d ./data                 devtool server (REST API + web UI)
package main

import (
	"fmt"
	"os"
	"strings"

	"github.com/spf13/cobra"
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
