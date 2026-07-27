package main

import (
	"encoding/json"
	"fmt"

	"github.com/spf13/cobra"

	"github.com/Southclaws/rad/rad/protocol"
)

func schemaDiffCmd(options *schemaOptions) *cobra.Command {
	var format string
	cmd := &cobra.Command{
		Use:   "diff",
		Short: "Show desired schema changes and data preflight findings",
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
			switch format {
			case "text":
				printSchemaDiff(cmd, diff)
			case "json":
				encoded, err := json.MarshalIndent(diff, "", "  ")
				if err != nil {
					return err
				}
				fmt.Fprintln(cmd.OutOrStdout(), string(encoded))
			default:
				return fmt.Errorf("unsupported format %q (use text or json)", format)
			}
			return nil
		},
	}
	cmd.Flags().StringVar(&format, "format", "text", "output format: text or json")
	return cmd
}

func printSchemaDiff(cmd *cobra.Command, diff protocol.SchemaDiff) {
	out := cmd.OutOrStdout()
	if len(diff.Changes) == 0 {
		fmt.Fprintln(out, "No schema changes.")
	} else {
		fmt.Fprintln(out, "Schema changes")
		for _, change := range diff.Changes {
			fmt.Fprintf(out, "  - %s\n", change.Summary)
		}
	}
	printFindings(cmd, "Data loss", diff.Destructive)
	printFindings(cmd, "Cannot apply", diff.Blocking)
}

func printFindings(cmd *cobra.Command, title string, findings []protocol.SchemaFinding) {
	if len(findings) == 0 {
		return
	}
	fmt.Fprintf(cmd.OutOrStdout(), "\n%s\n", title)
	for _, finding := range findings {
		fmt.Fprintf(cmd.OutOrStdout(), "  - %s\n", finding.Summary)
	}
}
