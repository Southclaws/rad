package planner_test

// Binder tests: name/scope resolution, slot assignment, literal coercion,
// uniqueness-aware cardinality, crossing determinism rules, the order
// tie-breaker, and the validation matrix. The forcing query binds as a
// golden snapshot — the single acceptance shape for the whole IR.

import (
	"context"
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	lir "github.com/Southclaws/rad/rad/engine/03_lir"
	"github.com/Southclaws/rad/rad/engine/03_lir/bound"
	planner "github.com/Southclaws/rad/rad/engine/04_planner"
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

// -
// unbound construction helpers
// -

func bcol(scope, name string) lir.Column { return lir.Column{Scope: scope, Name: name} }
func blit(v any) lir.Literal             { return lir.Literal{Raw: v} }
func beq(l, r lir.Expr) lir.Expr         { return lir.Binary{Op: lir.OpEq, L: l, R: r} }
func band(l, r lir.Expr) lir.Expr        { return lir.Binary{Op: lir.OpAnd, L: l, R: r} }
func bscan(table, scope string) lir.Scan { return lir.Scan{Table: table, Scope: scope} }
func bfilter(in lir.Relation, pred lir.Expr) lir.Filter {
	return lir.Filter{Input: in, Pred: pred}
}

// testQuery adds an explicit (but otherwise uninteresting) order around test
// roots that exercise a lower binding/planning concern. Individual tests that
// assert materialisation semantics use a meaningful order themselves.
func testQuery(q lir.Query) (lir.Query, bool) {
	if q.Card != lir.CardMany {
		return q, false
	}
	if _, ordered := q.Root.(lir.Order); ordered {
		return q, false
	}
	q.Root = lir.Order{Input: q.Root, Terms: []lir.OrderTerm{{Expr: blit(true)}}}
	return q, true
}

func bind(t *testing.T, q lir.Query) *bound.Query {
	t.Helper()
	cat, ctx := trackerCat(t)
	q, wrapped := testQuery(q)
	bq, err := planner.Bind(ctx, cat, q)
	if err != nil {
		t.Fatal(err)
	}
	if wrapped {
		bq.Root = bq.Root.(*bound.Order).In
	}
	return bq
}

func bindErr(t *testing.T, q lir.Query, want string) {
	t.Helper()
	cat, ctx := trackerCat(t)
	q, _ = testQuery(q)
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
func forcingQuery() lir.Query {
	owner := bfilter(bscan("users", "o"), beq(bcol("o", "id"), bcol("b", "owner_id")))
	assignee := bfilter(bscan("users", "a"), beq(bcol("a", "id"), bcol("t", "assignee_id")))
	commentCount := lir.Aggregate{
		Input: bfilter(bscan("comments", "c"), beq(bcol("c", "task_id"), bcol("t", "id"))),
		Terms: []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
	}
	openTasks := lir.Project{
		Input: lir.Slice{
			Input: lir.Order{
				Input: bfilter(bscan("tasks", "t"),
					band(beq(bcol("t", "board_id"), bcol("b", "id")),
						beq(bcol("t", "status"), blit("open")))),
				Terms: []lir.OrderTerm{{Expr: bcol("t", "priority"), Desc: true}},
			},
			Limit: new(int(20)),
		},
		Spread: []string{"t"},
		Fields: []lir.ProjField{
			{As: "assignee", Expr: lir.First{Rel: assignee}},
			{As: "comment_count", Expr: lir.Scalar{Rel: commentCount}},
		},
	}
	return lir.Query{
		Card: lir.CardMany,
		Root: lir.Project{
			Input:  bscan("boards", "b"),
			Spread: []string{"b"},
			Fields: []lir.ProjField{
				{As: "owner", Expr: lir.First{Rel: owner}},
				{As: "tasks", Expr: lir.Array{Rel: openTasks}},
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

// -
// determinism rules
// -

func TestFirstRequiresDeterminism(t *testing.T) {
	// Unordered multi-row: rejected.
	bindErr(t, lir.Query{Card: lir.CardMany, Root: lir.Project{
		Input:  bscan("boards", "b"),
		Spread: []string{"b"},
		Fields: []lir.ProjField{{As: "any_task", Expr: lir.First{Rel: bscan("tasks", "t")}}},
	}}, "first over an unordered multi-row relation")

	// A unique-key filter is statically at-most-one: accepted. So is an
	// explicitly ordered relation.
	bind(t, lir.Query{Card: lir.CardMany, Root: lir.Project{
		Input:  bscan("boards", "b"),
		Spread: []string{"b"},
		Fields: []lir.ProjField{
			{As: "owner", Expr: lir.First{Rel: bfilter(bscan("users", "o"), beq(bcol("o", "id"), bcol("b", "owner_id")))}},
			{As: "by_name", Expr: lir.First{Rel: bfilter(bscan("users", "u"), beq(bcol("u", "name"), blit("ada")))}},
			{As: "top_task", Expr: lir.First{Rel: lir.Order{
				Input: bscan("tasks", "t"),
				Terms: []lir.OrderTerm{{Expr: bcol("t", "priority"), Desc: true}},
			}}},
		},
	}})

	// A non-unique equality does not pin: still rejected.
	bindErr(t, lir.Query{Card: lir.CardMany, Root: lir.Project{
		Input:  bscan("boards", "b"),
		Spread: []string{"b"},
		Fields: []lir.ProjField{{As: "open", Expr: lir.First{
			Rel: bfilter(bscan("tasks", "t"), beq(bcol("t", "status"), blit("open"))),
		}}},
	}}, "first over an unordered multi-row relation")
}

func TestScalarAssertsCardinalityAndArity(t *testing.T) {
	count := lir.Aggregate{Input: bscan("tasks", "t"), Terms: []lir.AggTerm{{Fn: lir.AggCount, As: "n"}}}

	// Global fold: exactly one row, one column — accepted.
	bind(t, lir.Query{Card: lir.CardMany, Root: lir.Project{
		Input:  bscan("boards", "b"),
		Spread: []string{"b"},
		Fields: []lir.ProjField{{As: "n", Expr: lir.Scalar{Rel: count}}},
	}})

	// Multi-row, even ordered: rejected — Scalar is an assertion, and
	// "first scalar" must be spelled Scalar(Slice₁(Order(...))).
	bindErr(t, lir.Query{Card: lir.CardMany, Root: lir.Project{
		Input:  bscan("boards", "b"),
		Spread: []string{"b"},
		Fields: []lir.ProjField{{As: "x", Expr: lir.Scalar{Rel: lir.Order{
			Input: bscan("tasks", "t"),
			Terms: []lir.OrderTerm{{Expr: bcol("t", "priority")}},
		}}}},
	}}, "scalar asserts at most one row")

	// At-most-one but five columns: the arity check fires.
	bindErr(t, lir.Query{Card: lir.CardMany, Root: lir.Project{
		Input:  bscan("boards", "b"),
		Spread: []string{"b"},
		Fields: []lir.ProjField{{As: "x", Expr: lir.Scalar{
			Rel: lir.Slice{Input: bscan("tasks", "t"), Limit: new(int(1))},
		}}},
	}}, "single-column relation")
}

func TestRootCardinalityRules(t *testing.T) {
	bindErr(t, lir.Query{Card: "some", Root: bscan("tasks", "t")}, "unknown root cardinality")
	bindErr(t, lir.Query{Card: lir.CardScalar, Root: bscan("tasks", "t")}, "single-column root")
	bindErr(t, lir.Query{Card: lir.CardFirst, Root: bscan("tasks", "t")}, "depend on the access path")
	// Slice 1 proves that first can observe at most one row.
	bind(t, lir.Query{Card: lir.CardFirst, Root: lir.Slice{Input: bscan("tasks", "t"), Limit: new(int(1))}})

	// Ordering makes first deterministic, but scalar is an at-most-one
	// assertion rather than another spelling of first.
	orderedOneColumn := lir.Order{
		Input: lir.Project{
			Input:  bscan("tasks", "ordered_tasks"),
			Scope:  "ordered_project",
			Fields: []lir.ProjField{{As: "id", Expr: bcol("ordered_tasks", "id")}},
		},
		Terms: []lir.OrderTerm{{Expr: bcol("ordered_project", "id")}},
	}
	bindErr(t, lir.Query{Card: lir.CardScalar, Root: orderedOneColumn}, "root scalar asserts at most one row")

	// A one-column unique lookup and a one-row slice satisfy the assertion.
	bind(t, lir.Query{Card: lir.CardScalar, Root: lir.Project{
		Input:  bfilter(bscan("tasks", "unique_task"), beq(bcol("unique_task", "id"), blit("t1"))),
		Fields: []lir.ProjField{{As: "id", Expr: bcol("unique_task", "id")}},
	}})
	bind(t, lir.Query{Card: lir.CardScalar, Root: lir.Project{
		Input:  lir.Slice{Input: bscan("tasks", "sliced_tasks"), Limit: new(int(1))},
		Fields: []lir.ProjField{{As: "id", Expr: bcol("sliced_tasks", "id")}},
	}})
}

// -
// the order tie-breaker
// -

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
	q := bind(t, lir.Query{Card: lir.CardMany, Root: lir.Order{
		Input: bscan("tasks", "t"),
		Terms: []lir.OrderTerm{{Expr: bcol("t", "priority"), Desc: true}},
	}})
	terms := rootOrder(q).Terms
	if len(terms) != 2 || terms[1].Desc {
		t.Fatalf("terms = %+v, want priority desc + id asc tie-breaker", terms)
	}
	if ref, ok := terms[1].Expr.(bound.SlotRef); !ok || ref.Name != "id" {
		t.Fatalf("tie-breaker = %+v, want the id slot", terms[1].Expr)
	}

	// Ordering by the key itself appends nothing.
	q = bind(t, lir.Query{Card: lir.CardMany, Root: lir.Order{
		Input: bscan("tasks", "t"),
		Terms: []lir.OrderTerm{{Expr: bcol("t", "id")}},
	}})
	if len(rootOrder(q).Terms) != 1 {
		t.Fatalf("ordering by the pk grew terms: %+v", rootOrder(q).Terms)
	}

	// Above a grouped aggregate, the group attributes are the unique key.
	q = bind(t, lir.Query{Card: lir.CardMany, Root: lir.Order{
		Input: lir.Aggregate{
			Input:  bscan("tasks", "t"),
			Scope:  "stats",
			Groups: []lir.GroupTerm{{Expr: bcol("t", "status")}},
			Terms:  []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
		},
		Terms: []lir.OrderTerm{{Expr: bcol("stats", "n"), Desc: true}},
	}})
	terms = rootOrder(q).Terms
	if len(terms) != 2 {
		t.Fatalf("terms = %+v, want n desc + status asc", terms)
	}
	if ref, ok := terms[1].Expr.(bound.SlotRef); !ok || ref.Name != "status" {
		t.Fatalf("tie-breaker = %+v, want the status group slot", terms[1].Expr)
	}
}

// -
// relational closure
// -

// Aggregate outputs are ordinary attributes: filter and order above the fold
// address them through the aggregate's scope, and nothing else is visible
// there — the input's scopes closed at the aggregate.
func TestClosureAboveAggregate(t *testing.T) {
	grouped := lir.Aggregate{
		Input:  bscan("tasks", "t"),
		Scope:  "stats",
		Groups: []lir.GroupTerm{{Expr: bcol("t", "status")}},
		Terms:  []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
	}
	bind(t, lir.Query{Card: lir.CardMany, Root: lir.Filter{
		Input: grouped,
		Pred:  lir.Binary{Op: lir.OpGt, L: bcol("stats", "n"), R: blit(10)},
	}})

	// The scan's scope is gone above the aggregate — and the error says so,
	// instead of claiming the scope never existed.
	bindErr(t, lir.Query{Card: lir.CardMany, Root: lir.Filter{
		Input: grouped,
		Pred:  beq(bcol("t", "status"), blit("open")),
	}}, `scope "t" exists but is not visible here`)

	// Unlabelled aggregates cannot be referenced — the error says why.
	bindErr(t, lir.Query{Card: lir.CardMany, Root: lir.Filter{
		Input: lir.Aggregate{
			Input:  bscan("tasks", "t"),
			Groups: []lir.GroupTerm{{Expr: bcol("t", "status")}},
			Terms:  []lir.AggTerm{{Fn: lir.AggCount, As: "n"}},
		},
		Pred: lir.Binary{Op: lir.OpGt, L: bcol("stats", "n"), R: blit(10)},
	}}, `unknown scope "stats"`)
}

func TestClosureAboveProject(t *testing.T) {
	shaped := lir.Project{
		Input: bscan("tasks", "t"),
		Scope: "shaped",
		Fields: []lir.ProjField{
			{As: "id", Expr: bcol("t", "id")},
			{As: "score", Expr: lir.Binary{Op: lir.OpMul, L: bcol("t", "priority"), R: blit(2)}},
		},
	}
	bind(t, lir.Query{Card: lir.CardMany, Root: lir.Filter{
		Input: shaped,
		Pred:  lir.Binary{Op: lir.OpGt, L: bcol("shaped", "score"), R: blit(10)},
	}})
	bindErr(t, lir.Query{Card: lir.CardMany, Root: lir.Filter{
		Input: shaped,
		Pred:  beq(bcol("t", "title"), blit("x")),
	}}, `scope "t" exists but is not visible here`)
}

// -
// literal coercion
// -

func TestLiteralCoercion(t *testing.T) {
	// A NULL literal adopts the column's type.
	bind(t, lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"),
		beq(bcol("t", "assignee_id"), blit(nil)))})

	// Numbers coerce by column type, never by guess.
	bind(t, lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"),
		lir.Binary{Op: lir.OpGte, L: bcol("t", "priority"), R: blit(3)})})
	bind(t, lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"),
		lir.Binary{Op: lir.OpLt, L: bcol("t", "estimate"), R: blit(2)})}) // int literal, float column

	bindErr(t, lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"),
		beq(bcol("t", "status"), blit(5)))}, "expected a text value")
	bindErr(t, lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"),
		beq(bcol("t", "priority"), blit(1.5)))}, "expected an int64 value, got 1.5 — cast the column to float64")
	bindErr(t, lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"),
		beq(blit(nil), blit(nil)))}, "bare NULL")
}

