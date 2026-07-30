package protocol

import (
	_ "embed"
	"encoding/json"
	"fmt"

	"github.com/Southclaws/rad/clients/go/protocol/pirwire"
)

// The repository's protocol/pir.schema.yaml is the authored source of truth.
// `task generate:protocol` converts it to the embedded pir.schema.json, emits
// the Schemancer wire model, and publishes the website copy together.

//go:embed pir.schema.json
var pirSchemaJSON []byte

// MarshalProgram encodes a wire program and validates it against the PIR
// schema (envelope grammar plus each statement's opaque LIR relation against
// the LIR schema). The program is a Schemancer-generated union, so there is
// nothing to convert.
func MarshalProgram(p pirwire.Program) ([]byte, error) {
	raw, err := json.Marshal(p)
	if err != nil {
		return nil, fmt.Errorf("protocol: encode PIR: %w", err)
	}
	if err := ValidatePIRJSON(raw); err != nil {
		return nil, err
	}
	return raw, nil
}

// UnmarshalProgram validates a raw program (envelope plus each statement's LIR
// payload against the LIR schema), then decodes it into the generated union
// types.
func UnmarshalProgram(raw []byte) (pirwire.Program, error) {
	if err := ValidatePIRJSON(raw); err != nil {
		return pirwire.Program{}, err
	}
	var p pirwire.Program
	if err := json.Unmarshal(raw, &p); err != nil {
		return pirwire.Program{}, fmt.Errorf("protocol: decode PIR: %w", err)
	}
	return p, nil
}

// statementNameAndRelation pulls the name and optional raw LIR payload out of
// a wire statement. Catalog statements return a nil relation.
func statementNameAndRelation(s pirwire.Statement) (string, json.RawMessage) {
	switch x := s.StatementUnion.(type) {
	case *pirwire.QueryStatement:
		return x.Name, x.Relation
	case *pirwire.CreateStatement:
		return x.Name, x.Relation
	case *pirwire.UpdateStatement:
		return x.Name, x.Relation
	case *pirwire.DeleteStatement:
		return x.Name, x.Relation
	case *pirwire.CreateTableStatement:
		return x.Name, nil
	case *pirwire.RenameTableStatement:
		return x.Name, nil
	case *pirwire.DeleteTableStatement:
		return x.Name, nil
	case *pirwire.CreateColumnStatement:
		return x.Name, nil
	case *pirwire.RenameColumnStatement:
		return x.Name, nil
	case *pirwire.ChangeColumnDefaultStatement:
		return x.Name, nil
	case *pirwire.DeleteColumnStatement:
		return x.Name, nil
	case *pirwire.CreateIndexStatement:
		return x.Name, nil
	case *pirwire.DeleteIndexStatement:
		return x.Name, nil
	case *pirwire.StartIndexBuildStatement:
		return x.Name, nil
	case *pirwire.StartColumnReplacementStatement:
		return x.Name, nil
	case *pirwire.StartConstraintValidationStatement:
		return x.Name, nil
	default:
		return "", nil
	}
}
