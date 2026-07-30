// Command jsonschemadoc renders Rad's LIR and PIR JSON Schemas as MDX
// references.
package main

import (
	"errors"
	"flag"
	"fmt"
	"log"
	"os"
	"sort"
	"strings"

	"github.com/Southclaws/rad/tools/internal/docgen"
	yaml "github.com/goccy/go-yaml"
)

type schemaDocument struct {
	Title       string                 `yaml:"title"`
	Description string                 `yaml:"description"`
	Ref         string                 `yaml:"$ref"`
	Definitions map[string]*schemaNode `yaml:"$defs"`
}

type schemaNode struct {
	Ref         string                 `yaml:"$ref"`
	Title       string                 `yaml:"title"`
	Description string                 `yaml:"description"`
	Type        any                    `yaml:"type"`
	Format      string                 `yaml:"format"`
	Properties  map[string]*schemaNode `yaml:"properties"`
	Required    []string               `yaml:"required"`
	Items       *schemaNode            `yaml:"items"`
	OneOf       []*schemaNode          `yaml:"oneOf"`
	AnyOf       []*schemaNode          `yaml:"anyOf"`
	AllOf       []*schemaNode          `yaml:"allOf"`
	Enum        []any                  `yaml:"enum"`
	Const       any                    `yaml:"const"`
	Default     any                    `yaml:"default"`
	MinLength   *uint64                `yaml:"minLength"`
	MaxLength   *uint64                `yaml:"maxLength"`
	MinItems    *uint64                `yaml:"minItems"`
	MaxItems    *uint64                `yaml:"maxItems"`
	UniqueItems bool                   `yaml:"uniqueItems"`
}

type page struct {
	Title         string
	Description   string
	SchemaURL     string
	Specification string
	Groups        []schemaGroup
}

type schemaGroup struct {
	Name        string
	Description string
	Definitions []definitionDoc
}

type definitionDoc struct {
	Name        string
	Description string
	Variants    []string
	Properties  []propertyDoc
}

type propertyDoc struct {
	Name        string
	Type        string
	Required    string
	Description string
}

type profile struct {
	Title       string
	Description string
	SchemaURL   string
	Groups      []groupDefinition
	Classify    func(string) string
}

type groupDefinition struct {
	Name        string
	Description string
}

const pageTemplate = `---
title: {{ .Title }}
description: {{ .Description }}
---

[Open the canonical JSON Schema]({{ .SchemaURL }}).
{{- if .Specification }}

{{ .Specification }}
{{- end }}
{{- range .Groups }}

## {{ .Name }}

{{ .Description }}
{{- range .Definitions }}

#### {{ code .Name }}

{{ .Description }}
{{- if .Variants }}

**Variants:** {{ range $index, $variant := .Variants }}{{ if $index }}, {{ end }}{{ $variant }}{{ end }}
{{- end }}
{{- if .Properties }}

| Field | Type | Required | Description |
| --- | --- | --- | --- |
{{- range .Properties }}
| {{ code .Name }} | {{ .Type }} | {{ .Required }} | {{ table .Description }} |
{{- end }}
{{- end }}
{{- end }}
{{- end }}
`

func main() {
	log.SetFlags(0)
	log.SetPrefix("jsonschemadoc: ")

	input := flag.String("input", "", "JSON Schema YAML document to read")
	output := flag.String("output", "", "MDX page to write")
	profileName := flag.String("profile", "", "documentation profile: lir or pir")
	flag.Parse()

	if *input == "" || *output == "" || *profileName == "" {
		log.Fatal("-input, -output, and -profile are required")
	}
	source, err := os.ReadFile(*input)
	if err != nil {
		log.Fatalf("read %s: %v", *input, err)
	}
	result, err := render(source, *profileName)
	if err != nil {
		log.Fatal(err)
	}
	if err := docgen.Write(*output, result); err != nil {
		log.Fatal(err)
	}
}

func render(source []byte, profileName string) ([]byte, error) {
	var spec schemaDocument
	if err := yaml.Unmarshal(source, &spec); err != nil {
		return nil, fmt.Errorf("parse JSON Schema YAML: %w", err)
	}
	if spec.Ref == "" {
		return nil, errors.New("$ref is required")
	}
	if len(spec.Definitions) == 0 {
		return nil, errors.New("$defs must contain at least one definition")
	}
	selected, err := selectProfile(profileName)
	if err != nil {
		return nil, err
	}
	result := buildPage(spec, selected)
	return docgen.Render("json-schema", pageTemplate, result, nil)
}

