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

func CreateTable(name string, table TableDefinition) Statement {
	return Statement{&CreateTableStatement{Kind: "create_table", Name: name, Table: table}}
}

func RenameTable(name string, tableID SchemaID, to string) Statement {
	return Statement{&RenameTableStatement{Kind: "rename_table", Name: name, TableID: tableID, To: to}}
}

func DeleteTable(name string, tableID SchemaID) Statement {
	return Statement{&DeleteTableStatement{Kind: "delete_table", Name: name, TableID: tableID}}
}

func CreateColumn(name string, tableID SchemaID, column ColumnDefinition) Statement {
	return Statement{&CreateColumnStatement{Kind: "create_column", Name: name, TableID: tableID, Column: column}}
}

func RenameColumn(name string, tableID, columnID SchemaID, to string) Statement {
	return Statement{&RenameColumnStatement{
		Kind: "rename_column", Name: name, TableID: tableID, ColumnID: columnID, To: to,
	}}
}

func ChangeColumnDefault(
	name string,
	tableID, columnID SchemaID,
	value *ColumnDefault,
) Statement {
	return Statement{&ChangeColumnDefaultStatement{
		Kind: "change_column_default", Name: name,
		TableID: tableID, ColumnID: columnID, Default: value,
	}}
}

func DeleteColumn(name string, tableID, columnID SchemaID) Statement {
	return Statement{&DeleteColumnStatement{
		Kind: "delete_column", Name: name, TableID: tableID, ColumnID: columnID,
	}}
}

func CreateIndex(name string, tableID SchemaID, index IndexDefinition) Statement {
	return Statement{&CreateIndexStatement{Kind: "create_index", Name: name, TableID: tableID, Index: index}}
}

func DeleteIndex(name string, tableID SchemaID, index string) Statement {
	return Statement{&DeleteIndexStatement{Kind: "delete_index", Name: name, TableID: tableID, Index: index}}
}

func StartIndexBuild(
	name string,
	tableID SchemaID,
	index IndexDefinition,
	prerequisites ...TransitionID,
) Statement {
	return Statement{&StartIndexBuildStatement{
		Kind: "start_index_build", Name: name, TableID: tableID, Index: index,
		Prerequisites: prerequisites,
	}}
}

func StartColumnReplacement(
	name string,
	tableID SchemaID,
	columnID SchemaID,
	replacement ColumnReplacementDefinition,
) Statement {
	return Statement{&StartColumnReplacementStatement{
		Kind: "start_column_replacement", Name: name, TableID: tableID,
		ColumnID: columnID, Replacement: replacement,
	}}
}

func StartConstraintValidation(
	name string,
	tableID SchemaID,
	constraint ConstraintValidationDefinition,
) Statement {
	return Statement{&StartConstraintValidationStatement{
		Kind: "start_constraint_validation", Name: name, TableID: tableID,
		Constraint: constraint,
	}}
}

// WithAfter makes a transition start wait for earlier transition-start
// statements in the same program. Ordinary statement order starts work in
// order but does not wait for background completion.
func WithAfter(statement Statement, names ...StatementName) Statement {
	after := append([]StatementName{}, names...)
	switch value := statement.StatementUnion.(type) {
	case *StartIndexBuildStatement:
		value.After = after
	case *StartColumnReplacementStatement:
		value.After = after
	case *StartConstraintValidationStatement:
		value.After = after
	default:
		panic("pirwire: WithAfter requires a transition-start statement")
	}
	return statement
}

// Prog assembles a program. An empty result selects nothing explicitly. That
// is valid for a single relational statement (which selects itself) or for a
// catalog-only program (which returns null); mixed and multi-relational
// programs name their relational result explicitly.
func Prog(result string, statements ...Statement) Program {
	p := Program{Statements: statements}
	if result != "" {
		p.Result = &result
	}
	return p
}
