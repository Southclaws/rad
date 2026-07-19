package sql

import (
	"fmt"
	"strings"

	"github.com/pgplex/pgparser/nodes"
	"github.com/pgplex/pgparser/parser"

	"github.com/Southclaws/rad/rad/protocol/lirwire"
	"github.com/Southclaws/rad/rad/protocol/pirwire"
)

// lowerInsert compiles INSERT (with optional ON CONFLICT and RETURNING).
//
// ON CONFLICT lowers to program composition instead of a dedicated upsert
// primitive: DO NOTHING creates only the rows whose conflict key has no
// existing match; DO UPDATE splits into update-where-exists and
// create-where-missing, and RETURNING reads the affected keys back from
// post-statement state (later statements observe earlier effects within one
// program).
func (p *program) lowerInsert(ins *nodes.InsertStmt) error {
	if ins.WithClause != nil {
		return unsupportedf("WITH on INSERT")
	}
	table, alias, err := p.targetTable(ins.Relation)
	if err != nil {
		return err
	}
	_ = alias
	cols, err := insertColumns(table, ins.Cols)
	if err != nil {
		return err
	}

	sel, _ := ins.SelectStmt.(*nodes.SelectStmt)
	if sel == nil {
		return unsupportedf("INSERT ... DEFAULT VALUES")
	}

	if ins.OnConflictClause != nil {
		return p.lowerInsertOnConflict(ins, table, cols, sel)
	}

	c := p.newRelCC()
	var root string
	if sel.ValuesLists != nil {
		root, _, err = c.lowerValuesRows(table, cols, sel.ValuesLists)
	} else {
		root, err = c.lowerInsertSelect(table, cols, sel)
	}
	if err != nil {
		return err
	}
	rel, err := c.relation(root, "many")
	if err != nil {
		return err
	}
	p.statements = append(p.statements, pirwire.Create("m", table.Name, rel))
	p.tag = "INSERT 0"
	p.tagStmts = []string{"m"}
	return p.applyReturning(table, ins.ReturningList, "m")
}

// lowerValuesRows builds a literal rows relation for INSERT VALUES. Cells
// must be constants or parameters; each coerces to its target column type.
func (c *cc) lowerValuesRows(table *Table, cols []*Column, valuesLists *nodes.List) (string, *scopeDef, error) {
	rowsCols := make([]lirwire.RowsColumn, len(cols))
	scopeCols := make([]colDef, len(cols))
	for i, col := range cols {
		nullable := col.Nullable
		rowsCols[i] = lirwire.RowsColumn{Name: col.Name, Type: col.Scalar}
		if nullable {
			rowsCols[i].Nullable = &nullable
		}
		scopeCols[i] = colDef{name: col.Name, typ: exprType{scalar: col.Scalar, format: col.Format, nullable: col.Nullable}}
	}
	var rows [][]lirwire.Cell
	for _, rowItem := range valuesLists.Items {
		row, ok := rowItem.(*nodes.List)
		if !ok {
			return "", nil, fmt.Errorf("unexpected VALUES row %T", rowItem)
		}
		if len(row.Items) != len(cols) {
			return "", nil, fmt.Errorf("INSERT has %d target columns but %d values", len(cols), len(row.Items))
		}
		cells := make([]lirwire.Cell, len(cols))
		for i, item := range row.Items {
			col := cols[i]
			want := exprType{scalar: col.Scalar, format: col.Format, nullable: col.Nullable}
			expr, _, err := c.lowerExpr(&env{}, item, &want)
			if err != nil {
				return "", nil, err
			}
			cell, err := cellOf(expr)
			if err != nil {
				return "", nil, fmt.Errorf("column %s: %w", col.Name, err)
			}
			cells[i] = cell
		}
		rows = append(rows, cells)
	}
	label := c.scope("r")
	root := c.add(lirwire.Rows(label, rowsCols, rows))
	return root, &scopeDef{alias: "excluded", label: label, cols: scopeCols}, nil
}

