package main

import (
	"fmt"
	"os"
	"path/filepath"

	"github.com/spf13/cobra"

	"rad/codegen"
	"rad/rad/02_catalog/schema"
)

func generateCmd() *cobra.Command {
	var file, out, pkg string
	cmd := &cobra.Command{
		Use:   "generate",
		Short: "Generate the typed Go client for a schema.rad",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			src, err := os.ReadFile(file)
			if err != nil {
				return err
			}
			sch, err := schema.Parse(file, src)
			if err != nil {
				return err
			}
			code, err := codegen.Generate(pkg, sch, src)
			if err != nil {
				return err
			}
			if err := os.MkdirAll(out, 0o755); err != nil {
				return err
			}
			dest := filepath.Join(out, pkg+".go")
			if err := os.WriteFile(dest, code, 0o644); err != nil {
				return err
			}
			fmt.Printf("%s: generated %s (%d tables)\n", file, dest, len(sch.Tables))
			return nil
		},
	}
	cmd.Flags().StringVarP(&file, "file", "f", "schema.rad", "schema file")
	cmd.Flags().StringVarP(&out, "out", "o", "generated", "output directory")
	cmd.Flags().StringVar(&pkg, "pkg", "db", "generated package name")
	return cmd
}
