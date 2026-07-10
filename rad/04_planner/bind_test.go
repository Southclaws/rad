package planner_test

// Binder tests: name/scope resolution, slot assignment, literal coercion,
// uniqueness-aware cardinality, crossing determinism rules, the order
// tie-breaker, and the validation matrix. The forcing query binds as a
// golden snapshot — the single acceptance shape for the whole IR.

import (
	"context"
	"strings"
	"testing"

	"rad/rad/01_kv/kvslate"
	catalog "rad/rad/02_catalog"
	qir "rad/rad/03_qir"
	"rad/rad/03_qir/bound"
	planner "rad/rad/04_planner"
)

// trackerCat is the forcing-query schema: boards → owner, tasks → assignee,
// comments. users.name is unique to exercise unique-index refinement.
func trackerCat(t *testing.T) (*catalog.Catalog, context.Context) {
	t.Helper()
	ctx := context.Background()
	store, err := kvslate.Open("test-"+t.Name(), "memory:///")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })
	cat := catalog.New(store)

	mustCreate := func(def catalog.TableDef) {
		t.Helper()
		if _, err := cat.CreateTable(ctx, def); err != nil {
			t.Fatal(err)
		}
	}
	mustCreate(catalog.TableDef{
		Name: "users",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeText},
			{Name: "name", Type: catalog.TypeText},
		},
		PrimaryKey: []string{"id"},
		Indexes:    []catalog.IndexDef{{Name: "users_name_uq", Columns: []string{"name"}, Unique: true}},
	})
	mustCreate(catalog.TableDef{
		Name: "boards",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeText},
			{Name: "name", Type: catalog.TypeText},
			{Name: "owner_id", Type: catalog.TypeText},
		},
		PrimaryKey: []string{"id"},
		ForeignKeys: []catalog.ForeignKeyDef{
			{Name: "boards_owner_fk", Columns: []string{"owner_id"}, RefTable: "users", RefColumns: []string{"id"}},
		},
	})
	mustCreate(catalog.TableDef{
		Name: "tasks",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeText},
			{Name: "board_id", Type: catalog.TypeText},
			{Name: "title", Type: catalog.TypeText},
			{Name: "status", Type: catalog.TypeText},
			{Name: "priority", Type: catalog.TypeInt64},
			{Name: "estimate", Type: catalog.TypeFloat64, Nullable: true},
			{Name: "assignee_id", Type: catalog.TypeText, Nullable: true},
		},
		PrimaryKey: []string{"id"},
		Indexes: []catalog.IndexDef{
			{Name: "tasks_board_status_idx", Columns: []string{"board_id", "status"}},
		},
		ForeignKeys: []catalog.ForeignKeyDef{
			{Name: "tasks_board_fk", Columns: []string{"board_id"}, RefTable: "boards", RefColumns: []string{"id"}},
			{Name: "tasks_assignee_fk", Columns: []string{"assignee_id"}, RefTable: "users", RefColumns: []string{"id"}},
		},
	})
	mustCreate(catalog.TableDef{
		Name: "comments",
		Columns: []catalog.ColumnDef{
			{Name: "id", Type: catalog.TypeText},
			{Name: "task_id", Type: catalog.TypeText},
			{Name: "body", Type: catalog.TypeText},
		},
		PrimaryKey: []string{"id"},
		Indexes:    []catalog.IndexDef{{Name: "comments_task_idx", Columns: []string{"task_id"}}},
		ForeignKeys: []catalog.ForeignKeyDef{
			{Name: "comments_task_fk", Columns: []string{"task_id"}, RefTable: "tasks", RefColumns: []string{"id"}},
		},
	})
	return cat, ctx
}

// ── unbound construction helpers ────────────────────────────────────────────