// cellOf converts a lowered constant expression into a rows cell.
func cellOf(e lirwire.Expr) (lirwire.Cell, error) {
	lit, ok := e.ExprUnion.(*lirwire.LiteralExpr)
	if !ok {
		return nil, unsupportedf("non-constant VALUES expression")
	}
	switch v := lit.Value.ValueUnion.(type) {
	case *lirwire.TextValue:
		return v.Value, nil
	case *lirwire.Int64Value:
		return v.Value, nil
	case *lirwire.Float64Value:
		return v.Value, nil
	case *lirwire.BoolValue:
		if v.Value == nil {
			return nil, nil
		}
		s := "false"
		if *v.Value {
			s = "true"
		}
		return &s, nil
	case nil:
		return nil, fmt.Errorf("untyped NULL")
	}
	return nil, fmt.Errorf("unexpected literal value")
}

func (c *cc) lowerInsertSelect(table *Table, cols []*Column, sel *nodes.SelectStmt) (string, error) {
	out, err := c.lowerSelect(&env{}, sel, modeSub)
	if err != nil {
		return "", err
	}
	if len(out.cols) != len(cols) {
		return "", fmt.Errorf("INSERT has %d target columns but query returns %d", len(cols), len(out.cols))
	}
	// The create statement maps relation columns to table columns by name;
	// rename the query's outputs positionally onto the targets.
	fields := make([]lirwire.Field, len(cols))
	for i, col := range cols {
		fields[i] = lirwire.Field{As: col.Name, Expr: lirwire.Col(out.scope, out.cols[i].name)}
	}
	return c.add(lirwire.Project(out.root, c.scope("ins"), nil, fields)), nil
}

func insertColumns(table *Table, colList *nodes.List) ([]*Column, error) {
	if colList == nil || len(colList.Items) == 0 {
		return table.Columns, nil
	}
	out := make([]*Column, 0, len(colList.Items))
	for _, item := range colList.Items {
		rt, ok := item.(*nodes.ResTarget)
		if !ok {
			return nil, fmt.Errorf("unexpected INSERT column %T", item)
		}
		col, ok := table.Column(strings.ToLower(rt.Name))
		if !ok {
			return nil, fmt.Errorf("column %q of relation %q does not exist", rt.Name, table.Name)
		}
		out = append(out, col)
	}
	return out, nil
}

// conflictTargets resolves the ON CONFLICT arbiter column sets: the named
// columns when given, otherwise the primary key plus every unique index.
func conflictTargets(table *Table, infer *nodes.InferClause) ([][]string, error) {
	if infer != nil && infer.WhereClause != nil {
		return nil, unsupportedf("ON CONFLICT partial index inference")
	}
	if infer != nil && infer.IndexElems != nil && len(infer.IndexElems.Items) > 0 {
		var cols []string
		for _, item := range infer.IndexElems.Items {
			el, ok := item.(*nodes.IndexElem)
			if !ok || el.Name == "" {
				return nil, unsupportedf("ON CONFLICT expression target")
			}
			cols = append(cols, strings.ToLower(el.Name))
		}
		return [][]string{cols}, nil
	}
	sets := [][]string{table.PrimaryKey}
	// Unique index metadata is not part of the compiler schema snapshot;
	// the primary key covers the ent single-key upsert patterns. Explicit
	// conflict targets cover the rest.
	return sets, nil
}

// conflictPredicate builds OR-over-sets of EXISTS(σ(scan target, AND eq))
// testing whether a candidate row's conflict key already exists.
func (c *cc) conflictPredicate(table *Table, rowScope *scopeDef, sets [][]string) (lirwire.Expr, error) {
	var alternatives []lirwire.Expr
	for _, set := range sets {
		label := c.scope("t")
		scan := c.add(lirwire.Scan(table.Name, label))
		var conj []*lirwire.Expr
		for _, colName := range set {
			col, ok := table.Column(colName)
			if !ok {
				return lirwire.Expr{}, fmt.Errorf("conflict column %q does not exist", colName)
			}
			rc, ok := rowScope.col(colName)
			if !ok {
				return lirwire.Expr{}, fmt.Errorf("ON CONFLICT column %q is not inserted", colName)
			}
			_ = rc
			eq := lirwire.Binary("eq", lirwire.Col(label, col.Name), lirwire.Col(rowScope.label, colName))
			conj = append(conj, &eq)
		}
		filtered := c.add(lirwire.Filter(scan, lirwire.AndAll(derefAll(conj))))
		alternatives = append(alternatives, lirwire.Exists(filtered))
	}
	out := alternatives[0]
	for _, alt := range alternatives[1:] {
		out = lirwire.Binary("or", out, alt)
	}
	return out, nil
}

