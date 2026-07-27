package protocol

// PIR validation, in three passes over one program document:
//
//  1. Envelope schema — the program and statement grammar (kinds, required
//     fields, the closed statement union). Best-match on failure, mirroring
//     lirvalidate: a raw oneOf fan-out is re-narrowed to the failing
//     statement's kind variant and reported with the statement's index.
//  2. Program semantics — cross-field rules the schema cannot express: unique
//     statement names and the result-selection rule.
//  3. Two-phase LIR — each relational statement's opaque relation is validated
//     against the independent LIR schema. PIR never learns the LIR grammar; it
//     hands each payload to ValidateLIRJSON. Catalog statements have no
//     relation. Backward-reference and evolving-catalog resolution are the
//     engine's job at bind time, not here.

import (
	"bytes"
	"encoding/json"
	"fmt"
	"sync"

	"github.com/google/jsonschema-go/jsonschema"

	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

// statementDefs maps a statement `kind` to its named variant in the schema.
var statementDefs = map[string]string{
	"query":                       "QueryStatement",
	"create":                      "CreateStatement",
	"update":                      "UpdateStatement",
	"delete":                      "DeleteStatement",
	"create_table":                "CreateTableStatement",
	"rename_table":                "RenameTableStatement",
	"delete_table":                "DeleteTableStatement",
	"create_column":               "CreateColumnStatement",
	"rename_column":               "RenameColumnStatement",
	"change_column_default":       "ChangeColumnDefaultStatement",
	"delete_column":               "DeleteColumnStatement",
	"create_index":                "CreateIndexStatement",
	"delete_index":                "DeleteIndexStatement",
	"start_index_build":           "StartIndexBuildStatement",
	"start_column_replacement":    "StartColumnReplacementStatement",
	"start_constraint_validation": "StartConstraintValidationStatement",
}

var resolvePIRSchema = sync.OnceValues(func() (*jsonschema.Resolved, error) {
	var schema jsonschema.Schema
	if err := json.Unmarshal(pirSchemaJSON, &schema); err != nil {
		return nil, fmt.Errorf("protocol: decode embedded PIR schema: %w", err)
	}
	resolved, err := schema.Resolve(nil)
	if err != nil {
		return nil, fmt.Errorf("protocol: resolve embedded PIR schema: %w", err)
	}
	return resolved, nil
})

// resolveStatementVariants compiles one standalone resolved schema per
// statement kind, for best-match error reporting.
var resolveStatementVariants = sync.OnceValues(func() (map[string]*jsonschema.Resolved, error) {
	out := make(map[string]*jsonschema.Resolved, len(statementDefs))
	for kind, def := range statementDefs {
		var doc jsonschema.Schema
		if err := json.Unmarshal(pirSchemaJSON, &doc); err != nil {
			return nil, fmt.Errorf("protocol: decode embedded PIR schema: %w", err)
		}
		wrapper := &jsonschema.Schema{Defs: doc.Defs, Ref: "#/$defs/" + def}
		resolved, err := wrapper.Resolve(nil)
		if err != nil {
			return nil, fmt.Errorf("protocol: resolve PIR variant %s: %w", def, err)
		}
		out[kind] = resolved
	}
	return out, nil
})

// ValidatePIRJSON validates one raw program document: envelope grammar,
// program semantics, and each statement's LIR relation against the LIR schema.
func ValidatePIRJSON(raw []byte) error {
	dec := json.NewDecoder(bytes.NewReader(raw))
	var instance any
	if err := dec.Decode(&instance); err != nil {
		return fmt.Errorf("protocol: invalid PIR JSON: %w", err)
	}
	if err := ensureJSONEOF(dec); err != nil {
		return err
	}
	resolved, err := resolvePIRSchema()
	if err != nil {
		return err
	}
	if err := resolved.Validate(instance); err != nil {
		return pirValidationDetail(instance, fmt.Errorf("protocol: PIR schema validation failed: %w", err))
	}

	var w pirwire.Program
	if err := json.Unmarshal(raw, &w); err != nil {
		return fmt.Errorf("protocol: decode PIR: %w", err)
	}
	if err := checkProgramSemantics(w); err != nil {
		return err
	}
	for _, s := range w.Statements {
		name, rel := statementNameAndRelation(s)
		if rel == nil {
			continue
		}
		if err := ValidateLIRJSON(rel); err != nil {
			return fmt.Errorf("protocol: statement %q relation: %w", name, err)
		}
	}
	return nil
}

// checkProgramSemantics enforces the cross-field rules: statement names are
// unique, and the result selector is present when required and names a
// relational statement.
func checkProgramSemantics(w pirwire.Program) error {
	seen := make(map[string]bool, len(w.Statements))
	relational := make(map[string]bool, len(w.Statements))
	for _, s := range w.Statements {
		name, rel := statementNameAndRelation(s)
		if seen[name] {
			return fmt.Errorf("protocol: duplicate statement name %q", name)
		}
		seen[name] = true
		if rel != nil {
			relational[name] = true
		}
	}
	if w.Result == nil {
		if len(relational) == 0 || len(w.Statements) == 1 {
			return nil
		}
		return fmt.Errorf("protocol: a program with %d statements including relational work must name its result", len(w.Statements))
	}
	if !seen[*w.Result] {
		return fmt.Errorf("protocol: result names unknown statement %q", *w.Result)
	}
	if !relational[*w.Result] {
		return fmt.Errorf("protocol: result names catalog statement %q, which has no relation", *w.Result)
	}
	return nil
}

// pirValidationDetail turns a raw oneOf fan-out into the failing statement's
// kind-matched error with its array index, mirroring lirvalidate. When nothing
// statement-shaped explains the failure, the original error stands.
func pirValidationDetail(instance any, original error) error {
	variants, err := resolveStatementVariants()
	if err != nil {
		return original
	}
	doc, ok := instance.(map[string]any)
	if !ok {
		return original
	}
	stmts, ok := doc["statements"].([]any)
	if !ok {
		return original
	}
	for i, raw := range stmts {
		stmt, ok := raw.(map[string]any)
		if !ok {
			return fmt.Errorf("protocol: statement %d: not an object", i)
		}
		kind, _ := stmt["kind"].(string)
		if kind == "" {
			return fmt.Errorf("protocol: statement %d: missing \"kind\"", i)
		}
		variant, known := variants[kind]
		if !known {
			return fmt.Errorf("protocol: statement %d: unknown kind %q", i, kind)
		}
		if err := variant.Validate(stmt); err != nil {
			label := kind
			if name, ok := stmt["name"].(string); ok && name != "" {
				label = fmt.Sprintf("%q (%s)", name, kind)
			}
			return fmt.Errorf("protocol: statement %d %s: %w", i, label, err)
		}
	}
	return original
}