func bcol(scope, name string) qir.Column { return qir.Column{Scope: scope, Name: name} }
func blit(v any) qir.Literal             { return qir.Literal{Raw: v} }
func beq(l, r qir.Expr) qir.Expr         { return qir.Binary{Op: qir.OpEq, L: l, R: r} }
func band(l, r qir.Expr) qir.Expr        { return qir.Binary{Op: qir.OpAnd, L: l, R: r} }
func bscan(table, scope string) qir.Scan { return qir.Scan{Table: table, Scope: scope} }
func bfilter(in qir.Relation, pred qir.Expr) qir.Filter {
	return qir.Filter{Input: in, Pred: pred}
}

func bind(t *testing.T, q qir.Query) *bound.Query {
	t.Helper()
	cat, ctx := trackerCat(t)
	bq, err := planner.Bind(ctx, cat, q)
	if err != nil {
		t.Fatal(err)
	}
	return bq
}

func bindErr(t *testing.T, q qir.Query, want string) {
	t.Helper()
	cat, ctx := trackerCat(t)
	_, err := planner.Bind(ctx, cat, q)
	if err == nil {
		t.Fatalf("bind succeeded, want error containing %q", want)
	}
	if !strings.Contains(err.Error(), want) {
		t.Fatalf("error = %q, want it to contain %q", err, want)
	}
	if !strings.HasPrefix(err.Error(), "planner:") {
		t.Fatalf("binder error lost its planner: prefix: %q", err)
	}
}

// forcingQuery is the arc's acceptance shape: boards → owner → first 20 open
// tasks by priority → assignee + comment count. Zero include/parent/children
// special cases.
func forcingQuery() qir.Query {
	owner := bfilter(bscan("users", "o"), beq(bcol("o", "id"), bcol("b", "owner_id")))
	assignee := bfilter(bscan("users", "a"), beq(bcol("a", "id"), bcol("t", "assignee_id")))
	commentCount := qir.Aggregate{
		Input: bfilter(bscan("comments", "c"), beq(bcol("c", "task_id"), bcol("t", "id"))),
		Terms: []qir.AggTerm{{Fn: qir.AggCount, As: "n"}},
	}
	openTasks := qir.Project{
		Input: qir.Slice{
			Input: qir.Order{
				Input: bfilter(bscan("tasks", "t"),
					band(beq(bcol("t", "board_id"), bcol("b", "id")),
						beq(bcol("t", "status"), blit("open")))),
				Terms: []qir.OrderTerm{{Expr: bcol("t", "priority"), Desc: true}},
			},
			Limit: new(int(20)),
		},
		Spread: []string{"t"},
		Fields: []qir.ProjField{
			{As: "assignee", Expr: qir.First{Rel: assignee}},
			{As: "comment_count", Expr: qir.Scalar{Rel: commentCount}},
		},
	}
	return qir.Query{
		Card: qir.CardMany,
		Root: qir.Project{
			Input:  bscan("boards", "b"),
			Spread: []string{"b"},
			Fields: []qir.ProjField{
				{As: "owner", Expr: qir.First{Rel: owner}},
				{As: "tasks", Expr: qir.Array{Rel: openTasks}},
			},
		},
	}
}

func TestBindForcingQueryGolden(t *testing.T) {
	got := bound.Print(bind(t, forcingQuery()))
	if got != forcingGolden {
		t.Fatalf("bound forcing query drifted.\n--- got ---\n%s\n--- want ---\n%s", got, forcingGolden)
	}
}

