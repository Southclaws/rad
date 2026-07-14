package protocol

import (
	_ "embed"
	"encoding/json"
	"fmt"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

// pir.schema.yaml is the authored source of truth. schemagen converts it to
// pir.schema.json (embedded below, read by Schemancer) and publishes a copy to
// the website route so the $id URL resolves.
//
//go:generate go run github.com/Southclaws/rad/tools/schemagen -yaml pir.schema.yaml -json pir.schema.json -web ../../home/public/schema/pir.json

//go:embed pir.schema.json
var pirSchemaJSON []byte

// MarshalProgram converts the ergonomic program model to the Schemancer wire
// model and returns a schema-valid JSON document. Each statement's LIR
// relation is marshalled through the LIR wire model, so the two documents
// nest without either learning the other's grammar.
func MarshalProgram(p Program) ([]byte, error) {
	w, err := programToWire(p)
	if err != nil {
		return nil, err
	}
	raw, err := json.Marshal(w)
	if err != nil {
		return nil, fmt.Errorf("protocol: encode PIR: %w", err)
	}
	if err := ValidatePIRJSON(raw); err != nil {
		return nil, err
	}
	return raw, nil
}

// UnmarshalProgram validates a raw program (envelope plus each statement's LIR
// payload against the LIR schema), then decodes it into the ergonomic model.
func UnmarshalProgram(raw []byte) (Program, error) {
	if err := ValidatePIRJSON(raw); err != nil {
		return Program{}, err
	}
	var w pirwire.Program
	if err := json.Unmarshal(raw, &w); err != nil {
		return Program{}, fmt.Errorf("protocol: decode generated PIR types: %w", err)
	}
	return programFromWire(w)
}

func programToWire(p Program) (pirwire.Program, error) {
	stmts := make([]pirwire.Statement, len(p.Statements))
	for i, s := range p.Statements {
		raw, err := relationToRaw(s.Relation)
		if err != nil {
			return pirwire.Program{}, fmt.Errorf("protocol: statement %q: %w", s.Name, err)
		}
		var u pirwire.StatementUnion
		switch s.Kind {
		case StmtQuery:
			u = &pirwire.QueryStatement{Kind: "query", Name: s.Name, Relation: raw}
		case StmtCreate:
			u = &pirwire.CreateStatement{Kind: "create", Name: s.Name, Table: s.Table, Relation: raw}
		case StmtUpdate:
			u = &pirwire.UpdateStatement{Kind: "update", Name: s.Name, Table: s.Table, Relation: raw}
		case StmtDelete:
			u = &pirwire.DeleteStatement{Kind: "delete", Name: s.Name, Table: s.Table, Relation: raw}
		default:
			return pirwire.Program{}, fmt.Errorf("protocol: statement %q has unknown kind %q", s.Name, s.Kind)
		}
		stmts[i] = pirwire.Statement{StatementUnion: u}
	}
	w := pirwire.Program{Statements: stmts}
	if p.Result != "" {
		w.Result = &p.Result
	}
	return w, nil
}

func programFromWire(w pirwire.Program) (Program, error) {
	stmts := make([]Statement, len(w.Statements))
	for i, s := range w.Statements {
		st, err := statementFromWire(s)
		if err != nil {
			return Program{}, err
		}
		stmts[i] = st
	}
	return Program{Statements: stmts, Result: stringValue(w.Result)}, nil
}

func statementFromWire(s pirwire.Statement) (Statement, error) {
	switch x := s.StatementUnion.(type) {
	case *pirwire.QueryStatement:
		rel, err := relationFromRaw(x.Relation)
		return Statement{Name: x.Name, Kind: StmtQuery, Relation: rel}, err
	case *pirwire.CreateStatement:
		rel, err := relationFromRaw(x.Relation)
		return Statement{Name: x.Name, Kind: StmtCreate, Table: x.Table, Relation: rel}, err
	case *pirwire.UpdateStatement:
		rel, err := relationFromRaw(x.Relation)
		return Statement{Name: x.Name, Kind: StmtUpdate, Table: x.Table, Relation: rel}, err
	case *pirwire.DeleteStatement:
		rel, err := relationFromRaw(x.Relation)
		return Statement{Name: x.Name, Kind: StmtDelete, Table: x.Table, Relation: rel}, err
	default:
		return Statement{}, fmt.Errorf("protocol: unknown generated statement variant %T", s.StatementUnion)
	}
}

// relationToRaw marshals an ergonomic LIR Query to the raw bytes a statement's
// opaque relation field carries.
func relationToRaw(q Query) (json.RawMessage, error) {
	lw, err := queryToWire(q)
	if err != nil {
		return nil, err
	}
	raw, err := json.Marshal(lw)
	if err != nil {
		return nil, fmt.Errorf("encode relation: %w", err)
	}
	return raw, nil
}

// relationFromRaw decodes a statement's raw LIR payload into the ergonomic
// Query. The bytes were already schema-validated by ValidatePIRJSON's
// two-phase pass, so this only decodes.
func relationFromRaw(raw json.RawMessage) (Query, error) {
	var lw lirwire.Query
	if err := json.Unmarshal(raw, &lw); err != nil {
		return Query{}, fmt.Errorf("protocol: decode statement relation: %w", err)
	}
	return queryFromWire(lw)
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
