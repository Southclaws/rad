// Command openapidoc renders Rad's OpenAPI contract as an MDX reference.
package main

import (
	"flag"
	"fmt"
	"log"
	"os"
	"sort"
	"strconv"
	"strings"

	"github.com/Southclaws/rad/tools/internal/docgen"
	"github.com/ogen-go/ogen"
	"github.com/ogen-go/ogen/jsonschema"
	"github.com/ogen-go/ogen/openapi"
	openapiparser "github.com/ogen-go/ogen/openapi/parser"
)

type page struct {
	Description string
	Groups      []operationGroup
	Schemas     []schemaDoc
}

type operationGroup struct {
	Name        string
	Description string
	Operations  []operationDoc
}

type operationDoc struct {
	Method      string
	Path        string
	Summary     string
	Description string
	Parameters  []parameterDoc
	Bodies      []bodyDoc
	Responses   []responseDoc
}

type parameterDoc struct {
	Name        string
	Location    string
	Type        string
	Required    string
	Description string
}

type bodyDoc struct {
	ContentType string
	Type        string
	Required    string
	Description string
}

type responseDoc struct {
	Status      string
	Type        string
	Description string
}

type schemaDoc struct {
	Name        string
	Description string
	Properties  []propertyDoc
	Variants    []string
}

type propertyDoc struct {
	Name        string
	Type        string
	Required    string
	Description string
}

const pageTemplate = `---
title: HTTP API
description: Endpoints, request bodies, responses, and data shapes exposed by Rad.
---

{{ .Description }}

Errors use RFC 7807 Problem Details. Branch on the stable {{ code "code" }} field rather than parsing an error message.

## Endpoints
{{- range .Groups }}

### {{ .Name }}

{{ .Description }}

| Method | Path | Purpose |
| --- | --- | --- |
{{- range .Operations }}
| {{ code .Method }} | {{ code .Path }} | {{ table .Summary }} |
{{- end }}
{{- range .Operations }}

#### {{ code .Method }} {{ code .Path }}

{{ .Description }}
{{- if .Parameters }}

**Parameters**

| Name | In | Type | Required | Description |
| --- | --- | --- | --- | --- |
{{- range .Parameters }}
| {{ code .Name }} | {{ code .Location }} | {{ .Type }} | {{ .Required }} | {{ table .Description }} |
{{- end }}
{{- end }}
{{- if .Bodies }}

**Request body**

| Content type | Type | Required | Description |
| --- | --- | --- | --- |
{{- range .Bodies }}
| {{ code .ContentType }} | {{ .Type }} | {{ .Required }} | {{ table .Description }} |
{{- end }}
{{- end }}

**Responses**

| Status | Body | Meaning |
| --- | --- | --- |
{{- range .Responses }}
| {{ code .Status }} | {{ .Type }} | {{ table .Description }} |
{{- end }}
{{- end }}
{{- end }}

## Data shapes

These reusable objects appear in request bodies, responses, and error details.
{{- range .Schemas }}

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
`

func main() {
	log.SetFlags(0)
	log.SetPrefix("openapidoc: ")

	input := flag.String("input", "api/openapi.yaml", "OpenAPI document to read")
	output := flag.String("output", "home/content/docs/reference/http-api.mdx", "MDX page to write")
	flag.Parse()

	source, err := os.ReadFile(*input)
	if err != nil {
		log.Fatalf("read %s: %v", *input, err)
	}
	result, err := render(source)
	if err != nil {
		log.Fatal(err)
	}
	if err := docgen.Write(*output, result); err != nil {
		log.Fatal(err)
	}
}

func render(source []byte) ([]byte, error) {
	raw, err := ogen.Parse(source)
	if err != nil {
		return nil, fmt.Errorf("parse OpenAPI YAML: %w", err)
	}
	api, err := openapiparser.Parse(raw, openapiparser.Settings{
		InferTypes:                true,
		AllowCrossTypeConstraints: true,
	})
	if err != nil {
		return nil, fmt.Errorf("parse OpenAPI document: %w", err)
	}

	result := buildPage(api)
	return docgen.Render("openapi", pageTemplate, result, nil)
}