func buildPage(spec schemaDocument, selected profile) page {
	result := page{
		Title:         selected.Title,
		Description:   selected.Description,
		SchemaURL:     selected.SchemaURL,
		Specification: specificationSections(spec.Description),
	}

	groupIndex := make(map[string]int, len(selected.Groups))
	for _, definition := range selected.Groups {
		groupIndex[definition.Name] = len(result.Groups)
		result.Groups = append(result.Groups, schemaGroup{
			Name:        definition.Name,
			Description: definition.Description,
		})
	}

	names := make([]string, 0, len(spec.Definitions))
	for name := range spec.Definitions {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		groupName := selected.Classify(name)
		index, ok := groupIndex[groupName]
		if !ok {
			panic("JSON Schema documentation profile returned unknown group " + groupName)
		}
		result.Groups[index].Definitions = append(
			result.Groups[index].Definitions,
			buildDefinition(name, spec.Definitions[name]),
		)
	}

	groups := result.Groups[:0]
	for _, group := range result.Groups {
		if len(group.Definitions) > 0 {
			groups = append(groups, group)
		}
	}
	result.Groups = groups
	return result
}

func specificationSections(value string) string {
	value = strings.ReplaceAll(value, "\r\n", "\n")
	start := strings.Index(value, "\n# ")
	if start == -1 {
		return ""
	}
	value = strings.TrimSpace(value[start+1:])
	lines := strings.Split(value, "\n")
	for index, line := range lines {
		if strings.HasPrefix(line, "#") {
			lines[index] = "#" + line
		}
	}
	return docgen.Paragraphs(strings.Join(lines, "\n"))
}