// The golden pins: slot allocation order, spread slot reuse, correlation as
// free slots (owner/tasks/assignee/comment_count all reference outer slots),
// uniqueness-refined cardinalities (both First relations are 0..1), the
// appended id tie-breaker on the task ordering, and every inferred type.
const forcingGolden = `Query card=many
  Project  [card 0..many]
    id#0 = b.id#0 : text
    name#1 = b.name#1 : text
    owner_id#2 = b.owner_id#2 : text
    owner#5 = first(
      Filter eq(o.id#3, b.owner_id#2)  [card 0..1 free 2]
        Scan users (o) {id#3:text name#4:text}
    ) : row?
    tasks#21 = array(
      Project  [card 0..20 free 0]
        id#6 = t.id#6 : text
        board_id#7 = t.board_id#7 : text
        title#8 = t.title#8 : text
        status#9 = t.status#9 : text
        priority#10 = t.priority#10 : int64
        estimate#11 = t.estimate#11 : float64?
        assignee_id#12 = t.assignee_id#12 : text?
        assignee#15 = first(
      Filter eq(a.id#13, t.assignee_id#12)  [card 0..1 free 12]
        Scan users (a) {id#13:text name#14:text}
    ) : row?
        comment_count#20 = scalar(
      Aggregate  [card 1..1 free 6]
        n#19 = count(*) : int64
        Filter eq(c.task_id#17, t.id#6)  [card 0..many free 6]
          Scan comments (c) {id#16:text task_id#17:text body#18:text}
    ) : int64?
        Slice offset=0 limit=20  [card 0..20 free 0]
          Order t.priority#10 desc, id#6 asc  [card 0..many free 0]
            Filter and(eq(t.board_id#7, b.id#0), eq(t.status#9, "open"))  [card 0..many free 0]
              Scan tasks (t) {id#6:text board_id#7:text title#8:text status#9:text priority#10:int64 estimate#11:float64? assignee_id#12:text?}
    ) : array<row>
    Scan boards (b) {id#0:text name#1:text owner_id#2:text}
`

// ── determinism rules ───────────────────────────────────────────────────────

func TestFirstRequiresDeterminism(t *testing.T) {
	// Unordered multi-row: rejected.
	bindErr(t, qir.Query{Card: qir.CardMany, Root: qir.Project{
		Input:  bscan("boards", "b"),
		Spread: []string{"b"},
		Fields: []qir.ProjField{{As: "any_task", Expr: qir.First{Rel: bscan("tasks", "t")}}},
	}}, "first over an unordered multi-row relation")

	// A unique-key filter is statically at-most-one: accepted. So is an
	// explicitly ordered relation.
	bind(t, qir.Query{Card: qir.CardMany, Root: qir.Project{
		Input:  bscan("boards", "b"),
		Spread: []string{"b"},
		Fields: []qir.ProjField{
			{As: "owner", Expr: qir.First{Rel: bfilter(bscan("users", "o"), beq(bcol("o", "id"), bcol("b", "owner_id")))}},
			{As: "by_name", Expr: qir.First{Rel: bfilter(bscan("users", "u"), beq(bcol("u", "name"), blit("ada")))}},
			{As: "top_task", Expr: qir.First{Rel: qir.Order{
				Input: bscan("tasks", "t"),
				Terms: []qir.OrderTerm{{Expr: bcol("t", "priority"), Desc: true}},
			}}},
		},
	}})

	// A non-unique equality does not pin: still rejected.
	bindErr(t, qir.Query{Card: qir.CardMany, Root: qir.Project{
		Input:  bscan("boards", "b"),
		Spread: []string{"b"},
		Fields: []qir.ProjField{{As: "open", Expr: qir.First{
			Rel: bfilter(bscan("tasks", "t"), beq(bcol("t", "status"), blit("open"))),
		}}},
	}}, "first over an unordered multi-row relation")
}

func TestScalarAssertsCardinalityAndArity(t *testing.T) {
	count := qir.Aggregate{Input: bscan("tasks", "t"), Terms: []qir.AggTerm{{Fn: qir.AggCount, As: "n"}}}

	// Global fold: exactly one row, one column — accepted.
	bind(t, qir.Query{Card: qir.CardMany, Root: qir.Project{
		Input:  bscan("boards", "b"),
		Spread: []string{"b"},
		Fields: []qir.ProjField{{As: "n", Expr: qir.Scalar{Rel: count}}},
	}})

	// Multi-row, even ordered: rejected — Scalar is an assertion, and
	// "first scalar" must be spelled Scalar(Slice₁(Order(...))).
	bindErr(t, qir.Query{Card: qir.CardMany, Root: qir.Project{
		Input:  bscan("boards", "b"),
		Spread: []string{"b"},
		Fields: []qir.ProjField{{As: "x", Expr: qir.Scalar{Rel: qir.Order{
			Input: bscan("tasks", "t"),
			Terms: []qir.OrderTerm{{Expr: bcol("t", "priority")}},
		}}}},
	}}, "scalar asserts at most one row")

	// At-most-one but five columns: the arity check fires.
	bindErr(t, qir.Query{Card: qir.CardMany, Root: qir.Project{
		Input:  bscan("boards", "b"),
		Spread: []string{"b"},
		Fields: []qir.ProjField{{As: "x", Expr: qir.Scalar{
			Rel: qir.Slice{Input: bscan("tasks", "t"), Limit: new(int(1))},
		}}},
	}}, "single-column relation")
}