func buildPage(api *openapi.API) page {
	result := page{
		Description: firstParagraph(api.Info.Description),
	}

	operationsByTag := map[string][]*openapi.Operation{}
	for _, operation := range api.Operations {
		tag := "Other"
		if len(operation.Tags) > 0 {
			tag = operation.Tags[0]
		}
		operationsByTag[tag] = append(operationsByTag[tag], operation)
	}
	for _, tag := range api.Tags {
		operations := operationsByTag[tag.Name]
		if len(operations) == 0 {
			continue
		}
		group := operationGroup{
			Name:        titleCase(tag.Name),
			Description: docgen.Paragraphs(tag.Description),
			Operations:  buildOperations(operations),
		}
		result.Groups = append(result.Groups, group)
		delete(operationsByTag, tag.Name)
	}

	leftovers := make([]string, 0, len(operationsByTag))
	for name := range operationsByTag {
		leftovers = append(leftovers, name)
	}
	sort.Strings(leftovers)
	for _, name := range leftovers {
		result.Groups = append(result.Groups, operationGroup{
			Name:       titleCase(name),
			Operations: buildOperations(operationsByTag[name]),
		})
	}

	names := make([]string, 0, len(api.Components.Schemas))
	for name := range api.Components.Schemas {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		result.Schemas = append(result.Schemas, buildSchema(name, api.Components.Schemas[name]))
	}
	return result
}

func buildOperations(operations []*openapi.Operation) []operationDoc {
	sort.Slice(operations, func(i, j int) bool {
		left := operations[i].Path.String() + operations[i].HTTPMethod
		right := operations[j].Path.String() + operations[j].HTTPMethod
		return left < right
	})

	result := make([]operationDoc, 0, len(operations))
	for _, operation := range operations {
		doc := operationDoc{
			Method:      strings.ToUpper(operation.HTTPMethod),
			Path:        operation.Path.String(),
			Summary:     operation.Summary,
			Description: docgen.Paragraphs(operation.Description),
		}
		if doc.Description == "" {
			doc.Description = doc.Summary
		}
		for _, parameter := range operation.Parameters {
			doc.Parameters = append(doc.Parameters, parameterDoc{
				Name:        parameter.Name,
				Location:    parameter.In.String(),
				Type:        schemaType(parameter.Schema),
				Required:    yes(parameter.Required),
				Description: parameter.Description,
			})
		}
		if operation.RequestBody != nil {
			contentTypes := sortedKeys(operation.RequestBody.Content)
			for _, contentType := range contentTypes {
				doc.Bodies = append(doc.Bodies, bodyDoc{
					ContentType: contentType,
					Type:        schemaType(operation.RequestBody.Content[contentType].Schema),
					Required:    yes(operation.RequestBody.Required),
					Description: operation.RequestBody.Description,
				})
			}
		}
		doc.Responses = buildResponses(operation.Responses)
		result = append(result, doc)
	}
	return result
}

func buildResponses(responses openapi.Responses) []responseDoc {
	var result []responseDoc
	statusCodes := make([]int, 0, len(responses.StatusCode))
	for status := range responses.StatusCode {
		statusCodes = append(statusCodes, status)
	}
	sort.Ints(statusCodes)
	for _, status := range statusCodes {
		result = append(result, responseDocument(strconv.Itoa(status), responses.StatusCode[status]))
	}
	for index, response := range responses.Pattern {
		if response != nil {
			result = append(result, responseDocument(strconv.Itoa(index+1)+"XX", response))
		}
	}
	if responses.Default != nil {
		result = append(result, responseDocument("default", responses.Default))
	}
	return result
}

