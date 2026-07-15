package main

import (
	"fmt"
	"os"
	"path/filepath"

	"github.com/spf13/cobra"

	"github.com/Southclaws/rad/rad/codegen"
	// Register the built-in generators. Each package registers itself under a
	// language name in its init; blank-importing them here is what makes them
	// discoverable via codegen.Lookup without the codegen package importing them.
	_ "github.com/Southclaws/rad/rad/codegen/golang"
	_ "github.com/Southclaws/rad/rad/codegen/typescript"
	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
)

func generateCmd() *cobra.Command {
	var file, out, pkg, lang string
	cmd := &cobra.Command{
		Use:   "generate",
		Short: "Generate a typed client for a schema.rad",
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

			name := lang
			if name == "typescript" {
				name = "ts"
			}
			gen, ok := codegen.Lookup(name)
			if !ok {
				return fmt.Errorf("unknown --lang %q (have: %v)", lang, codegen.Languages())
			}

			model, err := codegen.Build(pkg, sch)
			if err != nil {
				return err
			}
			files, err := gen.Generate(model, codegen.Options{Package: pkg, SchemaSource: src})
			if err != nil {
				return err
			}
			for _, f := range files {
				dest := filepath.Join(out, f.Path)
				if err := os.MkdirAll(filepath.Dir(dest), 0o755); err != nil {
					return err
				}
				if err := os.WriteFile(dest, f.Content, 0o644); err != nil {
					return err
				}
				fmt.Printf("%s: generated %s (%d tables)\n", file, dest, len(sch.Tables))
			}
			return nil
		},
	}
	cmd.Flags().StringVarP(&file, "file", "f", "schema.rad", "schema file")
	cmd.Flags().StringVarP(&out, "out", "o", "generated", "output directory")
	cmd.Flags().StringVar(&pkg, "pkg", "db", "generated package (Go) / file basename")
	cmd.Flags().StringVar(&lang, "lang", "go", "client language: go, ts")
	return cmd
}