func TestRootCardinalityRules(t *testing.T) {
	bindErr(t, qir.Query{Card: "some", Root: bscan("tasks", "t")}, "unknown root cardinality")
	bindErr(t, qir.Query{Card: qir.CardScalar, Root: bscan("tasks", "t")}, "single-column root")
	bindErr(t, qir.Query{Card: qir.CardFirst, Root: bscan("tasks", "t")}, "depend on the access path")
	// Slice 1 makes first legal (declared-arbitrary selection).
	bind(t, qir.Query{Card: qir.CardFirst, Root: qir.Slice{Input: bscan("tasks", "t"), Limit: new(int(1))}})
}

// ── the order tie-breaker ───────────────────────────────────────────────────

func TestOrderTieBreaker(t *testing.T) {
	rootOrder := func(q *bound.Query) *bound.Order {
		t.Helper()
		o, ok := q.Root.(*bound.Order)
		if !ok {
			t.Fatalf("root is %T, want *bound.Order", q.Root)
		}
		return o
	}

	// Ordering by a non-key column appends the primary key ascending.
	q := bind(t, qir.Query{Card: qir.CardMany, Root: qir.Order{
		Input: bscan("tasks", "t"),
		Terms: []qir.OrderTerm{{Expr: bcol("t", "priority"), Desc: true}},
	}})
	terms := rootOrder(q).Terms
	if len(terms) != 2 || terms[1].Desc {
		t.Fatalf("terms = %+v, want priority desc + id asc tie-breaker", terms)
	}
	if ref, ok := terms[1].Expr.(bound.SlotRef); !ok || ref.Name != "id" {
		t.Fatalf("tie-breaker = %+v, want the id slot", terms[1].Expr)
	}

	// Ordering by the key itself appends nothing.
	q = bind(t, qir.Query{Card: qir.CardMany, Root: qir.Order{
		Input: bscan("tasks", "t"),
		Terms: []qir.OrderTerm{{Expr: bcol("t", "id")}},
	}})
	if len(rootOrder(q).Terms) != 1 {
		t.Fatalf("ordering by the pk grew terms: %+v", rootOrder(q).Terms)
	}

	// Above a grouped aggregate, the group attributes are the unique key.
	q = bind(t, qir.Query{Card: qir.CardMany, Root: qir.Order{
		Input: qir.Aggregate{
			Input:  bscan("tasks", "t"),
			Scope:  "stats",
			Groups: []qir.GroupTerm{{Expr: bcol("t", "status")}},
			Terms:  []qir.AggTerm{{Fn: qir.AggCount, As: "n"}},
		},
		Terms: []qir.OrderTerm{{Expr: bcol("stats", "n"), Desc: true}},
	}})
	terms = rootOrder(q).Terms
	if len(terms) != 2 {
		t.Fatalf("terms = %+v, want n desc + status asc", terms)
	}
	if ref, ok := terms[1].Expr.(bound.SlotRef); !ok || ref.Name != "status" {
		t.Fatalf("tie-breaker = %+v, want the status group slot", terms[1].Expr)
	}
}

// ── relational closure ──────────────────────────────────────────────────────

