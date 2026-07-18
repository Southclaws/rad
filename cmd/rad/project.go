package main

import (
	"fmt"
	"os"

	yaml "github.com/goccy/go-yaml"

	"github.com/Southclaws/rad/rad/protocol"
)

const (
	defaultSchemaFile = "rad.schema.yaml"
	defaultConfigFile = "rad.config.yaml"
	defaultStateDir   = "rad.state"
)

type projectConfig struct {
	DatabaseURL string `yaml:"database_url"`
}

func loadProjectConfig(filename string) (projectConfig, error) {
	source, err := os.ReadFile(filename)
	if err != nil {
		return projectConfig{}, err
	}
	var config projectConfig
	if err := yaml.UnmarshalWithOptions(source, &config, yaml.Strict()); err != nil {
		return projectConfig{}, fmt.Errorf("%s: %w", filename, err)
	}
	if config.DatabaseURL == "" {
		return projectConfig{}, fmt.Errorf("%s: database_url is required", filename)
	}
	if _, err := protocol.ParseURL(config.DatabaseURL); err != nil {
		return projectConfig{}, fmt.Errorf("%s: database_url: %w", filename, err)
	}
	return config, nil
}
