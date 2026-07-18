// Tracker — a team task-tracking product built entirely on Rad's generated
// client, connected to a Rad server over the wire. This is the proof of the
// developer experience:
//
//	rad.schema.yaml  ->  rad migrate  ->  rad generate  ->  this file
//
// Everything below uses examples/demo/generated (typed models, query builders,
// transactions) speaking rad:// to a server — no SQL, no IR, no keys, and
// no cgo in the application. Set RAD_URL to point elsewhere (default
// rad://localhost:7237); when rad.schema.yaml evolves, `rad generate` keeps this
// compiling.
package main

import (
	"context"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"time"

	tracker "demo/generated"
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		os.Exit(1)
	}
}

func ptr[T any](v T) *T { return &v }

func run() error {
	ctx := context.Background()

	url := os.Getenv("RAD_URL")
	if url == "" {
		url = "rad://localhost:7237"
	}
	db, err := tracker.Connect(url)
	if err != nil {
		return err
	}
	if err := db.Ping(ctx); err != nil {
		return fmt.Errorf("no Rad server at %s (start one with `rad serve`): %w", url, err)
	}
	fmt.Printf("connected to %s\n", url)

	// The client carries its schema: migrate the remote database on startup.
	steps, err := db.Migrate(ctx)
	if err != nil {
		return err
	}
	fmt.Printf("── migrate: %d schema steps applied\n\n", len(steps))

	// Accounts: username and password signup and login.
	fmt.Println("── signup & login")
	ada, err := signup(ctx, db, "ada", "hunter2", ptr("ada@tracker.dev"))
	if err != nil {
		return err
	}
	grace, err := signup(ctx, db, "grace", "correcthorse", nil)
	if err != nil {
		return err
	}
	linus, err := signup(ctx, db, "linus", "diving", nil)
	if err != nil {
		return err
	}

	// Duplicate usernames are impossible (unique index).
	if _, err := signup(ctx, db, "ada", "whatever", nil); err != nil {
		fmt.Printf("   duplicate username rejected: %v\n", err)
	}

	session, err := login(ctx, db, "ada", "hunter2")
	if err != nil {
		return err
	}
	fmt.Printf("   ada logged in, session %s…\n", session.Token[:8])
	if _, err := login(ctx, db, "ada", "wrong"); err != nil {
		fmt.Printf("   bad password rejected: %v\n", err)
	}
	who, err := whoami(ctx, db, session.Token)
	if err != nil {
		return err
	}
	fmt.Printf("   session resolves to %s\n\n", who.Username)

	// Seed a team, board, and labels. Each write is its own execution
	// program; a following create consumes the previous row's id.
	fmt.Println("── seed team, board, labels, tasks")
	var board tracker.Board
	var bugLabel, shipLabel tracker.Label
	err = func() error {
		team, err := db.Teams.Create(ctx, tracker.TeamCreate{Name: "Radlabs"})
		if err != nil {
			return err
		}
		for _, u := range []tracker.Account{ada, grace, linus} {
			role := "member"
			if u.ID == ada.ID {
				role = "owner"
			}
			if _, err := db.TeamMembers.Create(ctx, tracker.TeamMemberCreate{
				TeamID: team.ID, UserID: u.ID, Role: ptr(role),
			}); err != nil {
				return err
			}
		}
		board, err = db.Boards.Create(ctx, tracker.BoardCreate{TeamID: team.ID, Name: "Launch v0"})
		if err != nil {
			return err
		}
		bugLabel, err = db.Labels.Create(ctx, tracker.LabelCreate{TeamID: team.ID, Name: "bug", HexColor: ptr("#e74c3c")})
		if err != nil {
			return err
		}
		shipLabel, err = db.Labels.Create(ctx, tracker.LabelCreate{TeamID: team.ID, Name: "ship-blocker"})
		if err != nil {
			return err
		}

		// A task tree: parent with two subtasks (self-referential FK).
		parent, err := db.Tasks.Create(ctx, tracker.TaskCreate{
			BoardID: board.ID, Title: "Ship the demo", CreatorID: ada.ID,
			Priority: ptr(int64(1)), AssigneeID: &ada.ID,
			DueAt: ptr(time.Now().Add(48 * time.Hour).UnixMilli()),
		})
		if err != nil {
			return err
		}
		sub1, err := db.Tasks.Create(ctx, tracker.TaskCreate{
			BoardID: board.ID, Title: "Write the walkthrough", CreatorID: ada.ID,
			ParentID: &parent.ID, AssigneeID: &grace.ID, Priority: ptr(int64(2)),
			Estimate: ptr(3.5),
		})
		if err != nil {
			return err
		}
		if _, err := db.Tasks.Create(ctx, tracker.TaskCreate{
			BoardID: board.ID, Title: "Fix nested include bug", CreatorID: grace.ID,
			ParentID: &parent.ID, Priority: ptr(int64(1)),
			Description: ptr("children arrays lose their order"),
		}); err != nil {
			return err
		}
		// Labels via the join table, comments on a subtask.
		if _, err := db.TaskLabels.Create(ctx, tracker.TaskLabelCreate{TaskID: parent.ID, LabelID: shipLabel.ID}); err != nil {
			return err
		}
		if _, err := db.Comments.Create(ctx, tracker.CommentCreate{
			TaskID: sub1.ID, AuthorID: linus.ID, Body: "drafting tonight",
		}); err != nil {
			return err
		}
		return nil
	}()
	if err != nil {
		return err
	}
	fmt.Println("   seeded ✓")

	// Foreign keys hold even inside the typed API.
	if _, err := db.Tasks.Create(ctx, tracker.TaskCreate{
		BoardID: "no-such-board", Title: "orphan", CreatorID: ada.ID,
	}); err != nil {
		fmt.Printf("   dangling board_id rejected: %v\n\n", err)
	}

	// The board view is one query with a nested JSON include tree.
	fmt.Println("── board view (nested include tree, 3 levels deep)")
	launch, ok, err := db.Boards.Query().
		IDEq(board.ID).
		IncludeTasks(func(t *tracker.TaskInclude) {
			t.OrderByPriority().OrderByCreatedAt().
				IncludeAssignee().
				IncludeComments(func(c *tracker.CommentInclude) {
					c.OrderByID().IncludeAuthor()
				}).
				IncludeTaskLabels(func(tl *tracker.TaskLabelInclude) {
					tl.OrderByTaskID().OrderByLabelID().IncludeLabel()
				})
		}).
		First(ctx)
	if err != nil || !ok {
		return fmt.Errorf("board view: ok=%v err=%w", ok, err)
	}
	pretty, _ := json.MarshalIndent(launch, "   ", "  ")
	fmt.Printf("   %s\n\n", pretty)

	// Typed queries: filters, ordering, pagination, and NULL.
	fmt.Println("── queries")
	graceTasks, err := db.Tasks.Query().
		AssigneeIDEq(grace.ID).
		StatusNe("done").
		OrderByPriority().
		All(ctx)
	if err != nil {
		return err
	}
	fmt.Printf("   grace's open tasks: %d (%q)\n", len(graceTasks), graceTasks[0].Title)

	unassigned, err := db.Tasks.Query().
		BoardIDEq(board.ID).
		AssigneeIDNull().
		OrderByID().
		All(ctx)
	if err != nil {
		return err
	}
	fmt.Printf("   unassigned on the board: %d (%q)\n", len(unassigned), unassigned[0].Title)

	page2, err := db.Tasks.Query().
		BoardIDEq(board.ID).
		OrderByCreatedAt().
		Offset(1).Limit(1).
		All(ctx)
	if err != nil {
		return err
	}
	fmt.Printf("   page 2 of 3 (size 1): %q\n", page2[0].Title)

	dueSoon, err := db.Tasks.Query().
		DueAtLte(time.Now().Add(72 * time.Hour).UnixMilli()).
		OrderByDueAt().OrderByID().
		All(ctx)
	if err != nil {
		return err
	}
	fmt.Printf("   due within 72h: %d\n\n", len(dueSoon))

	// Aggregates: board stats without fetching the tasks.
	// The card a Tracker board would show — counts by status, an average
	// estimate, the next deadline — each one query that folds server-side.
	fmt.Println("── board stats (aggregates, no rows fetched)")
	onBoard := func() *tracker.TaskQuery { return db.Tasks.Query().BoardIDEq(board.ID) }
	// One grouped fold answers the whole card: counts per status, ordered by
	// the group key, in a single round trip.
	byStatus, err := onBoard().CountByStatus(ctx)
	if err != nil {
		return err
	}
	var total, done int64
	for _, g := range byStatus {
		total += g.Count
		if g.Status == "done" {
			done = g.Count
		}
	}
	fmt.Printf("   Launch v0: %d tasks · %d open · %d done (GROUP BY, one query)\n", total, total-done, done)

	avgEst, err := onBoard().AvgEstimate(ctx)
	if err != nil {
		return err
	}
	nextDue, err := onBoard().MinDueAt(ctx)
	if err != nil {
		return err
	}
	// avg/min return nil when no rows have a value — no "0 of nothing" lie.
	switch {
	case avgEst == nil:
		fmt.Println("   avg estimate: — (nothing estimated yet)")
	default:
		fmt.Printf("   avg estimate: %.1fh\n", *avgEst)
	}
	if nextDue != nil {
		fmt.Printf("   next deadline: %s\n\n", time.UnixMilli(*nextDue).Format(time.RFC3339))
	}

	// Mutations: patch, clear-to-NULL, and delete-restrict.
	fmt.Println("── mutations")
	fix := unassigned[0]
	fix, _, err = db.Tasks.Update(ctx, fix.ID, tracker.TaskPatch{
		AssigneeID: &linus.ID, Status: ptr("doing"),
	})
	if err != nil {
		return err
	}
	fmt.Printf("   %q claimed by linus, status=%s\n", fix.Title, fix.Status)

	fix, _, err = db.Tasks.Update(ctx, fix.ID, tracker.TaskPatch{
		Status: ptr("done"), ClearAssigneeID: true,
	})
	if err != nil {
		return err
	}
	fmt.Printf("   %q done, assignee cleared (nil=%v)\n", fix.Title, fix.AssigneeID == nil)

	// grace still owns tasks and comments reference linus: restrict wins.
	if _, err := db.Accounts.Delete(ctx, grace.ID); err != nil {
		fmt.Printf("   deleting grace blocked: %v\n", err)
	}

	// Read, decide, write: reassign a task after checking its owner. Each
	// step is its own execution program.
	fmt.Println("\n── reassign (read, decide, write)")
	target := graceTasks[0]
	t, ok, err := db.Tasks.Get(ctx, target.ID)
	if err != nil || !ok {
		return errors.Join(err, errors.New("task vanished"))
	}
	if t.AssigneeID == nil || *t.AssigneeID == grace.ID {
		if _, _, err := db.Tasks.Update(ctx, target.ID, tracker.TaskPatch{AssigneeID: &ada.ID}); err != nil {
			return err
		}
		fmt.Println("   reassigned to ada ✓")
	}

	// Cleanup path: children first, then the parent row.
	fmt.Println("\n── delete (restrict ordering)")
	if _, err := db.Labels.Delete(ctx, shipLabel.ID); err != nil {
		fmt.Printf("   label in use: %v\n", err)
	}
	tls, err := db.TaskLabels.Query().LabelIDEq(shipLabel.ID).OrderByTaskID().OrderByLabelID().All(ctx)
	if err != nil {
		return err
	}
	for _, tl := range tls {
		if _, err := db.TaskLabels.Delete(ctx, tl.TaskID, tl.LabelID); err != nil {
			return err
		}
	}
	if ok, err := db.Labels.Delete(ctx, shipLabel.ID); err != nil || !ok {
		return fmt.Errorf("label delete after unlink: ok=%v err=%w", ok, err)
	}
	fmt.Printf("   unlinked %d task_labels, then deleted the label ✓\n", len(tls))
	_ = bugLabel

	fmt.Println("\ndone — typed app, LIR under the hood, zero SQL.")
	return nil
}