// Aggregate outputs are ordinary attributes: filter and order above the fold
// address them through the aggregate's scope, and nothing else is visible
// there — the input's scopes closed at the aggregate.
func TestClosureAboveAggregate(t *testing.T) {
	grouped := qir.Aggregate{
		Input:  bscan("tasks", "t"),
		Scope:  "stats",
		Groups: []qir.GroupTerm{{Expr: bcol("t", "status")}},
		Terms:  []qir.AggTerm{{Fn: qir.AggCount, As: "n"}},
	}
	bind(t, qir.Query{Card: qir.CardMany, Root: qir.Filter{
		Input: grouped,
		Pred:  qir.Binary{Op: qir.OpGt, L: bcol("stats", "n"), R: blit(10)},
	}})

	// The scan's scope is gone above the aggregate.
	bindErr(t, qir.Query{Card: qir.CardMany, Root: qir.Filter{
		Input: grouped,
		Pred:  beq(bcol("t", "status"), blit("open")),
	}}, `unknown scope "t"`)

	// Unlabelled aggregates cannot be referenced — the error says why.
	bindErr(t, qir.Query{Card: qir.CardMany, Root: qir.Filter{
		Input: qir.Aggregate{
			Input:  bscan("tasks", "t"),
			Groups: []qir.GroupTerm{{Expr: bcol("t", "status")}},
			Terms:  []qir.AggTerm{{Fn: qir.AggCount, As: "n"}},
		},
		Pred: qir.Binary{Op: qir.OpGt, L: bcol("stats", "n"), R: blit(10)},
	}}, `unknown scope "stats"`)
}

func TestClosureAboveProject(t *testing.T) {
	shaped := qir.Project{
		Input: bscan("tasks", "t"),
		Scope: "shaped",
		Fields: []qir.ProjField{
			{As: "id", Expr: bcol("t", "id")},
			{As: "score", Expr: qir.Binary{Op: qir.OpMul, L: bcol("t", "priority"), R: blit(2)}},
		},
	}
	bind(t, qir.Query{Card: qir.CardMany, Root: qir.Filter{
		Input: shaped,
		Pred:  qir.Binary{Op: qir.OpGt, L: bcol("shaped", "score"), R: blit(10)},
	}})
	bindErr(t, qir.Query{Card: qir.CardMany, Root: qir.Filter{
		Input: shaped,
		Pred:  beq(bcol("t", "title"), blit("x")),
	}}, `unknown scope "t"`)
}

// ── literal coercion ────────────────────────────────────────────────────────

func TestLiteralCoercion(t *testing.T) {
	// A NULL literal adopts the column's type.
	bind(t, qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"),
		beq(bcol("t", "assignee_id"), blit(nil)))})

	// Numbers coerce by column type, never by guess.
	bind(t, qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"),
		qir.Binary{Op: qir.OpGte, L: bcol("t", "priority"), R: blit(3)})})
	bind(t, qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"),
		qir.Binary{Op: qir.OpLt, L: bcol("t", "estimate"), R: blit(2)})}) // int literal, float column

	bindErr(t, qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"),
		beq(bcol("t", "status"), blit(5)))}, "expected a text value")
	bindErr(t, qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"),
		beq(bcol("t", "priority"), blit(1.5)))}, "expected a int64 value")
	bindErr(t, qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"),
		beq(blit(nil), blit(nil)))}, "bare NULL")
}

// ── the validation matrix ───────────────────────────────────────────────────