func responseDocument(status string, response *openapi.Response) responseDoc {
	body := "No body"
	contentTypes := sortedKeys(response.Content)
	if len(contentTypes) > 0 {
		parts := make([]string, 0, len(contentTypes))
		for _, contentType := range contentTypes {
			parts = append(parts, contentType+" "+schemaType(response.Content[contentType].Schema))
		}
		body = strings.Join(parts, ", ")
	}
	return responseDoc{
		Status:      status,
		Type:        body,
		Description: response.Description,
	}
}

func buildSchema(name string, schema *jsonschema.Schema) schemaDoc {
	result := schemaDoc{
		Name:        name,
		Description: docgen.Paragraphs(schema.Description),
	}
	if result.Description == "" {
		result.Description = schema.Summary
	}
	for _, variant := range schema.OneOf {
		result.Variants = append(result.Variants, schemaType(variant))
	}
	for _, property := range schema.Properties {
		result.Properties = append(result.Properties, propertyDoc{
			Name:        property.Name,
			Type:        schemaType(property.Schema),
			Required:    yes(property.Required),
			Description: propertyDescription(property.Description, property.Schema),
		})
	}
	return result
}

func propertyDescription(description string, schema *jsonschema.Schema) string {
	var details []string
	if schema == nil {
		return description
	}
	if schema.DefaultSet {
		details = append(details, "Default: "+fmt.Sprint(schema.Default)+".")
	}
	if schema.Format != "" {
		details = append(details, "Format: "+schema.Format+".")
	}
	if schema.MinLength != nil {
		details = append(details, fmt.Sprintf("Minimum length: %d.", *schema.MinLength))
	}
	if schema.MinItems != nil {
		details = append(details, fmt.Sprintf("Minimum items: %d.", *schema.MinItems))
	}
	if schema.UniqueItems {
		details = append(details, "Items must be unique.")
	}
	return strings.TrimSpace(strings.Join(append([]string{description}, details...), " "))
}

func schemaType(schema *jsonschema.Schema) string {
	if schema == nil {
		return "Any JSON value"
	}
	if !schema.Ref.IsZero() {
		return docgen.Code(referenceName(schema.Ref.String()))
	}
	if schema.ConstSet {
		return docgen.Code(fmt.Sprint(schema.Const))
	}
	if len(schema.Enum) > 0 {
		values := make([]string, 0, len(schema.Enum))
		for _, value := range schema.Enum {
			values = append(values, fmt.Sprint(value))
		}
		return docgen.Code(strings.Join(values, " | "))
	}
	if len(schema.OneOf) > 0 {
		values := make([]string, 0, len(schema.OneOf))
		for _, variant := range schema.OneOf {
			values = append(values, strings.Trim(schemaType(variant), "`"))
		}
		return docgen.Code(strings.Join(values, " | "))
	}

	value := schema.Type.String()
	if schema.Type == jsonschema.Array {
		value = "array"
		if schema.Item != nil {
			value += " of " + strings.Trim(schemaType(schema.Item), "`")
		} else if len(schema.Items) == 1 {
			value += " of " + strings.Trim(schemaType(schema.Items[0]), "`")
		}
	}
	if value == "" {
		value = "value"
	}
	if schema.Nullable {
		value += " or null"
	}
	return docgen.Code(value)
}

func firstParagraph(value string) string {
	value = docgen.Paragraphs(value)
	if before, _, ok := strings.Cut(value, "\n\n"); ok {
		return before
	}
	return value
}

func referenceName(reference string) string {
	index := strings.LastIndex(reference, "/")
	if index == -1 {
		return strings.TrimPrefix(reference, "#")
	}
	return reference[index+1:]
}

func sortedKeys[V any](values map[string]V) []string {
	keys := make([]string, 0, len(values))
	for key := range values {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	return keys
}

func yes(value bool) string {
	if value {
		return "Yes"
	}
	return ""
}

func titleCase(value string) string {
	if value == "" {
		return value
	}
	return strings.ToUpper(value[:1]) + value[1:]
}