func derefAll(in []*lirwire.Expr) []lirwire.Expr {
	out := make([]lirwire.Expr, len(in))
	for i, e := range in {
		out[i] = *e
	}
	return out
}

func (p *program) lowerInsertOnConflict(ins *nodes.InsertStmt, table *Table, cols []*Column, sel *nodes.SelectStmt) error {
	oc := ins.OnConflictClause
	if sel.ValuesLists == nil {
		return unsupportedf("ON CONFLICT with INSERT ... SELECT")
	}
	sets, err := conflictTargets(table, oc.Infer)
	if err != nil {
		return err
	}

	doNothing := oc.Action == parser.ONCONFLICT_NOTHING
	doUpdate := oc.Action == parser.ONCONFLICT_UPDATE
	if !doNothing && !doUpdate {
		return unsupportedf("ON CONFLICT action %d", oc.Action)
	}

	// create-where-missing: shared by both actions.
	cCreate := p.newRelCC()
	rowsRoot, rowScope, err := cCreate.lowerValuesRows(table, cols, sel.ValuesLists)
	if err != nil {
		return err
	}
	pred, err := cCreate.conflictPredicate(table, rowScope, sets)
	if err != nil {
		return err
	}
	missing := cCreate.add(lirwire.Filter(rowsRoot, lirwire.Unary("not", pred)))
	createRel, err := cCreate.relation(missing, "many")
	if err != nil {
		return err
	}

	if doNothing {
		p.statements = append(p.statements, pirwire.Create("m", table.Name, createRel))
		p.tag = "INSERT 0"
		p.tagStmts = []string{"m"}
		return p.applyReturning(table, ins.ReturningList, "m")
	}

	// update-where-exists.
	cUpd := p.newRelCC()
	updRowsRoot, updRowScope, err := cUpd.lowerValuesRows(table, cols, sel.ValuesLists)
	if err != nil {
		return err
	}
	tLabel := cUpd.scope(table.Name)
	tScan := cUpd.add(lirwire.Scan(table.Name, tLabel))
	tScope := tableScope(table, tLabel)
	var onConj []lirwire.Expr
	for _, colName := range sets[0] {
		onConj = append(onConj, lirwire.Binary("eq",
			lirwire.Col(tLabel, colName),
			lirwire.Col(updRowScope.label, colName),
		))
	}
	join := cUpd.add(lirwire.Join(updRowsRoot, tScan, "inner", lirwire.AndAll(onConj)))

	setEnv := &env{scopes: []*scopeDef{updRowScope, tScope}}
	if oc.WhereClause != nil {
		where, _, err := cUpd.lowerExpr(setEnv, oc.WhereClause, &boolType)
		if err != nil {
			return err
		}
		join = cUpd.add(lirwire.Filter(join, where))
	}

	var fields []lirwire.Field
	for _, pk := range table.PrimaryKey {
		fields = append(fields, lirwire.Field{As: pk, Expr: lirwire.Col(tLabel, pk)})
	}
	pkSet := map[string]bool{}
	for _, pk := range table.PrimaryKey {
		pkSet[pk] = true
	}
	assigned := map[string]bool{}
	setTargets, err := resTargets(oc.TargetList)
	if err != nil {
		return err
	}
	for _, rt := range setTargets {
		col, ok := table.Column(strings.ToLower(rt.Name))
		if !ok {
			return fmt.Errorf("column %q of relation %q does not exist", rt.Name, table.Name)
		}
		// SET on a primary-key column (ent's UpdateNewValues assigns every
		// inserted column, the key included) is an identity assignment when
		// the conflict target is the key itself: the projected key already
		// identifies the row, and row identity never changes.
		if pkSet[col.Name] {
			continue
		}
		if assigned[col.Name] {
			return fmt.Errorf("column %q assigned twice", col.Name)
		}
		assigned[col.Name] = true
		want := exprType{scalar: col.Scalar, format: col.Format, nullable: col.Nullable}
		expr, _, err := cUpd.lowerExpr(setEnv, rt.Val, &want)
		if err != nil {
			return err
		}
		fields = append(fields, lirwire.Field{As: col.Name, Expr: expr})
	}
	shaped := cUpd.add(lirwire.Project(join, cUpd.scope("u"), nil, fields))
	updateRel, err := cUpd.relation(shaped, "many")
	if err != nil {
		return err
	}

	p.statements = append(p.statements,
		pirwire.Update("u", table.Name, updateRel),
		pirwire.Create("c", table.Name, createRel),
	)
	p.tag = "INSERT 0"
	p.tagStmts = []string{"u", "c"}

	if ins.ReturningList == nil || len(ins.ReturningList.Items) == 0 {
		p.result = "c"
		return nil
	}

	// RETURNING reads the conflict keys back from post-mutation state.
	cRet := p.newRelCC()
	retRowsRoot, retRowScope, err := cRet.lowerValuesRows(table, cols, sel.ValuesLists)
	if err != nil {
		return err
	}
	rLabel := cRet.scope(table.Name)
	rScan := cRet.add(lirwire.Scan(table.Name, rLabel))
	var retConj []lirwire.Expr
	for _, colName := range sets[0] {
		retConj = append(retConj, lirwire.Binary("eq",
			lirwire.Col(rLabel, colName),
			lirwire.Col(retRowScope.label, colName),
		))
	}
	retJoin := cRet.add(lirwire.Join(retRowsRoot, rScan, "inner", lirwire.AndAll(retConj)))
	var orderTerms []lirwire.OrderTerm
	for _, pk := range table.PrimaryKey {
		orderTerms = append(orderTerms, lirwire.OrderTerm{Expr: lirwire.Col(rLabel, pk)})
	}
	ordered := cRet.add(lirwire.Order(retJoin, orderTerms))
	retCols, retFields, err := returningFields(table, rLabel, ins.ReturningList)
	if err != nil {
		return err
	}
	retRoot := cRet.add(lirwire.Project(ordered, cRet.scope("ret"), nil, retFields))
	retRel, err := cRet.relation(retRoot, "many")
	if err != nil {
		return err
	}
	p.statements = append(p.statements, pirwire.Query("ret", retRel))
	p.result = "ret"
	p.columns = retCols
	p.card = "many"
	return nil
}

