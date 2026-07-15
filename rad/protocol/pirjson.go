package protocol

import (
	_ "embed"
	"encoding/json"
	"fmt"

	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

// pir.schema.yaml is the authored source of truth. schemagen converts it to
// pir.schema.json (embedded below, read by Schemancer) and publishes a copy to
// the website route so the $id URL resolves.
//
//go:generate go run github.com/Southclaws/rad/tools/schemagen -yaml pir.schema.yaml -json pir.schema.json -web ../../home/public/schema/pir.json

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

// statementNameAndRelation pulls the name and raw LIR payload out of a wire
// statement without caring which kind it is — the two fields every kind shares.
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
	default:
		return "", nil
	}
}