func TestValidationMatrix(t *testing.T) {
	cases := []struct {
		name string
		q    qir.Query
		want string
	}{
		{"unknown table",
			qir.Query{Card: qir.CardMany, Root: bscan("ghosts", "g")},
			`unknown table "ghosts"`},
		{"scan needs scope",
			qir.Query{Card: qir.CardMany, Root: bscan("tasks", "")},
			"needs a scope label"},
		{"duplicate scope",
			qir.Query{Card: qir.CardMany, Root: qir.Join{
				Left: bscan("tasks", "t"), Right: bscan("comments", "t"),
				Kind: qir.InnerJoin, On: blit(true)}},
			`duplicate scope "t"`},
		{"unknown column",
			qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"), beq(bcol("t", "ghost"), blit("x")))},
			`scope "t" has no column "ghost"`},
		{"unqualified column",
			qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"), beq(bcol("", "status"), blit("x")))},
			"needs a scope qualifier"},
		{"filter needs bool",
			qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"), bcol("t", "title"))},
			"filter predicate must be boolean"},
		{"and needs bools",
			qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"),
				band(bcol("t", "title"), blit(true)))},
			"and needs boolean operands"},
		{"not needs bool",
			qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"),
				qir.Unary{Op: qir.OpNot, X: bcol("t", "title")})},
			"not needs a boolean"},
		{"negate needs numeric",
			qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"),
				beq(qir.Unary{Op: qir.OpNegate, X: bcol("t", "title")}, blit("x")))},
			"cannot negate"},
		{"comparing mixed kinds",
			qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"),
				beq(bcol("t", "priority"), bcol("t", "title")))},
			"cannot compare int64 with text"},
		{"arithmetic needs numbers",
			qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"),
				beq(qir.Binary{Op: qir.OpAdd, L: bcol("t", "title"), R: blit("x")}, blit("y")))},
			"add needs numeric operands"},
		{"unknown function",
			qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"),
				beq(qir.Call{Fn: "lower", Args: []qir.Expr{bcol("t", "title")}}, blit("x")))},
			`unknown function "lower"`},
		{"bad cast",
			qir.Query{Card: qir.CardMany, Root: bfilter(bscan("tasks", "t"),
				beq(qir.Cast{X: bcol("t", "title"), To: qir.KindBool}, blit(true)))},
			"cannot cast text to bool"},
		{"sum needs numeric",
			qir.Query{Card: qir.CardMany, Root: qir.Aggregate{Input: bscan("tasks", "t"),
				Terms: []qir.AggTerm{{Fn: qir.AggSum, Arg: bcol("t", "title"), As: "s"}}}},
			"sum requires a numeric argument"},
		{"aggregate needs as",
			qir.Query{Card: qir.CardMany, Root: qir.Aggregate{Input: bscan("tasks", "t"),
				Terms: []qir.AggTerm{{Fn: qir.AggCount}}}},
			"needs an output name"},
		{"duplicate aggregate name",
			qir.Query{Card: qir.CardMany, Root: qir.Aggregate{Input: bscan("tasks", "t"),
				Terms: []qir.AggTerm{{Fn: qir.AggCount, As: "n"}, {Fn: qir.AggCount, As: "n"}}}},
			`duplicate aggregate output name "n"`},
		{"unknown aggregate fn",
			qir.Query{Card: qir.CardMany, Root: qir.Aggregate{Input: bscan("tasks", "t"),
				Terms: []qir.AggTerm{{Fn: "median", Arg: bcol("t", "priority"), As: "m"}}}},
			`unknown aggregate function "median"`},
		{"empty aggregate",
			qir.Query{Card: qir.CardMany, Root: qir.Aggregate{Input: bscan("tasks", "t")}},
			"at least one group or term"},
		{"group needs name",
			qir.Query{Card: qir.CardMany, Root: qir.Aggregate{Input: bscan("tasks", "t"),
				Groups: []qir.GroupTerm{{Expr: qir.Binary{Op: qir.OpAdd, L: bcol("t", "priority"), R: blit(1)}}},
				Terms:  []qir.AggTerm{{Fn: qir.AggCount, As: "n"}}}},
			"group expression needs an output name"},
		{"projection needs fields",
			qir.Query{Card: qir.CardMany, Root: qir.Project{Input: bscan("tasks", "t")}},
			"projection has no fields"},
		{"duplicate projection field",
			qir.Query{Card: qir.CardMany, Root: qir.Project{Input: bscan("tasks", "t"),
				Fields: []qir.ProjField{
					{As: "x", Expr: bcol("t", "id")},
					{As: "x", Expr: bcol("t", "title")}}}},
			`duplicate projection field "x"`},
		{"spread collides with field",
			qir.Query{Card: qir.CardMany, Root: qir.Project{Input: bscan("tasks", "t"),
				Spread: []string{"t"},
				Fields: []qir.ProjField{{As: "title", Expr: bcol("t", "id")}}}},
			`duplicate projection field "title"`},
		{"spread of unknown scope",
			qir.Query{Card: qir.CardMany, Root: qir.Project{Input: bscan("tasks", "t"),
				Spread: []string{"z"},
				Fields: []qir.ProjField{{As: "id", Expr: bcol("t", "id")}}}},
			`spread scope "z"`},
		{"order needs terms",
			qir.Query{Card: qir.CardMany, Root: qir.Order{Input: bscan("tasks", "t")}},
			"order needs at least one term"},
		{"order by array",
			qir.Query{Card: qir.CardMany, Root: qir.Order{
				Input: bscan("boards", "b"),
				Terms: []qir.OrderTerm{{Expr: qir.Array{Rel: bfilter(bscan("tasks", "t"),
					beq(bcol("t", "board_id"), bcol("b", "id")))}}}}},
			"cannot order by a array value"},
		{"negative offset",
			qir.Query{Card: qir.CardMany, Root: qir.Slice{Input: bscan("tasks", "t"), Offset: -1}},
			"offset must be >= 0"},
		{"negative limit",
			qir.Query{Card: qir.CardMany, Root: qir.Slice{Input: bscan("tasks", "t"), Limit: new(int(-2))}},
			"limit must be >= 0"},
		{"join kind",
			qir.Query{Card: qir.CardMany, Root: qir.Join{
				Left: bscan("tasks", "t"), Right: bscan("users", "u"),
				Kind: "cross", On: blit(true)}},
			`unsupported join kind "cross"`},
		{"join on bool",
			qir.Query{Card: qir.CardMany, Root: qir.Join{
				Left: bscan("tasks", "t"), Right: bscan("users", "u"),
				Kind: qir.InnerJoin, On: bcol("t", "title")}},
			"join condition must be boolean"},
		{"spread across a crossing boundary",
			qir.Query{Card: qir.CardMany, Root: qir.Project{
				Input:  bscan("boards", "b"),
				Spread: []string{"b"},
				Fields: []qir.ProjField{{As: "tasks", Expr: qir.Array{Rel: qir.Project{
					Input:  bfilter(bscan("tasks", "t"), beq(bcol("t", "board_id"), bcol("b", "id"))),
					Spread: []string{"b"}, // outer scope: visible for refs, not spreadable
					Fields: []qir.ProjField{{As: "id", Expr: bcol("t", "id")}},
				}}}},
			}},
			`spread scope "b" is not produced beneath`},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) { bindErr(t, tc.q, tc.want) })
	}
}

// A join keeps both sides' scopes visible above it, and slice-limit nil vs 0
// are distinct: zero rows is expressible.
func TestJoinScopesAndZeroLimit(t *testing.T) {
	q := bind(t, qir.Query{Card: qir.CardMany, Root: qir.Filter{
		Input: qir.Join{
			Left:  bscan("tasks", "t"),
			Right: bscan("users", "u"),
			Kind:  qir.LeftJoin,
			On:    beq(bcol("t", "assignee_id"), bcol("u", "id")),
		},
		Pred: beq(bcol("u", "name"), blit("ada")),
	}})
	if _, ok := q.Root.(*bound.Filter); !ok {
		t.Fatalf("root = %T", q.Root)
	}

	zq := bind(t, qir.Query{Card: qir.CardMany, Root: qir.Slice{Input: bscan("tasks", "t"), Limit: new(int(0))}})
	if got := zq.Root.Card(); !got.AtMostOne() || got.Max != 0 {
		t.Fatalf("limit 0 card = %v, want 0..0", got)
	}
}
