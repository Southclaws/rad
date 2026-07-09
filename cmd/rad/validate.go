package main

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"

	schema "rad/rad/02_catalog/schema"
)

func validateCmd() *cobra.Command {
	var file string
	cmd := &cobra.Command{
		Use:   "validate",
		Short: "Validate a schema.rad file without a running database",
		Long: `Parse and validate a schema.rad file — structure, types, and references —
without touching a database.`,
		Args:          cobra.NoArgs,
		SilenceErrors: true,
		SilenceUsage:  true,
		RunE: func(cmd *cobra.Command, args []string) error {
			src, err := os.ReadFile(file)
			if err != nil {
				fmt.Fprintln(os.Stderr, cRed("✗")+" "+err.Error())
				return err
			}
			sch, err := schema.Parse(file, src)
			if err != nil {
				fmt.Fprintln(os.Stderr, cRed("✗ invalid")+"  "+err.Error())
				return err
			}
			cols := 0
			for _, t := range sch.Tables {
				cols += len(t.Def.Columns)
			}
			fmt.Printf("%s  %s %s\n",
				cGreen("✓ valid"),
				cBold(file),
				cDim(fmt.Sprintf("— %d tables, %d columns", len(sch.Tables), cols)),
			)
			return nil
		},
	}
	cmd.Flags().StringVarP(&file, "file", "f", "schema.rad", "schema file")
	return cmd
}
