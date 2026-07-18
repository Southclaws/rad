package main

import (
	"fmt"
	"os"
	"path/filepath"

	yaml "github.com/goccy/go-yaml"

	"github.com/Southclaws/rad/rad/protocol"
)

const (
	defaultSchemaFile = "rad.schema.yaml"
	defaultConfigFile = "rad.config.yaml"
	defaultStateDir   = "rad.state"
)

type projectConfig struct {
	DatabaseURL string            `yaml:"database_url"`
	Generate    []generatorConfig `yaml:"generate"`
}

type generatorConfig struct {
	Language string `yaml:"language"`
	Output   string `yaml:"output"`
	Package  string `yaml:"package"`
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
	if len(config.Generate) == 0 {
		config.Generate = []generatorConfig{{Language: "go", Output: "generated", Package: "db"}}
	}
	for i := range config.Generate {
		generator := &config.Generate[i]
		if generator.Language == "" {
			generator.Language = "go"
		}
		if generator.Output == "" {
			generator.Output = "generated"
		}
		if generator.Package == "" {
			generator.Package = "db"
		}
		if generator.Language != "go" && generator.Language != "ts" && generator.Language != "typescript" {
			return projectConfig{}, fmt.Errorf("%s: generate[%d].language %q is not supported", filename, i, generator.Language)
		}
	}
	return config, nil
}

type project struct {
	Root       string
	ConfigFile string
	SchemaFile string
	StateDir   string
	Config     projectConfig
}

func loadProject(configFile, schemaFile string) (project, error) {
	absoluteConfig, err := filepath.Abs(configFile)
	if err != nil {
		return project{}, err
	}
	config, err := loadProjectConfig(absoluteConfig)
	if err != nil {
		return project{}, err
	}
	root := filepath.Dir(absoluteConfig)
	if schemaFile == defaultSchemaFile {
		schemaFile = filepath.Join(root, defaultSchemaFile)
	} else if !filepath.IsAbs(schemaFile) {
		absoluteSchema, err := filepath.Abs(schemaFile)
		if err != nil {
			return project{}, err
		}
		schemaFile = absoluteSchema
	}
	return project{
		Root: root, ConfigFile: absoluteConfig, SchemaFile: schemaFile,
		StateDir: filepath.Join(root, defaultStateDir), Config: config,
	}, nil
}
