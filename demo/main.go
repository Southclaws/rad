// Tracker — a team task-tracking product built entirely on RAD's generated
// client. This is the proof of the developer experience:
//
//	schema.rad  →  rad migrate  →  rad generate  →  this file
//
// Everything below uses demo/generated (typed models, query builders,
// transactions). No SQL, no IR, no keys — and when schema.rad evolves,
// `rad migrate && rad generate` keeps this compiling.
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

	db, err := tracker.Open("data")
	if err != nil {
		return err
	}
	defer db.Close()

	// The client carries its schema: migrate on startup. (Equivalent to
	// `rad migrate -f schema.rad -d data`.)
	steps, err := db.Migrate(ctx)
	if err != nil {
		return err
	}
	fmt.Printf("── migrate: %d schema steps applied\n\n", len(steps))

	// ── 1. Accounts: username+password signup and login. ────────────────
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

	// ── 2. A team, its board, and labels — atomically. ──────────────────
	fmt.Println("── seed team, board, labels, tasks (one transaction)")
	var board tracker.Board
	var bugLabel, shipLabel tracker.Label
	err = db.Tx(ctx, func(tx *tracker.Tx) error {
		team, err := tx.Teams.Create(ctx, tracker.TeamCreate{Name: "Radlabs"})
		if err != nil {
			return err
		}
		for _, u := range []tracker.User{ada, grace, linus} {
			role := "member"
			if u.ID == ada.ID {
				role = "owner"
			}
			if _, err := tx.TeamMembers.Create(ctx, tracker.TeamMemberCreate{
				TeamID: team.ID, UserID: u.ID, Role: ptr(role),
			}); err != nil {
				return err
			}
		}
		board, err = tx.Boards.Create(ctx, tracker.BoardCreate{TeamID: team.ID, Name: "Launch v0"})
		if err != nil {
			return err
		}
		bugLabel, err = tx.Labels.Create(ctx, tracker.LabelCreate{TeamID: team.ID, Name: "bug", HexColor: ptr("#e74c3c")})
		if err != nil {
			return err
		}
		shipLabel, err = tx.Labels.Create(ctx, tracker.LabelCreate{TeamID: team.ID, Name: "ship-blocker"})
		if err != nil {
			return err
		}

		// A task tree: parent with two subtasks (self-referential FK).
		parent, err := tx.Tasks.Create(ctx, tracker.TaskCreate{
			BoardID: board.ID, Title: "Ship the demo", CreatorID: ada.ID,
			Priority: ptr(int64(1)), AssigneeID: &ada.ID,
			DueAt: ptr(time.Now().Add(48 * time.Hour).UnixMilli()),
		})
		if err != nil {
			return err
		}
		sub1, err := tx.Tasks.Create(ctx, tracker.TaskCreate{
			BoardID: board.ID, Title: "Write the walkthrough", CreatorID: ada.ID,
			ParentID: &parent.ID, AssigneeID: &grace.ID, Priority: ptr(int64(2)),
			Estimate: ptr(3.5),
		})
		if err != nil {
			return err
		}
		if _, err := tx.Tasks.Create(ctx, tracker.TaskCreate{
			BoardID: board.ID, Title: "Fix nested include bug", CreatorID: grace.ID,
			ParentID: &parent.ID, Priority: ptr(int64(1)),
			Description: ptr("children arrays lose their order"),
		}); err != nil {
			return err
		}
		// Labels via the join table, comments on a subtask.
		if _, err := tx.TaskLabels.Create(ctx, tracker.TaskLabelCreate{TaskID: parent.ID, LabelID: shipLabel.ID}); err != nil {
			return err
		}
		if _, err := tx.Comments.Create(ctx, tracker.CommentCreate{
			TaskID: sub1.ID, AuthorID: linus.ID, Body: "drafting tonight",
		}); err != nil {
			return err
		}
		return nil
	})
	if err != nil {
		return err
	}
	fmt.Println("   committed atomically ✓")

	// Foreign keys hold even inside the typed API.
	if _, err := db.Tasks.Create(ctx, tracker.TaskCreate{
		BoardID: "no-such-board", Title: "orphan", CreatorID: ada.ID,
	}); err != nil {
		fmt.Printf("   dangling board_id rejected: %v\n\n", err)
	}

	// ── 3. The board view: one query, nested JSON. ──────────────────────
	fmt.Println("── board view (nested include tree, 3 levels deep)")
	launch, ok, err := db.Boards.Query().
		IDEq(board.ID).
		IncludeTasks(func(t *tracker.TaskInclude) {
			t.OrderByPriority().OrderByCreatedAt().
				IncludeAssignee().
				IncludeComments(func(c *tracker.CommentInclude) {
					c.IncludeAuthor()
				}).
				IncludeTaskLabels(func(tl *tracker.TaskLabelInclude) {
					tl.IncludeLabel()
				})
		}).
		First(ctx)
	if err != nil || !ok {
		return fmt.Errorf("board view: ok=%v err=%w", ok, err)
	}
	pretty, _ := json.MarshalIndent(launch, "   ", "  ")
	fmt.Printf("   %s\n\n", pretty)

	// ── 4. Typed queries: filters, ordering, pagination, NULL. ──────────
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
		All(ctx)
	if err != nil {
		return err
	}
	fmt.Printf("   due within 72h: %d\n\n", len(dueSoon))

	// ── 5. Mutations: patch, clear-to-NULL, delete-restrict. ────────────
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
	if _, err := db.Users.Delete(ctx, grace.ID); err != nil {
		fmt.Printf("   deleting grace blocked: %v\n", err)
	}

	// ── 6. Optimistic concurrency: claim races retry cleanly. ───────────
	fmt.Println("\n── conflict & retry")
	target := graceTasks[0]
	attempt := 0
	claim := func() error {
		return db.Tx(ctx, func(tx *tracker.Tx) error {
			attempt++
			t, ok, err := tx.Tasks.Get(ctx, target.ID)
			if err != nil || !ok {
				return errors.Join(err, errors.New("task vanished"))
			}
			if t.AssigneeID != nil && *t.AssigneeID != grace.ID {
				return fmt.Errorf("already claimed by someone else")
			}
			if attempt == 1 {
				// A rival commits first, invalidating our snapshot.
				if _, _, err := db.Tasks.Update(ctx, target.ID, tracker.TaskPatch{AssigneeID: &linus.ID}); err != nil {
					return err
				}
			}
			_, _, err = tx.Tasks.Update(ctx, target.ID, tracker.TaskPatch{AssigneeID: &ada.ID})
			return err
		})
	}
	err = claim()
	fmt.Printf("   first attempt: conflict=%v\n", tracker.IsConflict(err))
	if tracker.IsConflict(err) {
		err = claim()
	}
	fmt.Printf("   retry: %v (linus won the race, ada saw fresh state)\n", err)

	// ── 7. Cleanup path: children first, then the parent row. ───────────
	fmt.Println("\n── delete (restrict ordering)")
	if _, err := db.Labels.Delete(ctx, shipLabel.ID); err != nil {
		fmt.Printf("   label in use: %v\n", err)
	}
	tls, err := db.TaskLabels.Query().LabelIDEq(shipLabel.ID).All(ctx)
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

	fmt.Println("\ndone — typed app, QIR under the hood, zero SQL.")
	return nil
}