// -
// the validation matrix
// -

func TestValidationMatrix(t *testing.T) {
	cases := []struct {
		name string
		q    lir.Query
		want string
	}{
		{"unknown table",
			lir.Query{Card: lir.CardMany, Root: bscan("ghosts", "g")},
			`unknown table "ghosts"`},
		{"scan needs scope",
			lir.Query{Card: lir.CardMany, Root: bscan("tasks", "")},
			"needs a scope label"},
		{"duplicate scope",
			lir.Query{Card: lir.CardMany, Root: lir.Join{
				Left: bscan("tasks", "t"), Right: bscan("comments", "t"),
				Kind: lir.InnerJoin, On: blit(true)}},
			`duplicate scope "t"`},
		{"unknown column",
			lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"), beq(bcol("t", "ghost"), blit("x")))},
			`scope "t" has no column "ghost"`},
		{"unqualified column",
			lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"), beq(bcol("", "status"), blit("x")))},
			"needs a scope qualifier"},
		{"filter needs bool",
			lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"), bcol("t", "title"))},
			"filter predicate must be boolean"},
		{"and needs bools",
			lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"),
				band(bcol("t", "title"), blit(true)))},
			"and needs boolean operands"},
		{"not needs bool",
			lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"),
				lir.Unary{Op: lir.OpNot, X: bcol("t", "title")})},
			"not needs a boolean"},
		{"negate needs numeric",
			lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"),
				beq(lir.Unary{Op: lir.OpNegate, X: bcol("t", "title")}, blit("x")))},
			"cannot negate"},
		{"comparing mixed kinds",
			lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"),
				beq(bcol("t", "priority"), bcol("t", "title")))},
			"cannot compare int64 with text"},
		{"arithmetic needs numbers",
			lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"),
				beq(lir.Binary{Op: lir.OpAdd, L: bcol("t", "title"), R: blit("x")}, blit("y")))},
			"add needs numeric operands"},
		{"bad cast",
			lir.Query{Card: lir.CardMany, Root: bfilter(bscan("tasks", "t"),
				beq(lir.Cast{X: bcol("t", "title"), To: lir.KindBool}, blit(true)))},
			"cannot cast text to bool"},
		{"sum needs numeric",
			lir.Query{Card: lir.CardMany, Root: lir.Aggregate{Input: bscan("tasks", "t"),
				Terms: []lir.AggTerm{{Fn: lir.AggSum, Arg: bcol("t", "title"), As: "s"}}}},
			"sum requires a numeric argument"},
		{"aggregate needs as",
			lir.Query{Card: lir.CardMany, Root: lir.Aggregate{Input: bscan("tasks", "t"),
				Terms: []lir.AggTerm{{Fn: lir.AggCount}}}},
			"needs an output name"},
		{"duplicate aggregate name",
			lir.Query{Card: lir.CardMany, Root: lir.Aggregate{Input: bscan("tasks", "t"),
				Terms: []lir.AggTerm{{Fn: lir.AggCount, As: "n"}, {Fn: lir.AggCount, As: "n"}}}},
			`duplicate aggregate output name "n"`},
		{"unknown aggregate fn",
			lir.Query{Card: lir.CardMany, Root: lir.Aggregate{Input: bscan("tasks", "t"),
				Terms: []lir.AggTerm{{Fn: "median", Arg: bcol("t", "priority"), As: "m"}}}},
			`unknown aggregate function "median"`},
		{"empty aggregate",
			lir.Query{Card: lir.CardMany, Root: lir.Aggregate{Input: bscan("tasks", "t")}},
			"at least one group or term"},
		{"group needs name",
			lir.Query{Card: lir.CardMany, Root: lir.Aggregate{Input: bscan("tasks", "t"),
				Groups: []lir.GroupTerm{{Expr: lir.Binary{Op: lir.OpAdd, L: bcol("t", "priority"), R: blit(1)}}},
				Terms:  []lir.AggTerm{{Fn: lir.AggCount, As: "n"}}}},
			"group expression needs an output name"},
		{"projection needs fields",
			lir.Query{Card: lir.CardMany, Root: lir.Project{Input: bscan("tasks", "t")}},
			"projection has no fields"},
		{"duplicate projection field",
			lir.Query{Card: lir.CardMany, Root: lir.Project{Input: bscan("tasks", "t"),
				Fields: []lir.ProjField{
					{As: "x", Expr: bcol("t", "id")},
					{As: "x", Expr: bcol("t", "title")}}}},
			`duplicate projection field "x"`},
		{"spread collides with field",
			lir.Query{Card: lir.CardMany, Root: lir.Project{Input: bscan("tasks", "t"),
				Spread: []string{"t"},
				Fields: []lir.ProjField{{As: "title", Expr: bcol("t", "id")}}}},
			`duplicate projection field "title"`},
		{"spread of unknown scope",
			lir.Query{Card: lir.CardMany, Root: lir.Project{Input: bscan("tasks", "t"),
				Spread: []string{"z"},
				Fields: []lir.ProjField{{As: "id", Expr: bcol("t", "id")}}}},
			`spread scope "z"`},
		{"order needs terms",
			lir.Query{Card: lir.CardMany, Root: lir.Order{Input: bscan("tasks", "t")}},
			"order needs at least one term"},
		{"order by array",
			lir.Query{
				Card: lir.CardMany,
				Root: lir.Order{
					Input: bscan("boards", "b"),
					Terms: []lir.OrderTerm{
						{Expr: lir.Array{Rel: lir.Order{
							Input: bfilter(bscan("tasks", "t"), beq(bcol("t", "board_id"), bcol("b", "id"))),
							Terms: []lir.OrderTerm{{Expr: bcol("t", "id")}},
						}}},
					},
				},
			},
			"cannot order by a array value"},
		{"negative offset",
			lir.Query{Card: lir.CardMany, Root: lir.Slice{Input: bscan("tasks", "t"), Offset: -1}},
			"offset must be >= 0"},
		{"negative limit",
			lir.Query{Card: lir.CardMany, Root: lir.Slice{Input: bscan("tasks", "t"), Limit: new(int(-2))}},
			"limit must be >= 0"},
		{"join kind",
			lir.Query{Card: lir.CardMany, Root: lir.Join{
				Left: bscan("tasks", "t"), Right: bscan("users", "u"),
				Kind: "cross", On: blit(true)}},
			`unsupported join kind "cross"`},
		{"join on bool",
			lir.Query{Card: lir.CardMany, Root: lir.Join{
				Left: bscan("tasks", "t"), Right: bscan("users", "u"),
				Kind: lir.InnerJoin, On: bcol("t", "title")}},
			"join condition must be boolean"},
		{"dependent join right side",
			lir.Query{Card: lir.CardMany, Root: lir.Join{
				Left: bscan("boards", "b"),
				Right: bfilter(bscan("tasks", "t"),
					beq(bcol("t", "board_id"), bcol("b", "id"))),
				Kind: lir.InnerJoin, On: blit(true)}},
			"join right side references"},
		{"dependent join through a projection",
			lir.Query{Card: lir.CardMany, Root: lir.Join{
				Left: bscan("boards", "b"),
				Right: lir.Project{Input: bscan("tasks", "t"),
					Fields: []lir.ProjField{{As: "owner", Expr: bcol("b", "owner_id")}}},
				Kind: lir.LeftJoin, On: blit(true)}},
			"join right side references"},
		{"spread across a crossing boundary",
			lir.Query{Card: lir.CardMany, Root: lir.Project{
				Input:  bscan("boards", "b"),
				Spread: []string{"b"},
				Fields: []lir.ProjField{{As: "tasks", Expr: lir.Array{Rel: lir.Project{
					Input:  bfilter(bscan("tasks", "t"), beq(bcol("t", "board_id"), bcol("b", "id"))),
					Spread: []string{"b"}, // outer scope: visible for refs, not spreadable
					Fields: []lir.ProjField{{As: "id", Expr: bcol("t", "id")}},
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
	// tasks and users both expose `id`, so the joined row type collides;
	// project to a unique output before the root renders it.
	q := bind(t, lir.Query{Card: lir.CardMany, Root: lir.Project{
		Input: lir.Filter{
			Input: lir.Join{
				Left:  bscan("tasks", "t"),
				Right: bscan("users", "u"),
				Kind:  lir.LeftJoin,
				On:    beq(bcol("t", "assignee_id"), bcol("u", "id")),
			},
			Pred: beq(bcol("u", "name"), blit("ada")),
		},
		Scope: "j",
		Fields: []lir.ProjField{
			{As: "task", Expr: bcol("t", "title")},
			{As: "who", Expr: bcol("u", "name")},
		},
	}})
	proj, ok := q.Root.(*bound.Project)
	if !ok {
		t.Fatalf("root = %T", q.Root)
	}
	if _, ok := proj.In.(*bound.Filter); !ok {
		t.Fatalf("project input = %T, want *bound.Filter", proj.In)
	}

	zq := bind(t, lir.Query{Card: lir.CardMany, Root: lir.Slice{Input: bscan("tasks", "t"), Limit: new(int(0))}})
	if got := zq.Root.Card(); !got.AtMostOne() || got.Max != 0 {
		t.Fatalf("limit 0 card = %v, want 0..0", got)
	}
}

// A join input may be correlated with an enclosing query — only sibling
// dependence is rejected. Both sides here reference the outer board scope.
func TestJoinCorrelatedWithEnclosingScope(t *testing.T) {
	bind(t, lir.Query{
		Card: lir.CardMany,
		Root: lir.Project{
			Input:  bscan("boards", "b"),
			Spread: []string{"b"},
			Fields: []lir.ProjField{
				{As: "pairs", Expr: lir.Array{Rel: lir.Order{
					Input: lir.Project{
						Input: lir.Join{
							Left: bfilter(bscan("tasks", "t"),
								beq(bcol("t", "board_id"), bcol("b", "id"))),
							Right: bfilter(bscan("users", "u"),
								beq(bcol("u", "id"), bcol("b", "owner_id"))),
							Kind: lir.InnerJoin,
							On:   beq(bcol("t", "assignee_id"), bcol("u", "id")),
						},
						Fields: []lir.ProjField{
							{As: "task", Expr: bcol("t", "title")},
							{As: "user", Expr: bcol("u", "name")},
						},
						Scope: "pair",
					},
					Terms: []lir.OrderTerm{{Expr: bcol("pair", "task")}},
				}}},
			},
		},
	})
}
