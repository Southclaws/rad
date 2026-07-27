package main

import (
	"path/filepath"

	"github.com/Southclaws/rad/cmd/rad/config"
)

type project struct {
	Root       string
	ConfigFile string
	SchemaFile string
	StateDir   string
	Config     config.RadProjectConfiguration
}

func loadProject(configFile, schemaFile string) (project, error) {
	absoluteConfig, err := filepath.Abs(configFile)
	if err != nil {
		return project{}, err
	}
	cfg, err := config.Load(absoluteConfig)
	if err != nil {
		return project{}, err
	}
	root := filepath.Dir(absoluteConfig)
	if schemaFile == config.DefaultSchemaFile {
		schemaFile = filepath.Join(root, config.DefaultSchemaFile)
	} else if !filepath.IsAbs(schemaFile) {
		absoluteSchema, err := filepath.Abs(schemaFile)
		if err != nil {
			return project{}, err
		}
		schemaFile = absoluteSchema
	}
	return project{
		Root: root, ConfigFile: absoluteConfig, SchemaFile: schemaFile,
		StateDir: filepath.Join(root, config.DefaultStateDir), Config: cfg,
	}, nil
}
