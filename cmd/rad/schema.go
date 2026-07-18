package main

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"

	radclient "github.com/Southclaws/rad/rad/client"
)

type schemaOptions struct {
	configFile string
	schemaFile string
}

func schemaCmd() *cobra.Command {
	options := &schemaOptions{}
	cmd := &cobra.Command{
		Use:   "schema",
		Short: "Inspect and manage the database schema",
		Args:  cobra.NoArgs,
	}
	cmd.PersistentFlags().StringVar(&options.configFile, "config", defaultConfigFile, "project config file")
	cmd.PersistentFlags().StringVarP(&options.schemaFile, "file", "f", defaultSchemaFile, "desired schema file")
	cmd.AddCommand(
		schemaStatusCmd(options),
		schemaDiffCmd(options),
		schemaMigrateCmd(options),
		schemaPullCmd(options),
	)
	return cmd
}

func openSchemaProject(options *schemaOptions) (project, *radclient.Client, error) {
	project, err := loadProject(options.configFile, options.schemaFile)
	if err != nil {
		return project, nil, err
	}
	client, err := radclient.Dial(project.Config.DatabaseURL)
	if err != nil {
		return project, nil, err
	}
	return project, client, nil
}

func readDesired(project project) ([]byte, error) {
	source, err := os.ReadFile(project.SchemaFile)
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", project.SchemaFile, err)
	}
	return source, nil
}
