package main

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/spf13/cobra"

	"github.com/Southclaws/rad/rad/codegen"
	projectstate "github.com/Southclaws/rad/rad/state"
)

func schemaStatusCmd(options *schemaOptions) *cobra.Command {
	return &cobra.Command{
		Use:   "status",
		Short: "Compare server, accepted, desired, and generated schemas",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			project, client, err := openSchemaProject(options)
			if err != nil {
				return err
			}
			server, err := client.Schema(cmd.Context())
			if err != nil {
				return err
			}
			fmt.Fprintf(cmd.OutOrStdout(), "Server\n  version: %d\n  hash:    %s\n", server.SchemaVersion, server.SchemaHash)

			accepted, acceptedErr := projectstate.New(project.StateDir).Load()
			if acceptedErr == nil {
				fmt.Fprintf(cmd.OutOrStdout(), "\nLocal accepted schema\n  version: %d\n  hash:    %s\n", accepted.Lock.SchemaVersion, accepted.Lock.SchemaHash)
			} else {
				fmt.Fprintf(cmd.OutOrStdout(), "\nLocal accepted schema\n  unavailable: %s\n", stateErrorSummary(acceptedErr))
			}

			desired, desiredErr := readDesired(project)
			changes := -1
			if desiredErr == nil {
				diff, err := client.SchemaDiff(cmd.Context(), string(desired))
				if err != nil {
					fmt.Fprintf(cmd.OutOrStdout(), "\nLocal schema\n  unavailable: %s\n", err)
				} else {
					changes = len(diff.Changes)
					fmt.Fprintf(cmd.OutOrStdout(), "\nLocal schema\n  changes: %d\n", changes)
				}
			} else {
				fmt.Fprintf(cmd.OutOrStdout(), "\nLocal schema\n  unavailable: %s\n", desiredErr)
			}

			generated := generatedState(project, server.SchemaVersion, server.SchemaHash)
			fmt.Fprintf(cmd.OutOrStdout(), "\nGenerated client\n  %s\n", generated)
			fmt.Fprintf(cmd.OutOrStdout(), "\nStatus: %s\n", schemaStatusSummary(server.SchemaVersion, server.SchemaHash, accepted, acceptedErr, changes, generated))
			return nil
		},
	}
}

func stateErrorSummary(err error) string {
	if errors.Is(err, os.ErrNotExist) {
		return "missing lockfile or snapshot"
	}
	return err.Error()
}

func generatedState(project project, version uint64, hash string) string {
	if len(project.Config.Generate) == 0 {
		return "not configured"
	}
	for _, target := range project.Config.Generate {
		language := target.Language
		filename := codegen.GoClientFilename
		versionText := "SchemaVersion uint64 = " + strconv.FormatUint(version, 10)
		hashText := "SchemaHash = " + strconv.Quote(hash)
		if language == "ts" || language == "typescript" {
			filename = codegen.TypeScriptClientFilename
			versionText = "schemaVersion = " + strconv.FormatUint(version, 10)
			hashText = "schemaHash = " + strconv.Quote(hash)
		}
		output := target.Output
		if !filepath.IsAbs(output) {
			output = filepath.Join(project.Root, output)
		}
		path := filepath.Join(output, filename)
		source, err := os.ReadFile(path)
		if err != nil {
			return "missing: " + path
		}
		text := string(source)
		if !strings.Contains(text, versionText) || !strings.Contains(text, hashText) {
			return "outdated: " + path
		}
	}
	return fmt.Sprintf("version: %d\n  hash:    %s", version, hash)
}

func schemaStatusSummary(
	serverVersion uint64,
	serverHash string,
	accepted projectstate.Accepted,
	acceptedErr error,
	changes int,
	generated string,
) string {
	if acceptedErr != nil {
		return "local accepted schema state is unavailable"
	}
	if accepted.Lock.SchemaVersion == serverVersion && accepted.Lock.SchemaHash != serverHash {
		return "schema history diverged"
	}
	if accepted.Lock.SchemaVersion < serverVersion {
		return "server is ahead of local accepted state"
	}
	if accepted.Lock.SchemaVersion > serverVersion {
		return "local accepted state is ahead of server"
	}
	if changes < 0 {
		return "rad.schema.yaml is unavailable"
	}
	if changes > 0 {
		return "rad.schema.yaml contains unapplied changes"
	}
	if strings.HasPrefix(generated, "missing:") || strings.HasPrefix(generated, "outdated:") {
		return "generated client is outdated"
	}
	return "synchronized"
}