// ── auth helpers (application code, generated client only) ──────────────

func hash(password string) string {
	sum := sha256.Sum256([]byte(password))
	return hex.EncodeToString(sum[:])
}

func signup(ctx context.Context, db *tracker.Client, username, password string, email *string) (tracker.User, error) {
	u, err := db.Users.Create(ctx, tracker.UserCreate{
		Username:     username,
		PasswordHash: hash(password),
		Email:        email,
	})
	if err != nil {
		return tracker.User{}, err
	}
	fmt.Printf("   signed up %s (id %s…)\n", u.Username, u.ID[:8])
	return u, nil
}

func login(ctx context.Context, db *tracker.Client, username, password string) (tracker.Session, error) {
	u, ok, err := db.Users.ByUsername(ctx, username)
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

func whoami(ctx context.Context, db *tracker.Client, token string) (tracker.User, error) {
	s, ok, err := db.Sessions.Get(ctx, token)
	if err != nil {
		return tracker.User{}, err
	}
	if !ok || s.ExpiresAt < time.Now().UnixMilli() {
		return tracker.User{}, errors.New("session expired")
	}
	u, ok, err := db.Users.Get(ctx, s.UserID)
	if err != nil || !ok {
		return tracker.User{}, errors.Join(err, errors.New("user missing"))
	}
	return u, nil
}
