// Package config loads local Rad project configuration.
package config

import (
	"fmt"
	"os"

	yaml "github.com/goccy/go-yaml"

	"github.com/Southclaws/rad/rad/protocol"
)

const (
	DefaultSchemaFile = "rad.schema.yaml"
	DefaultConfigFile = "rad.config.yaml"
	DefaultStateDir   = "rad.state"
)

// Load reads and validates a rad.config.yaml file from the given path.
func Load(filename string) (RadProjectConfiguration, error) {
	source, err := os.ReadFile(filename)
	if err != nil {
		return RadProjectConfiguration{}, err
	}
	var config RadProjectConfiguration
	if err := yaml.UnmarshalWithOptions(source, &config, yaml.Strict()); err != nil {
		return RadProjectConfiguration{}, fmt.Errorf("%s: %w", filename, err)
	}
	if err := validate(filename, &config); err != nil {
		return RadProjectConfiguration{}, err
	}
	return config, nil
}

func validate(filename string, config *RadProjectConfiguration) error {
	if config.DatabaseURL == "" {
		return fmt.Errorf("%s: database_url is required", filename)
	}
	if _, err := protocol.ParseURL(config.DatabaseURL); err != nil {
		return fmt.Errorf("%s: database_url: %w", filename, err)
	}
	if len(config.Generate) == 0 {
		config.Generate = []RadProjectConfigurationGenerateItem{
			{
				Language: new("go"),
				Output:   new("generated"),
				Package:  new("db"),
			},
		}
	}
	for i := range config.Generate {
		generator := &config.Generate[i]
		if generator.Language == nil || *generator.Language == "" {
			generator.Language = new("go")
		}
		if generator.Output == nil || *generator.Output == "" {
			generator.Output = new("generated")
		}
		if generator.Package == nil || *generator.Package == "" {
			generator.Package = new("db")
		}
		lang := *generator.Language
		if lang != "go" && lang != "ts" && lang != "typescript" {
			return fmt.Errorf("%s: generate[%d].language %q is not supported", filename, i, lang)
		}
	}
	return nil
}

// GetLanguage returns the language field value, defaulting to "go" if nil or empty.
func (g *RadProjectConfigurationGenerateItem) GetLanguage() string {
	if g.Language == nil || *g.Language == "" {
		return "go"
	}
	return *g.Language
}

// GetOutput returns the output field value, defaulting to "generated" if nil or empty.
func (g *RadProjectConfigurationGenerateItem) GetOutput() string {
	if g.Output == nil || *g.Output == "" {
		return "generated"
	}
	return *g.Output
}

// GetPackage returns the package field value, defaulting to "db" if nil or empty.
func (g *RadProjectConfigurationGenerateItem) GetPackage() string {
	if g.Package == nil || *g.Package == "" {
		return "db"
	}
	return *g.Package
}
