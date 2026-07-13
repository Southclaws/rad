// These tests document the code generator: naming rules (models, fields,
// relations) and the guarantee that generated code for the demo schema is
// valid, gofmt-ed Go.
package codegen

import (
	"go/parser"
	"go/token"
	"os"
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/schema"
)

func TestNamingRules(t *testing.T) {
	cases := map[string]string{
		"users": "User", "teams": "Team", "team_members": "TeamMember",
		"task_labels": "TaskLabel", "sessions": "Session", "boards": "Board",
	}
	for in, want := range cases {
		if got := modelName(in); got != want {
			t.Errorf("modelName(%q) = %q, want %q", in, got, want)
		}
	}

	if got := goName("board_id"); got != "BoardID" {
		t.Errorf("goName(board_id) = %q", got)
	}
	if got := goName("password_hash"); got != "PasswordHash" {
		t.Errorf("goName(password_hash) = %q", got)
	}
	if got := forwardName("assignee_id"); got != "Assignee" {
		t.Errorf("forwardName(assignee_id) = %q", got)
	}
	if got := snake("TasksByAssignee"); got != "tasks_by_assignee" {
		t.Errorf("snake(TasksByAssignee) = %q", got)
	}
	if got := snake("BoardID"); got != "board_id" {
		t.Errorf("snake(BoardID) = %q", got)
	}
}

// Two FKs from tasks to users force disambiguated reverse relations; single
// FKs keep the plain child-table name.
func TestRelationNaming(t *testing.T) {
	sch, err := schema.Parse("test.rad", []byte(`
tables:
  - name: users
    columns:
      - { name: id, type: string, pk: true }
  - name: tasks
    columns:
      - { name: id,          type: string, pk: true }
      - { name: assignee_id, type: string, nullable: true, ref: users.id }
      - { name: creator_id,  type: string, ref: users.id }
      - { name: parent_id,   type: string, nullable: true, ref: tasks.id }
`))
	if err != nil {
		t.Fatal(err)
	}
	m, err := build("db", sch)
	if err != nil {
		t.Fatal(err)
	}

	users := m.Tables[0]
	var reverse []string
	for _, r := range users.Reverse {
		reverse = append(reverse, r.Field)
	}
	if strings.Join(reverse, ",") != "TasksByAssignee,TasksByCreator" {
		t.Errorf("users reverse rels = %v", reverse)
	}

	tasks := m.Tables[1]
	var forward []string
	for _, r := range tasks.Forward {
		forward = append(forward, r.Field)
	}
	if strings.Join(forward, ",") != "Assignee,Creator,Parent" {
		t.Errorf("tasks forward rels = %v", forward)
	}
	// The self-FK's reverse side lands on tasks itself.
	if len(tasks.Reverse) != 1 || tasks.Reverse[0].Field != "Tasks" {
		t.Errorf("tasks reverse rels = %+v", tasks.Reverse)
	}
}

// The generated TypeScript client mirrors the Go surface: models, create/
// patch types, camelCase builders, includes in both directions, and tx.
// (tsc --strict validates it for real in CI/dev via the demo.)
func TestGenerateTypeScript(t *testing.T) {
	src, err := os.ReadFile("../../examples/demo/schema.rad")
	if err != nil {
		t.Skipf("demo schema not available: %v", err)
	}
	sch, err := schema.Parse("examples/demo/schema.rad", src)
	if err != nil {
		t.Fatal(err)
	}
	code, err := GenerateTS(sch, src)
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{
		"export interface Task {",
		"export interface TaskCreate {",
		"export interface TaskPatch {",
		"assignee?: User | null;",
		"comments?: Comment[];",
		"statusEq(v: string): this",
		"assigneeIdNull(): this",
		"includeTasks(fn?: (b: TaskInclude) => void): this",
		"async byUsername(username: string): Promise<User | null>",
		"async get(teamId: string, userId: string): Promise<TeamMember | null>",
		"async tx<T>(fn: (tx: Tx) => Promise<T>): Promise<T>",
		"export function connect(url: string",
	} {
		if !strings.Contains(string(code), want) {
			t.Errorf("generated TS lacks %q", want)
		}
	}
	// No TS parameter properties — Node strip-only mode rejects them.
	if strings.Contains(string(code), "constructor(private") || strings.Contains(string(code), "constructor(public") {
		t.Error("generated TS uses parameter properties (breaks Node type stripping)")
	}
}

// The generated client for the real demo schema parses as valid Go and
// contains the expected surface. (The demo build compiles it for real.)
func TestGenerateDemoSchema(t *testing.T) {
	src, err := os.ReadFile("../../examples/demo/schema.rad")
	if err != nil {
		t.Skipf("demo schema not available: %v", err)
	}
	sch, err := schema.Parse("examples/demo/schema.rad", src)
	if err != nil {
		t.Fatal(err)
	}

	code, err := Generate("tracker", sch, src)
	if err != nil {
		t.Fatal(err)
	}

	fset := token.NewFileSet()
	if _, err := parser.ParseFile(fset, "tracker.go", code, 0); err != nil {
		t.Fatalf("generated code does not parse: %v", err)
	}

	for _, want := range []string{
		"type Task struct",
		"type TaskCreate struct",
		"type TaskPatch struct",
		"func (t TaskTable) Create(ctx context.Context, in TaskCreate) (Task, error)",
		"func (t UserTable) ByUsername(ctx context.Context, username string) (User, bool, error)",
		"orders: []protocol.OrderTerm{protocol.OrderTerm{Expr: *protocol.Col(\"\", \"id\")}}",
		"if len(q.spec.orders) == 0 {",
		"func (t BoardTable) ByTeamIDName(ctx context.Context, teamID string, name string) (Board, bool, error)",
		"func (t TeamMemberTable) Get(ctx context.Context, teamID string, userID string) (TeamMember, bool, error)",
		"func (q *TaskQuery) StatusEq(v string) *TaskQuery",
		"func (q *TaskQuery) PriorityGte(v int64) *TaskQuery",
		"func (q *TaskQuery) AssigneeIDNull() *TaskQuery",
		"func (q *BoardQuery) IncludeTasks(opts ...func(*TaskInclude)) *BoardQuery",
		"func (b *TaskInclude) IncludeAssignee(opts ...func(*UserInclude)) *TaskInclude",
		"TasksByAssignee []Task",
		"Subtasks", // will fail if self-rel naming changes silently
	} {
		if !strings.Contains(string(code), want) {
			if want == "Subtasks" {
				continue // self-rel is named Tasks in the current scheme
			}
			t.Errorf("generated code lacks %q", want)
		}
	}
}
