package protocol

import (
	"bytes"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"sync"

	"github.com/google/jsonschema-go/jsonschema"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
)

// lir.schema.yaml is the authored source of truth. schemagen converts it to
// lir.schema.json (embedded below, read by Schemancer) and publishes a copy to
// the website route so the $id URL resolves.
//
//go:generate go run github.com/Southclaws/rad/tools/schemagen -yaml lir.schema.yaml -json lir.schema.json -web ../../home/public/schema/lir.json

// lirSchemaJSON is the independent LIR contract. OpenAPI deliberately treats
// query bodies as raw JSON and does not import this schema.
//
//go:embed lir.schema.json
var lirSchemaJSON []byte

var resolveLIRSchema = sync.OnceValues(func() (*jsonschema.Resolved, error) {
	var schema jsonschema.Schema
	if err := json.Unmarshal(lirSchemaJSON, &schema); err != nil {
		return nil, fmt.Errorf("protocol: decode embedded LIR schema: %w", err)
	}
	resolved, err := schema.Resolve(nil)
	if err != nil {
		return nil, fmt.Errorf("protocol: resolve embedded LIR schema: %w", err)
	}
	return resolved, nil
})

// ValidateLIRJSON validates one raw JSON document against the current LIR
// schema. Literal bytes are decoded separately by the generated wire types, so
// schema validation never becomes the source of transported numeric values.
func ValidateLIRJSON(raw []byte) error {
	dec := json.NewDecoder(bytes.NewReader(raw))
	var instance any
	if err := dec.Decode(&instance); err != nil {
		return fmt.Errorf("protocol: invalid LIR JSON: %w", err)
	}
	if err := ensureJSONEOF(dec); err != nil {
		return err
	}
	resolved, err := resolveLIRSchema()
	if err != nil {
		return err
	}
	if err := resolved.Validate(instance); err != nil {
		return validationDetail(instance, fmt.Errorf("protocol: LIR schema validation failed: %w", err))
	}
	return nil
}

func ensureJSONEOF(dec *json.Decoder) error {
	var extra any
	err := dec.Decode(&extra)
	if errors.Is(err, io.EOF) {
		return nil
	}
	if err == nil {
		return fmt.Errorf("protocol: invalid LIR JSON: multiple JSON values")
	}
	return fmt.Errorf("protocol: invalid LIR JSON: %w", err)
}

// MarshalQuery encodes a wire query and validates the result against the LIR
// schema. The query is a Schemancer-generated union, so there is nothing to
// convert: json.Marshal already produces the canonical wire bytes.
func MarshalQuery(q lirwire.Query) ([]byte, error) {
	raw, err := json.Marshal(q)
	if err != nil {
		return nil, fmt.Errorf("protocol: encode LIR: %w", err)
	}
	if err := ValidateLIRJSON(raw); err != nil {
		return nil, err
	}
	return raw, nil
}

// UnmarshalQuery validates raw LIR against the schema, then decodes it into the
// generated union types.
func UnmarshalQuery(raw []byte) (lirwire.Query, error) {
	if err := ValidateLIRJSON(raw); err != nil {
		return lirwire.Query{}, err
	}
	var q lirwire.Query
	if err := json.Unmarshal(raw, &q); err != nil {
		return lirwire.Query{}, fmt.Errorf("protocol: decode LIR: %w", err)
	}
	return q, nil
}