func tableScope(table *Table, label string) *scopeDef {
	cols := make([]colDef, 0, len(table.Columns))
	for _, col := range table.Columns {
		cols = append(cols, colDef{
			name: col.Name,
			typ:  exprType{scalar: col.Scalar, format: col.Format, nullable: col.Nullable},
		})
	}
	return &scopeDef{alias: strings.ToLower(table.Name), label: label, cols: cols, table: table}
}

// returningFields resolves a RETURNING list restricted to plain column
// references and stars.
func returningFields(table *Table, label string, list *nodes.List) ([]ResultColumn, []lirwire.Field, error) {
	targets, err := resTargets(list)
	if err != nil {
		return nil, nil, err
	}
	var out []ResultColumn
	var fields []lirwire.Field
	add := func(col *Column) {
		out = append(out, ResultColumn{Name: col.Name, Key: col.Name, Scalar: col.Scalar, Format: col.Format})
		fields = append(fields, lirwire.Field{As: col.Name, Expr: lirwire.Col(label, col.Name)})
	}
	for _, rt := range targets {
		ref, ok := rt.Val.(*nodes.ColumnRef)
		if !ok {
			return nil, nil, unsupportedf("RETURNING expression")
		}
		_, colName, star, err := splitColumnRef(ref)
		if err != nil {
			return nil, nil, err
		}
		if star {
			for _, col := range table.Columns {
				add(col)
			}
			continue
		}
		col, ok := table.Column(colName)
		if !ok {
			return nil, nil, fmt.Errorf("column %q of relation %q does not exist", colName, table.Name)
		}
		add(col)
		if rt.Name != "" {
			out[len(out)-1].Name = rt.Name
		}
	}
	return out, fields, nil
}

// applyReturning wires RETURNING onto a mutation whose statement result
// already carries the full row image (created rows, update post-image,
// delete pre-image).
func (p *program) applyReturning(table *Table, list *nodes.List, stmt string) error {
	if list == nil || len(list.Items) == 0 {
		return nil
	}
	cols, _, err := returningFields(table, "", list)
	if err != nil {
		return err
	}
	p.result = stmt
	p.columns = cols
	p.card = "bag"
	return nil
}