// hash is application code using only the generated client API.

func hash(password string) string {
	sum := sha256.Sum256([]byte(password))
	return hex.EncodeToString(sum[:])
}

func signup(ctx context.Context, db *tracker.Client, username, password string, email *string) (tracker.Account, error) {
	u, err := db.Accounts.Create(ctx, tracker.AccountCreate{
		Username:     username,
		PasswordHash: hash(password),
		Email:        email,
	})
	if err != nil {
		return tracker.Account{}, err
	}
	fmt.Printf("   signed up %s (id %s…)\n", u.Username, u.ID[:8])
	return u, nil
}

func login(ctx context.Context, db *tracker.Client, username, password string) (tracker.Session, error) {
	u, ok, err := db.Accounts.ByUsername(ctx, username)
	if err != nil {
		return tracker.Session{}, err
	}
	if !ok || subtle.ConstantTimeCompare([]byte(u.PasswordHash), []byte(hash(password))) != 1 {
		return tracker.Session{}, errors.New("invalid credentials")
	}
	return db.Sessions.Create(ctx, tracker.SessionCreate{
		UserID:    u.ID,
		ExpiresAt: time.Now().Add(24 * time.Hour).UnixMilli(),
	})
}

func whoami(ctx context.Context, db *tracker.Client, token string) (tracker.Account, error) {
	s, ok, err := db.Sessions.Get(ctx, token)
	if err != nil {
		return tracker.Account{}, err
	}
	if !ok || s.ExpiresAt < time.Now().UnixMilli() {
		return tracker.Account{}, errors.New("session expired")
	}
	u, ok, err := db.Accounts.Get(ctx, s.UserID)
	if err != nil || !ok {
		return tracker.Account{}, errors.Join(err, errors.New("user missing"))
	}
	return u, nil
}
