package main

import (
	"fmt"
	"os"
	"path/filepath"

	"github.com/spf13/cobra"

	"github.com/Southclaws/rad/rad/codegen"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
)

func generateCmd() *cobra.Command {
	var file, out, pkg, lang string
	cmd := &cobra.Command{
		Use:   "generate",
		Short: "Generate a typed client (Go or TypeScript) for a schema.rad",
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

			var code []byte
			var dest string
			switch lang {
			case "go":
				code, err = codegen.Generate(pkg, sch, src)
				dest = filepath.Join(out, pkg+".go")
			case "ts", "typescript":
				code, err = codegen.GenerateTS(sch, src)
				dest = filepath.Join(out, pkg+".ts")
			default:
				return fmt.Errorf("unknown --lang %q (go, ts)", lang)
			}
			if err != nil {
				return err
			}
			if err := os.MkdirAll(out, 0o755); err != nil {
				return err
			}
			if err := os.WriteFile(dest, code, 0o644); err != nil {
				return err
			}
			fmt.Printf("%s: generated %s (%d tables)\n", file, dest, len(sch.Tables))
			return nil
		},
	}
	cmd.Flags().StringVarP(&file, "file", "f", "schema.rad", "schema file")
	cmd.Flags().StringVarP(&out, "out", "o", "generated", "output directory")
	cmd.Flags().StringVar(&pkg, "pkg", "db", "generated package (Go) / file basename")
	cmd.Flags().StringVar(&lang, "lang", "go", "client language: go, ts")
	return cmd
}
