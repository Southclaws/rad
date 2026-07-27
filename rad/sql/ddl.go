package sql

import (
	"encoding/json"
	"fmt"
	"strconv"
	"strings"

	"github.com/pgplex/pgparser/nodes"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

// DDL is idempotent at the frontend: creating an object that already exists
// compiles to a no-op success, because migration tooling (ent/Atlas) is
// answered with an empty-catalog inspection and re-emits its full DDL on
// every run. Foreign-key ALTERs are accepted and dropped — the engine's
// foreign keys are create-time-only and RESTRICT-only, and no FKs at all
// beats failing every cascade delete the tooling expects.

func (p *program) lowerCreateTable(cs *nodes.CreateStmt) error {
	if cs.Relation == nil {
		return fmt.Errorf("CREATE TABLE without a name")
	}
	name := strings.ToLower(cs.Relation.Relname)
	p.tag = "CREATE TABLE"
	if _, exists := p.schema.Table(name); exists {
		p.noop = true
		return nil
	}
	if cs.Partspec != nil || cs.OfTypename != nil || (cs.InhRelations != nil && len(cs.InhRelations.Items) > 0) {
		return unsupportedf("partitioned/typed/inherited table")
	}

	var cols []pirwire.ColumnDefinition
	var pk []string
	var indexes []pirwire.IndexDefinition
	var fks []pirwire.ForeignKeyDefinition

	if cs.TableElts == nil {
		return unsupportedf("CREATE TABLE with no columns")
	}
	for _, item := range cs.TableElts.Items {
		switch elt := item.(type) {
		case *nodes.ColumnDef:
			col, colPK, colIndexes, colFKs, err := p.lowerColumnDef(name, elt)
			if err != nil {
				return err
			}
			cols = append(cols, col)
			if colPK {
				pk = append(pk, string(col.Name))
			}
			indexes = append(indexes, colIndexes...)
			fks = append(fks, colFKs...)
		case *nodes.Constraint:
			switch elt.Contype {
			case nodes.CONSTR_PRIMARY:
				names, err := stringItems(elt.Keys)
				if err != nil {
					return err
				}
				pk = names
			case nodes.CONSTR_UNIQUE:
				names, err := stringItems(elt.Keys)
				if err != nil {
					return err
				}
				indexes = append(indexes, uniqueIndex(name, elt.Conname, names))
			case nodes.CONSTR_FOREIGN:
				fk, err := p.foreignKey(name, elt, nil)
				if err != nil {
					return err
				}
				fks = append(fks, *fk)
			case nodes.CONSTR_CHECK:
				// Dropped: the engine has no check constraints.
			default:
				return unsupportedf("table constraint %d", elt.Contype)
			}
		default:
			return unsupportedf("table element %T", item)
		}
	}
	if len(pk) == 0 {
		return unsupportedf("table without a PRIMARY KEY")
	}
	// Primary key columns are implicitly NOT NULL.
	pkSet := map[string]bool{}
	for _, k := range pk {
		pkSet[k] = true
	}
	for i := range cols {
		if pkSet[string(cols[i].Name)] {
			cols[i].Nullable = nil
		}
	}

	def := pirwire.TableDefinition{
		Name:       pirwire.CatalogName(name),
		Columns:    cols,
		PrimaryKey: catalogNames(pk),
	}
	if len(indexes) > 0 {
		def.Indexes = indexes
	}
	// Foreign keys are parsed but never installed — the engine's FKs are
	// RESTRICT-only, while Postgres DDL from migration tooling relies on
	// ON DELETE CASCADE/SET NULL. No enforcement at all diverges less than
	// rejecting deletes the application performs routinely (matching the
	// ALTER TABLE ADD CONSTRAINT handling).
	_ = fks
	p.statements = append(p.statements, pirwire.CreateTable("ddl", def))
	return nil
}

func (p *program) lowerColumnDef(tableName string, cd *nodes.ColumnDef) (pirwire.ColumnDefinition, bool, []pirwire.IndexDefinition, []pirwire.ForeignKeyDefinition, error) {
	scalar, format, err := typeNameOf(cd.TypeName)
	if err != nil {
		return pirwire.ColumnDefinition{}, false, nil, nil, err
	}
	colName := strings.ToLower(cd.Colname)
	col := pirwire.ColumnDefinition{
		Name: pirwire.CatalogName(colName),
		Type: pirwire.ColumnType(scalar),
	}
	if format != "" {
		col.Format = &format
	}
	nullable := !cd.IsNotNull
	isPK := false
	var indexes []pirwire.IndexDefinition
	var fks []pirwire.ForeignKeyDefinition

	handleDefault := func(raw nodes.Node) error {
		def, err := literalDefault(raw, scalar, format)
		if err != nil {
			return err
		}
		if def != nil {
			col.Default = def
		}
		return nil
	}
	if cd.RawDefault != nil {
		if err := handleDefault(cd.RawDefault); err != nil {
			return col, false, nil, nil, err
		}
	}
	if cd.Constraints != nil {
		for _, item := range cd.Constraints.Items {
			con, ok := item.(*nodes.Constraint)
			if !ok {
				return col, false, nil, nil, fmt.Errorf("unexpected column constraint %T", item)
			}
			switch con.Contype {
			case nodes.CONSTR_NOTNULL:
				nullable = false
			case nodes.CONSTR_NULL:
				nullable = true
			case nodes.CONSTR_PRIMARY:
				isPK = true
				nullable = false
			case nodes.CONSTR_UNIQUE:
				indexes = append(indexes, uniqueIndex(tableName, con.Conname, []string{colName}))
			case nodes.CONSTR_DEFAULT:
				if err := handleDefault(con.RawExpr); err != nil {
					return col, false, nil, nil, err
				}
			case nodes.CONSTR_FOREIGN:
				fk, err := p.foreignKey(tableName, con, []string{colName})
				if err != nil {
					return col, false, nil, nil, err
				}
				fks = append(fks, *fk)
			case nodes.CONSTR_CHECK:
				// Dropped.
			default:
				return col, false, nil, nil, unsupportedf("column constraint %d", con.Contype)
			}
		}
	}
	if nullable {
		col.Nullable = &nullable
	}
	return col, isPK, indexes, fks, nil
}

// literalDefault maps a DDL default expression: literals become literal
// defaults, volatile time functions are dropped (clients supply values;
// microsecond timestamps have no engine generator), anything else is
// dropped rather than rejected so tooling-emitted DDL keeps flowing.
func literalDefault(raw nodes.Node, scalar lirwire.ScalarType, format string) (*pirwire.ColumnDefault, error) {
	switch v := raw.(type) {
	case *nodes.A_Const:
		if v.Isnull {
			return nil, nil
		}
		var payload any
		switch val := v.Val.(type) {
		case *nodes.Integer:
			payload = val.Ival
		case *nodes.Float:
			payload = json.RawMessage(val.Fval)
		case *nodes.String:
			// Postgres assignment-casts quoted defaults ('0' on bigint);
			// coerce to the column scalar, dropping the default when the
			// text does not parse.
			coerced, ok := coerceDefaultString(val.Str, scalar, format)
			if !ok {
				return nil, nil
			}
			payload = coerced
		case *nodes.Boolean:
			payload = val.Boolval
		default:
			return nil, nil
		}
		rawJSON, err := json.Marshal(payload)
		if err != nil {
			return nil, err
		}
		return &pirwire.ColumnDefault{ColumnDefaultUnion: &pirwire.LiteralDefault{Kind: "literal", Value: rawJSON}}, nil
	case *nodes.TypeCast:
		return literalDefault(v.Arg, scalar, format)
	case *nodes.FuncCall, *nodes.SQLValueFunction:
		return nil, nil
	}
	return nil, nil
}

func coerceDefaultString(s string, scalar lirwire.ScalarType, format string) (any, bool) {
	if IsTimeFormat(format) {
		us, err := ParseTimestamp(s)
		if err != nil {
			return nil, false
		}
		return us, true
	}
	switch scalar {
	case lirwire.ScalarTypeInt64:
		i, err := strconv.ParseInt(strings.TrimSpace(s), 10, 64)
		if err != nil {
			return nil, false
		}
		return i, true
	case lirwire.ScalarTypeFloat64:
		f, err := strconv.ParseFloat(strings.TrimSpace(s), 64)
		if err != nil {
			return nil, false
		}
		return f, true
	case lirwire.ScalarTypeBool:
		b, err := strconv.ParseBool(strings.TrimSpace(s))
		if err != nil {
			return nil, false
		}
		return b, true
	}
	return s, true
}

func uniqueIndex(tableName, conname string, cols []string) pirwire.IndexDefinition {
	name := conname
	if name == "" {
		name = tableName + "_" + strings.Join(cols, "_") + "_key"
	}
	unique := true
	return pirwire.IndexDefinition{
		Name:    pirwire.CatalogName(strings.ToLower(name)),
		Columns: catalogNames(cols),
		Unique:  &unique,
	}
}

// foreignKey resolves a REFERENCES clause. The referenced columns must be
// the target's full primary key; when omitted they resolve from the current
// schema snapshot (or, self-referentially, the table being created).
func (p *program) foreignKey(tableName string, con *nodes.Constraint, cols []string) (*pirwire.ForeignKeyDefinition, error) {
	if con.Pktable == nil {
		return nil, fmt.Errorf("REFERENCES without a table")
	}
	refTable := strings.ToLower(con.Pktable.Relname)
	if cols == nil {
		names, err := stringItems(con.FkAttrs)
		if err != nil {
			return nil, err
		}
		cols = names
	}
	refCols, err := stringItems(con.PkAttrs)
	if err != nil {
		return nil, err
	}
	if len(refCols) == 0 {
		if t, ok := p.schema.Table(refTable); ok {
			refCols = t.PrimaryKey
		} else if refTable == tableName {
			return nil, unsupportedf("self-referential FK without explicit columns")
		} else {
			return nil, fmt.Errorf("referenced table %q does not exist", refTable)
		}
	}
	name := con.Conname
	if name == "" {
		name = tableName + "_" + strings.Join(cols, "_") + "_fkey"
	}
	return &pirwire.ForeignKeyDefinition{
		Name:       pirwire.CatalogName(strings.ToLower(name)),
		Columns:    catalogNames(cols),
		RefTable:   pirwire.CatalogName(refTable),
		RefColumns: catalogNames(refCols),
	}, nil
}

func stringItems(list *nodes.List) ([]string, error) {
	if list == nil {
		return nil, nil
	}
	out := make([]string, 0, len(list.Items))
	for _, item := range list.Items {
		s, ok := item.(*nodes.String)
		if !ok {
			return nil, fmt.Errorf("unexpected name node %T", item)
		}
		out = append(out, strings.ToLower(s.Str))
	}
	return out, nil
}

func catalogNames(in []string) []pirwire.CatalogName {
	out := make([]pirwire.CatalogName, len(in))
	for i, s := range in {
		out[i] = pirwire.CatalogName(s)
	}
	return out
}

func (p *program) lowerCreateIndex(is *nodes.IndexStmt) error {
	p.tag = "CREATE INDEX"
	if is.Relation == nil {
		return fmt.Errorf("CREATE INDEX without a table")
	}
	tableName := strings.ToLower(is.Relation.Relname)
	table, ok := p.schema.Table(tableName)
	if !ok {
		return fmt.Errorf("relation %q does not exist", tableName)
	}
	var cols []string
	for _, item := range is.IndexParams.Items {
		el, ok := item.(*nodes.IndexElem)
		if !ok || el.Name == "" {
			return unsupportedf("expression index")
		}
		cols = append(cols, strings.ToLower(el.Name))
	}
	if is.WhereClause != nil {
		return unsupportedf("partial index")
	}
	name := strings.ToLower(is.Idxname)
	if name == "" {
		name = tableName + "_" + strings.Join(cols, "_") + "_idx"
	}
	for _, idx := range table.Indexes {
		if idx.Name == name {
			p.noop = true
			return nil
		}
	}
	def := pirwire.IndexDefinition{Name: pirwire.CatalogName(name), Columns: catalogNames(cols)}
	if is.Unique {
		u := true
		def.Unique = &u
	}
	p.statements = append(p.statements, pirwire.CreateIndex("ddl", pirwire.SchemaID(table.ID), def))
	return nil
}

func (p *program) lowerDrop(ds *nodes.DropStmt) error {
	switch nodes.ObjectType(ds.RemoveType) {
	case nodes.OBJECT_TABLE:
		p.tag = "DROP TABLE"
		for _, obj := range ds.Objects.Items {
			name, err := lastName(obj)
			if err != nil {
				return err
			}
			table, ok := p.schema.Table(name)
			if !ok {
				if ds.Missing_ok {
					continue
				}
				return fmt.Errorf("table %q does not exist", name)
			}
			p.statements = append(p.statements, pirwire.DeleteTable("ddl_"+name, pirwire.SchemaID(table.ID)))
		}
	case nodes.OBJECT_INDEX:
		p.tag = "DROP INDEX"
		for _, obj := range ds.Objects.Items {
			name, err := lastName(obj)
			if err != nil {
				return err
			}
			table, idxName := p.schema.findIndex(name)
			if table == nil {
				if ds.Missing_ok {
					continue
				}
				return fmt.Errorf("index %q does not exist", name)
			}
			p.statements = append(p.statements, pirwire.DeleteIndex("ddl_"+idxName, pirwire.SchemaID(table.ID), idxName))
		}
	default:
		return unsupportedf("DROP object type %d", ds.RemoveType)
	}
	if len(p.statements) == 0 {
		p.noop = true
	}
	return nil
}

func lastName(obj nodes.Node) (string, error) {
	switch v := obj.(type) {
	case *nodes.String:
		return strings.ToLower(v.Str), nil
	case *nodes.List:
		if len(v.Items) == 0 {
			return "", fmt.Errorf("empty object name")
		}
		return lastName(v.Items[len(v.Items)-1])
	}
	return "", fmt.Errorf("unexpected object name %T", obj)
}

func (p *program) lowerAlterTable(at *nodes.AlterTableStmt) error {
	p.tag = "ALTER TABLE"
	if at.Relation == nil {
		return fmt.Errorf("ALTER TABLE without a name")
	}
	tableName := strings.ToLower(at.Relation.Relname)
	table, ok := p.schema.Table(tableName)
	if !ok {
		if at.Missing_ok {
			p.noop = true
			return nil
		}
		return fmt.Errorf("relation %q does not exist", tableName)
	}
	for _, item := range at.Cmds.Items {
		cmd, ok := item.(*nodes.AlterTableCmd)
		if !ok {
			return fmt.Errorf("unexpected ALTER TABLE command %T", item)
		}
		switch nodes.AlterTableType(cmd.Subtype) {
		case nodes.AT_AddConstraint:
			con, ok := cmd.Def.(*nodes.Constraint)
			if !ok {
				return fmt.Errorf("ADD CONSTRAINT without a constraint")
			}
			switch con.Contype {
			case nodes.CONSTR_FOREIGN:
				// Accepted and dropped (see the package note above).
			case nodes.CONSTR_UNIQUE:
				names, err := stringItems(con.Keys)
				if err != nil {
					return err
				}
				idx := uniqueIndex(tableName, con.Conname, names)
				p.statements = append(p.statements, pirwire.CreateIndex("ddl_"+string(idx.Name), pirwire.SchemaID(table.ID), idx))
			default:
				return unsupportedf("ADD CONSTRAINT type %d", con.Contype)
			}
		case nodes.AT_AddColumn:
			cd, ok := cmd.Def.(*nodes.ColumnDef)
			if !ok {
				return fmt.Errorf("ADD COLUMN without a definition")
			}
			if _, exists := table.Column(strings.ToLower(cd.Colname)); exists {
				continue
			}
			col, _, _, _, err := p.lowerColumnDef(tableName, cd)
			if err != nil {
				return err
			}
			p.statements = append(p.statements, pirwire.CreateColumn("ddl_"+string(col.Name), pirwire.SchemaID(table.ID), col))
		case nodes.AT_DropColumn:
			col, exists := table.Column(strings.ToLower(cmd.Name))
			if !exists {
				if cmd.Missing_ok {
					continue
				}
				return fmt.Errorf("column %q does not exist", cmd.Name)
			}
			p.statements = append(p.statements, pirwire.DeleteColumn("ddl_"+col.Name, pirwire.SchemaID(table.ID), pirwire.SchemaID(col.ID)))
		case nodes.AT_DropConstraint:
			// FKs were never installed; dropping one is trivially done.
		case nodes.AT_ColumnDefault:
			// SET/DROP DEFAULT: no catalog statement exists; accepted.
		default:
			return unsupportedf("ALTER TABLE command %d", cmd.Subtype)
		}
	}
	if len(p.statements) == 0 {
		p.noop = true
	}
	return nil
}

// lowerTruncate maps TRUNCATE onto delete-everything: a delete statement
// whose relation is the table's primary key projection.
func (p *program) lowerTruncate(ts *nodes.TruncateStmt) error {
	p.tag = "TRUNCATE TABLE"
	for _, item := range ts.Relations.Items {
		rv, ok := item.(*nodes.RangeVar)
		if !ok {
			return fmt.Errorf("unexpected TRUNCATE target %T", item)
		}
		name := strings.ToLower(rv.Relname)
		table, ok := p.schema.Table(name)
		if !ok {
			return fmt.Errorf("relation %q does not exist", name)
		}
		c := p.newRelCC()
		label := c.scope(name)
		scan := c.add(lirwire.Scan(table.Name, label))
		var fields []lirwire.Field
		for _, pk := range table.PrimaryKey {
			fields = append(fields, lirwire.Field{As: pk, Expr: lirwire.Col(label, pk)})
		}
		shaped := c.add(lirwire.Project(scan, c.scope("d"), nil, fields))
		rel, err := c.relation(shaped, "many")
		if err != nil {
			return err
		}
		p.statements = append(p.statements, pirwire.Delete("tr_"+name, table.Name, rel))
		p.tagStmts = append(p.tagStmts, "tr_"+name)
	}
	return nil
}
