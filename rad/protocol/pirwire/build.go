package pirwire

// Hand-written, colocated with the Schemancer-generated PIR wire types: the
// ergonomic construction surface for programs. Statements carry an opaque
// `Relation` (a marshalled LIR document); callers build the LIR with the
// lirwire builders and json.Marshal it into the Relation. Regeneration
// rewrites only pir.go, never this file.

func Query(name string, relation Relation) Statement {
	return Statement{&QueryStatement{Kind: "query", Name: name, Relation: relation}}
}

func Create(name, table string, relation Relation) Statement {
	return Statement{&CreateStatement{Kind: "create", Name: name, Table: table, Relation: relation}}
}

func Update(name, table string, relation Relation) Statement {
	return Statement{&UpdateStatement{Kind: "update", Name: name, Table: table, Relation: relation}}
}

func Delete(name, table string, relation Relation) Statement {
	return Statement{&DeleteStatement{Kind: "delete", Name: name, Table: table, Relation: relation}}
}

// Prog assembles a program. An empty result selects nothing explicitly — valid
// only for a single-statement program, which the server treats as its own
// result.
func Prog(result string, statements ...Statement) Program {
	p := Program{Statements: statements}
	if result != "" {
		p.Result = &result
	}
	return p
}
