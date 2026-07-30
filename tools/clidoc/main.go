// Command clidoc renders the OpenCLI command specification as an MDX manual.
package main

import (
	"errors"
	"flag"
	"fmt"
	"log"
	"os"
	"strings"

	"github.com/Southclaws/rad/tools/internal/docgen"
	yaml "github.com/goccy/go-yaml"
)

type document struct {
	Info       info       `yaml:"info"`
	ExitCodes  []exitCode `yaml:"exitCodes"`
	Commands   []command  `yaml:"commands"`
	Components components `yaml:"components"`
}

type info struct {
	BinaryName  string `yaml:"binaryName"`
	Description string `yaml:"description"`
}

type exitCode struct {
	Code        int    `yaml:"code"`
	Label       string `yaml:"label"`
	Description string `yaml:"description"`
}

type components struct {
	Flags map[string]option `yaml:"flags"`
}

type command struct {
	Name            string    `yaml:"name"`
	Description     string    `yaml:"description"`
	LongDescription string    `yaml:"longDescription"`
	Flags           []option  `yaml:"flags"`
	Commands        []command `yaml:"commands"`
}

type option struct {
	Ref         string   `yaml:"$ref"`
	Name        string   `yaml:"name"`
	Short       string   `yaml:"short"`
	Description string   `yaml:"description"`
	Type        string   `yaml:"type"`
	EnvVar      string   `yaml:"envVar"`
	Default     any      `yaml:"default"`
	Choices     []string `yaml:"choices"`
}

type page struct {
	Description string
	Summary     []commandSummary
	Commands    []commandDoc
	ExitCodes   []exitCode
}

type commandSummary struct {
	Invocation  string
	Description string
}

type commandDoc struct {
	Heading     string
	Invocation  string
	Description string
	Subcommands []commandSummary
	Options     []optionDoc
}

type optionDoc struct {
	Name        string
	Description string
	Default     string
	Environment string
}

const pageTemplate = `---
title: CLI
description: Commands and options provided by the Rad command-line tool.
---

{{ .Description }}

## Commands

| Command | Purpose |
| --- | --- |
{{- range .Summary }}
| {{ code .Invocation }} | {{ table .Description }} |
{{- end }}
{{- range .Commands }}

{{ .Heading }} {{ code .Invocation }}

{{ .Description }}
{{- if .Subcommands }}

| Subcommand | Purpose |
| --- | --- |
{{- range .Subcommands }}
| {{ code .Invocation }} | {{ table .Description }} |
{{- end }}
{{- end }}
{{- if .Options }}

| Option | Purpose | Default | Environment |
| --- | --- | --- | --- |
{{- range .Options }}
| {{ .Name }} | {{ table .Description }} | {{ .Default }} | {{ .Environment }} |
{{- end }}
{{- end }}
{{- end }}
{{- if .ExitCodes }}

## Exit codes

| Code | Meaning |
| --- | --- |
{{- range .ExitCodes }}
| {{ code .Code }} | {{ table .Label }}: {{ table .Description }} |
{{- end }}
{{- end }}
`

func main() {
	log.SetFlags(0)
	log.SetPrefix("clidoc: ")

	input := flag.String("input", "opencli.yaml", "OpenCLI document to read")
	output := flag.String("output", "home/content/docs/cli.mdx", "MDX page to write")
	flag.Parse()

	source, err := os.ReadFile(*input)
	if err != nil {
		log.Fatalf("read %s: %v", *input, err)
	}

	var spec document
	if err := yaml.Unmarshal(source, &spec); err != nil {
		log.Fatalf("parse %s: %v", *input, err)
	}

	page, err := render(spec)
	if err != nil {
		log.Fatal(err)
	}
	if err := docgen.Write(*output, page); err != nil {
		log.Fatal(err)
	}
}

func render(spec document) ([]byte, error) {
	if spec.Info.BinaryName == "" {
		return nil, errors.New("info.binaryName is required")
	}

	result := page{
		Description: "Use the CLI to run Rad, apply `rad.schema.yaml`, and generate a typed client from the accepted schema.",
		ExitCodes:   spec.ExitCodes,
	}
	for _, item := range spec.Commands {
		if err := appendCommand(&result, spec, nil, item); err != nil {
			return nil, err
		}
	}
	return docgen.Render("cli", pageTemplate, result, nil)
}

func appendCommand(result *page, spec document, parents []string, item command) error {
	path := appendPath(parents, item.Name)
	invocation := spec.Info.BinaryName + " " + strings.Join(path, " ")
	description := strings.TrimSpace(item.LongDescription)
	if description == "" {
		description = strings.TrimSpace(item.Description)
	}
	doc := commandDoc{
		Heading:     "###",
		Invocation:  invocation,
		Description: docgen.Paragraphs(description),
	}
	if len(parents) > 0 {
		doc.Heading = "####"
	}
	result.Summary = append(result.Summary, commandSummary{
		Invocation:  invocation,
		Description: item.Description,
	})

	for _, child := range item.Commands {
		doc.Subcommands = append(doc.Subcommands, commandSummary{
			Invocation:  child.Name,
			Description: child.Description,
		})
	}

	for _, raw := range item.Flags {
		value, err := resolveOption(spec, raw)
		if err != nil {
			return fmt.Errorf("%s: %w", invocation, err)
		}
		doc.Options = append(doc.Options, optionDoc{
			Name:        optionName(value),
			Description: optionDescription(value),
			Default:     docgen.Code(value.Default),
			Environment: docgen.Code(value.EnvVar),
		})
	}
	result.Commands = append(result.Commands, doc)

	for _, child := range item.Commands {
		if err := appendCommand(result, spec, path, child); err != nil {
			return err
		}
	}
	return nil
}

func resolveOption(spec document, value option) (option, error) {
	if value.Ref == "" {
		return value, nil
	}
	const prefix = "#/components/flags/"
	if !strings.HasPrefix(value.Ref, prefix) {
		return option{}, fmt.Errorf("unsupported option reference %q", value.Ref)
	}
	resolved, ok := spec.Components.Flags[strings.TrimPrefix(value.Ref, prefix)]
	if !ok {
		return option{}, fmt.Errorf("option reference %q does not exist", value.Ref)
	}
	return resolved, nil
}

func optionName(value option) string {
	name := "`--" + value.Name
	if placeholder := optionPlaceholder(value); placeholder != "" {
		name += " " + strings.ReplaceAll(placeholder, "|", "\\|")
	}
	name += "`"
	if value.Short != "" {
		name = "`-" + value.Short + "`, " + name
	}
	return name
}

func optionPlaceholder(value option) string {
	if value.Type == "boolean" {
		return ""
	}
	if len(value.Choices) > 0 {
		return "<" + strings.Join(value.Choices, "|") + ">"
	}
	switch value.Type {
	case "file":
		return "<file>"
	case "path":
		return "<path>"
	default:
		return "<value>"
	}
}

func optionDescription(value option) string {
	description := docgen.OneLine(value.Description)
	if len(value.Choices) > 0 {
		description += " Choices: " + strings.Join(value.Choices, ", ") + "."
	}
	return description
}

func appendPath(path []string, value string) []string {
	next := make([]string, len(path), len(path)+1)
	copy(next, path)
	return append(next, value)
}
