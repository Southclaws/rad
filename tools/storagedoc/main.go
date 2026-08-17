// Command storagedoc renders Rad's storage keyspace registry and storage
// JSON Schemas as an MDX reference.
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"io/fs"
	"log"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/Southclaws/rad/tools/internal/docgen"
	yaml "github.com/goccy/go-yaml"
)

type keyspaceRegistry struct {
	RootMagic string          `json:"root_magic"`
	Keyspaces []keyspaceEntry `json:"keyspaces"`
	Reserved  []reservedEntry `json:"reserved"`
}

type keyspaceEntry struct {
	Name       string   `json:"name"`
	Tag        string   `json:"tag"`
	Key        string   `json:"key"`
	Value      string   `json:"value"`
	Invariants []string `json:"invariants"`
}

type reservedEntry struct {
	Tag    string `json:"tag"`
	Reason string `json:"reason"`
}

type schemaNode struct {
	Description string                 `yaml:"description"`
	Type        any                    `yaml:"type"`
	Properties  map[string]*schemaNode `yaml:"properties"`
	Required    []string               `yaml:"required"`
	Items       *schemaNode            `yaml:"items"`
	Enum        []any                  `yaml:"enum"`
}

type schemaDocument struct {
	Title       string                 `yaml:"title"`
	Description string                 `yaml:"description"`
	Properties  map[string]*schemaNode `yaml:"properties"`
}

type page struct {
	RootMagic string
	Keyspaces []keyspaceRow
	Reserved  []reservedEntry
	Schemas   []schemaDoc
}

type keyspaceRow struct {
	Tag        string
	Name       string
	Key        string
	Value      string
	Invariants string
}

type schemaDoc struct {
	Name        string
	Description string
	Fields      []fieldRow
}

type fieldRow struct {
	Path        string
	Type        string
	Required    string
	Description string
}

const pageTemplate = `---
title: Storage format
description: The durable keyspaces and stored-value contracts of the storage engine.
---

Every durable key starts with the root magic ` + "`0x{{ .RootMagic }}`" + ` followed by
one permanently allocated keyspace tag. The normative byte-level
specification is [` + "`protocol/storage.keyform`" + `](https://github.com/Southclaws/rad/blob/main/protocol/storage.keyform);
this page documents the keyspaces and the structured values they store.

Structured values are JSON inside an envelope:

` + "```json" + `
{ "format": 1, "record": { ... } }
` + "```" + `

The ` + "`format`" + ` discriminator names the payload serialization explicitly. The
field tables below document the ` + "`record`" + ` member of each contract.

## Keyspaces

| Tag | Keyspace | Key layout | Value |
| --- | --- | --- | --- |
{{- range .Keyspaces }}
| {{ code .Tag }} | {{ code .Name }} | {{ code .Key }} | {{ table .Value }} |
{{- end }}
{{- range .Reserved }}

Tag {{ code .Tag }} is permanently reserved: {{ table .Reason }}
{{- end }}
{{- range .Schemas }}

## {{ code .Name }}

{{ .Description }}

| Field | Type | Required | Description |
| --- | --- | --- | --- |
{{- range .Fields }}
| {{ code .Path }} | {{ .Type }} | {{ .Required }} | {{ table .Description }} |
{{- end }}
{{- end }}
`

func main() {
	log.SetFlags(0)
	log.SetPrefix("storagedoc: ")

	schemas := flag.String("schemas", "", "directory holding the storage schema YAML documents")
	keyspaces := flag.String("keyspaces", "", "generated storage keyspace registry JSON")
	output := flag.String("output", "", "MDX page to write")
	flag.Parse()

	if *schemas == "" || *keyspaces == "" || *output == "" {
		log.Fatal("-schemas, -keyspaces, and -output are required")
	}
	registrySource, err := os.ReadFile(*keyspaces)
	if err != nil {
		log.Fatalf("read %s: %v", *keyspaces, err)
	}
	documents, err := loadSchemas(*schemas)
	if err != nil {
		log.Fatal(err)
	}
	result, err := render(registrySource, documents)
	if err != nil {
		log.Fatal(err)
	}
	if err := docgen.Write(*output, result); err != nil {
		log.Fatal(err)
	}
}