func buildDefinition(name string, schema *schemaNode) definitionDoc {
	result := definitionDoc{
		Name:        name,
		Description: docgen.Paragraphs(schema.Description),
	}
	if result.Description == "" {
		result.Description = schema.Title
	}
	for _, variant := range schema.OneOf {
		result.Variants = append(result.Variants, schemaType(variant))
	}

	required := make(map[string]struct{}, len(schema.Required))
	for _, name := range schema.Required {
		required[name] = struct{}{}
	}
	names := make([]string, 0, len(schema.Properties))
	for name := range schema.Properties {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, propertyName := range names {
		property := schema.Properties[propertyName]
		_, isRequired := required[propertyName]
		result.Properties = append(result.Properties, propertyDoc{
			Name:        propertyName,
			Type:        schemaType(property),
			Required:    yes(isRequired),
			Description: propertyDescription(property),
		})
	}
	return result
}

func propertyDescription(schema *schemaNode) string {
	var details []string
	if schema.Default != nil {
		details = append(details, "Default: "+fmt.Sprint(schema.Default)+".")
	}
	if schema.Format != "" {
		details = append(details, "Format: "+schema.Format+".")
	}
	if schema.MinLength != nil {
		details = append(details, fmt.Sprintf("Minimum length: %d.", *schema.MinLength))
	}
	if schema.MaxLength != nil {
		details = append(details, fmt.Sprintf("Maximum length: %d.", *schema.MaxLength))
	}
	if schema.MinItems != nil {
		details = append(details, fmt.Sprintf("Minimum items: %d.", *schema.MinItems))
	}
	if schema.MaxItems != nil {
		details = append(details, fmt.Sprintf("Maximum items: %d.", *schema.MaxItems))
	}
	if schema.UniqueItems {
		details = append(details, "Items must be unique.")
	}
	return strings.TrimSpace(strings.Join(append([]string{schema.Description}, details...), " "))
}

func schemaType(schema *schemaNode) string {
	if schema == nil {
		return "Any JSON value"
	}
	if schema.Ref != "" {
		return docgen.Code(referenceName(schema.Ref))
	}
	if schema.Const != nil {
		return docgen.Code(fmt.Sprint(schema.Const))
	}
	if len(schema.Enum) > 0 {
		values := make([]string, 0, len(schema.Enum))
		for _, value := range schema.Enum {
			values = append(values, fmt.Sprint(value))
		}
		return docgen.Code(strings.Join(values, " | "))
	}
	if len(schema.OneOf) > 0 || len(schema.AnyOf) > 0 {
		variants := schema.OneOf
		if len(variants) == 0 {
			variants = schema.AnyOf
		}
		values := make([]string, 0, len(variants))
		for _, variant := range variants {
			values = append(values, strings.Trim(schemaType(variant), "`"))
		}
		return docgen.Code(strings.Join(values, " | "))
	}

	types := schemaTypes(schema.Type)
	value := strings.Join(types, " | ")
	if len(types) == 1 && types[0] == "array" {
		value = "array"
		if schema.Items != nil {
			value += " of " + strings.Trim(schemaType(schema.Items), "`")
		}
	}
	if value == "" {
		value = "value"
	}
	return docgen.Code(value)
}

func schemaTypes(value any) []string {
	switch typed := value.(type) {
	case nil:
		return nil
	case string:
		return []string{typed}
	case []string:
		return typed
	case []any:
		result := make([]string, 0, len(typed))
		for _, item := range typed {
			result = append(result, fmt.Sprint(item))
		}
		return result
	default:
		return []string{fmt.Sprint(typed)}
	}
}

func referenceName(reference string) string {
	index := strings.LastIndex(reference, "/")
	if index == -1 {
		return strings.TrimPrefix(reference, "#")
	}
	return reference[index+1:]
}

func yes(value bool) string {
	if value {
		return "Yes"
	}
	return ""
}

func selectProfile(name string) (profile, error) {
	switch name {
	case "lir":
		return lirProfile(), nil
	case "pir":
		return pirProfile(), nil
	default:
		return profile{}, fmt.Errorf("unknown profile %q", name)
	}
}

func lirProfile() profile {
	return profile{
		Title:       "LIR schema",
		Description: "The relations, expressions, crossings, and result shapes accepted by Rad.",
		SchemaURL:   "/schema/lir.json",
		Groups: []groupDefinition{
			{Name: "Query document", Description: "The top-level query, its named graph, and result root."},
			{Name: "Relations", Description: "Nodes that read, combine, reshape, and order rows."},
			{Name: "Expressions", Description: "Values and predicates evaluated in a relation scope."},
			{Name: "Text matching", Description: "Literal spans, wildcards, and comparison rules for text patterns."},
			{Name: "Projection and terms", Description: "Fields, grouping terms, aggregates, and ordering terms."},
			{Name: "Values", Description: "Scalar types and JSON value representations."},
		},
		Classify: classifyLIR,
	}
}

func classifyLIR(name string) string {
	switch name {
	case "Query", "Binding", "DerivedBinding", "RecursiveBinding", "Root":
		return "Query document"
	case "TextMatchExpr", "TextComparison", "TextMatchExprPart", "LiteralTextMatchPart", "AnyManyTextMatchPart":
		return "Text matching"
	case "Field", "GroupTerm", "AggTerm", "OrderTerm":
		return "Projection and terms"
	case "ScalarType", "Value", "TextValue", "Int64Value", "Float64Value", "BoolValue", "Cell":
		return "Values"
	}
	if name == "Node" || strings.HasSuffix(name, "Node") || name == "RowsColumn" {
		return "Relations"
	}
	return "Expressions"
}

func pirProfile() profile {
	return profile{
		Title:       "PIR schema",
		Description: "The ordered transactional programs accepted by Rad.",
		SchemaURL:   "/schema/pir.json",
		Groups: []groupDefinition{
			{Name: "Program", Description: "The program envelope and relational statement families."},
			{Name: "Catalog changes", Description: "Transactional changes to tables, columns, and indexes."},
			{Name: "Online schema work", Description: "Statements that start durable physical work."},
			{Name: "Catalog definitions", Description: "Stable identities and definitions carried by catalog statements."},
			{Name: "LIR relation", Description: "The embedded relation consumed by a relational statement."},
		},
		Classify: classifyPIR,
	}
}

func classifyPIR(name string) string {
	switch name {
	case "Program", "Statement", "QueryStatement", "CreateStatement", "UpdateStatement", "DeleteStatement":
		return "Program"
	case "StartIndexBuildStatement", "StartColumnReplacementStatement", "StartConstraintValidationStatement":
		return "Online schema work"
	case "Relation":
		return "LIR relation"
	}
	if strings.HasSuffix(name, "Statement") {
		return "Catalog changes"
	}
	return "Catalog definitions"
}