func (p *program) lowerUpdate(upd *nodes.UpdateStmt) error {
	if upd.WithClause != nil {
		return unsupportedf("WITH on UPDATE")
	}
	if upd.FromClause != nil && len(upd.FromClause.Items) > 0 {
		return unsupportedf("UPDATE ... FROM")
	}
	table, alias, err := p.targetTable(upd.Relation)
	if err != nil {
		return err
	}
	c := p.newRelCC()
	label := c.scope(alias)
	cur := c.add(lirwire.Scan(table.Name, label))
	scope := tableScope(table, label)
	scope.alias = alias
	e := &env{scopes: []*scopeDef{scope}}
	if upd.WhereClause != nil {
		pred, _, err := c.lowerExpr(e, upd.WhereClause, &boolType)
		if err != nil {
			return err
		}
		cur = c.add(lirwire.Filter(cur, pred))
	}
	var fields []lirwire.Field
	for _, pk := range table.PrimaryKey {
		fields = append(fields, lirwire.Field{As: pk, Expr: lirwire.Col(label, pk)})
	}
	targets, err := resTargets(upd.TargetList)
	if err != nil {
		return err
	}
	assigned := map[string]bool{}
	for _, rt := range targets {
		if rt.Name == "" {
			return unsupportedf("multi-column SET")
		}
		col, ok := table.Column(strings.ToLower(rt.Name))
		if !ok {
			return fmt.Errorf("column %q of relation %q does not exist", rt.Name, table.Name)
		}
		if assigned[col.Name] {
			return fmt.Errorf("column %q assigned twice", col.Name)
		}
		assigned[col.Name] = true
		want := exprType{scalar: col.Scalar, format: col.Format, nullable: col.Nullable}
		expr, _, err := c.lowerExpr(e, rt.Val, &want)
		if err != nil {
			return err
		}
		fields = append(fields, lirwire.Field{As: col.Name, Expr: expr})
	}
	shaped := c.add(lirwire.Project(cur, c.scope("u"), nil, fields))
	rel, err := c.relation(shaped, "many")
	if err != nil {
		return err
	}
	p.statements = append(p.statements, pirwire.Update("m", table.Name, rel))
	p.tag = "UPDATE"
	p.tagStmts = []string{"m"}
	return p.applyReturning(table, upd.ReturningList, "m")
}

func (p *program) lowerDelete(del *nodes.DeleteStmt) error {
	if del.WithClause != nil {
		return unsupportedf("WITH on DELETE")
	}
	if del.UsingClause != nil && len(del.UsingClause.Items) > 0 {
		return unsupportedf("DELETE ... USING")
	}
	table, alias, err := p.targetTable(del.Relation)
	if err != nil {
		return err
	}
	c := p.newRelCC()
	label := c.scope(alias)
	cur := c.add(lirwire.Scan(table.Name, label))
	scope := tableScope(table, label)
	scope.alias = alias
	e := &env{scopes: []*scopeDef{scope}}
	if del.WhereClause != nil {
		pred, _, err := c.lowerExpr(e, del.WhereClause, &boolType)
		if err != nil {
			return err
		}
		cur = c.add(lirwire.Filter(cur, pred))
	}
	var fields []lirwire.Field
	for _, pk := range table.PrimaryKey {
		fields = append(fields, lirwire.Field{As: pk, Expr: lirwire.Col(label, pk)})
	}
	shaped := c.add(lirwire.Project(cur, c.scope("d"), nil, fields))
	rel, err := c.relation(shaped, "many")
	if err != nil {
		return err
	}
	p.statements = append(p.statements, pirwire.Delete("m", table.Name, rel))
	p.tag = "DELETE"
	p.tagStmts = []string{"m"}
	return p.applyReturning(table, del.ReturningList, "m")
}

func (p *program) targetTable(rv *nodes.RangeVar) (*Table, string, error) {
	if rv == nil {
		return nil, "", fmt.Errorf("missing target relation")
	}
	name := strings.ToLower(rv.Relname)
	table, ok := p.schema.Table(name)
	if !ok {
		return nil, "", fmt.Errorf("relation %q does not exist", name)
	}
	alias := name
	if rv.Alias != nil && rv.Alias.Aliasname != "" {
		alias = strings.ToLower(rv.Alias.Aliasname)
	}
	return table, alias, nil
}