func loadSchemas(root string) (map[string][]byte, error) {
	documents := make(map[string][]byte)
	err := filepath.WalkDir(root, func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if entry.IsDir() || !strings.HasSuffix(path, ".schema.yaml") {
			return nil
		}
		source, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		relative, err := filepath.Rel(root, path)
		if err != nil {
			return err
		}
		reference := strings.TrimSuffix(filepath.ToSlash(relative), ".schema.yaml")
		documents[reference] = source
		return nil
	})
	if err != nil {
		return nil, fmt.Errorf("walk %s: %w", root, err)
	}
	if len(documents) == 0 {
		return nil, fmt.Errorf("no *.schema.yaml documents under %s", root)
	}
	return documents, nil
}

func render(registrySource []byte, documents map[string][]byte) ([]byte, error) {
	var registry keyspaceRegistry
	if err := json.Unmarshal(registrySource, &registry); err != nil {
		return nil, fmt.Errorf("parse keyspace registry: %w", err)
	}
	if registry.RootMagic == "" || len(registry.Keyspaces) == 0 {
		return nil, fmt.Errorf("keyspace registry is empty")
	}

	result := page{RootMagic: registry.RootMagic, Reserved: registry.Reserved}
	for _, entry := range registry.Keyspaces {
		key := entry.Key
		if key == "" {
			key = "(the tag is the whole key)"
		}
		result.Keyspaces = append(result.Keyspaces, keyspaceRow{
			Tag:        entry.Tag,
			Name:       entry.Name,
			Key:        key,
			Value:      valueText(entry),
			Invariants: strings.Join(entry.Invariants, " "),
		})
	}

	references := make([]string, 0, len(documents))
	for reference := range documents {
		references = append(references, reference)
	}
	sort.Strings(references)
	for _, reference := range references {
		document, err := buildSchema(reference, documents[reference])
		if err != nil {
			return nil, err
		}
		result.Schemas = append(result.Schemas, document)
	}
	return docgen.Render("storage", pageTemplate, result, nil)
}

func valueText(entry keyspaceEntry) string {
	value := entry.Value
	if reference, ok := strings.CutPrefix(value, "json schema "); ok {
		value = fmt.Sprintf("[`%s`](#%s)", reference, anchor(reference))
	}
	for _, invariant := range entry.Invariants {
		value += ". Invariant: " + invariant
	}
	return value
}

func anchor(reference string) string {
	replacer := strings.NewReplacer("/", "", ".", "")
	return replacer.Replace(reference)
}

func buildSchema(reference string, source []byte) (schemaDoc, error) {
	var document schemaDocument
	if err := yaml.Unmarshal(source, &document); err != nil {
		return schemaDoc{}, fmt.Errorf("parse schema %s: %w", reference, err)
	}
	record, ok := document.Properties["record"]
	if !ok {
		return schemaDoc{}, fmt.Errorf("schema %s has no record property", reference)
	}
	result := schemaDoc{
		Name:        reference,
		Description: docgen.Paragraphs(document.Description),
	}
	collectFields(record, "", &result.Fields)
	return result, nil
}

// collectFields flattens nested properties into dotted paths relative to the
// record member, appending [] for array elements, mirroring the structural
// signatures in the allocation registry.
func collectFields(node *schemaNode, prefix string, fields *[]fieldRow) {
	required := make(map[string]struct{}, len(node.Required))
	for _, name := range node.Required {
		required[name] = struct{}{}
	}
	names := make([]string, 0, len(node.Properties))
	for name := range node.Properties {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		property := node.Properties[name]
		path := name
		if prefix != "" {
			path = prefix + "." + name
		}
		_, isRequired := required[name]
		*fields = append(*fields, fieldRow{
			Path:        path,
			Type:        schemaType(property),
			Required:    yes(isRequired),
			Description: property.Description,
		})
		descend(property, path, fields)
	}
}

func descend(node *schemaNode, path string, fields *[]fieldRow) {
	if len(node.Properties) > 0 {
		collectFields(node, path, fields)
	}
	if node.Items != nil && len(node.Items.Properties) > 0 {
		collectFields(node.Items, path+"[]", fields)
	}
}

func schemaType(node *schemaNode) string {
	if node == nil {
		return docgen.Code("value")
	}
	if len(node.Enum) > 0 {
		values := make([]string, 0, len(node.Enum))
		for _, value := range node.Enum {
			text := fmt.Sprint(value)
			if text == "" {
				text = `""`
			}
			values = append(values, text)
		}
		return docgen.Code(strings.Join(values, " | "))
	}
	value := fmt.Sprint(node.Type)
	if value == "array" {
		element := "value"
		if node.Items != nil {
			element = strings.Trim(schemaType(node.Items), "`")
		}
		value = "array of " + element
	}
	return docgen.Code(value)
}

func yes(value bool) string {
	if value {
		return "Yes"
	}
	return ""
}
