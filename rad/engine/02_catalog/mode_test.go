// Catalog management mode: set once when the database is initialised,
// immutable afterwards, defaulting to direct management. Enforcement is the
// transport's job; these tests cover only storage and the set-once rule.
package catalog_test

import (
	"strings"
	"testing"

	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
)

// A database that was never initialised through InitMode — embedded engines,
// tests, pre-mode data directories — is directly managed.
func TestModeDefaultsToDirect(t *testing.T) {
	cat, _, ctx := newCatalog(t)

	mode, err := cat.Mode(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if mode != model.ModeDirect {
		t.Errorf("unstamped database mode = %q, want %q", mode, model.ModeDirect)
	}
}

// InitMode with no preference stamps the direct default; the stamp persists
// and later boots settle on it.
func TestInitModeStampsDefault(t *testing.T) {
	cat, _, ctx := newCatalog(t)

	mode, err := cat.InitMode(ctx, "")
	if err != nil {
		t.Fatal(err)
	}
	if mode != model.ModeDirect {
		t.Errorf("InitMode(\"\") = %q, want %q", mode, model.ModeDirect)
	}

	again, err := cat.InitMode(ctx, "")
	if err != nil {
		t.Fatal(err)
	}
	if again != model.ModeDirect {
		t.Errorf("second InitMode(\"\") = %q, want %q", again, model.ModeDirect)
	}
}

// An explicit request on a fresh database stamps that mode permanently.
func TestInitModeStampsRequested(t *testing.T) {
	cat, _, ctx := newCatalog(t)

	mode, err := cat.InitMode(ctx, model.ModeSchema)
	if err != nil {
		t.Fatal(err)
	}
	if mode != model.ModeSchema {
		t.Fatalf("InitMode(schema) = %q, want schema", mode)
	}

	// Reads and preference-free boots see the stored mode.
	if got, err := cat.Mode(ctx); err != nil || got != model.ModeSchema {
		t.Errorf("Mode() = %q, %v; want schema", got, err)
	}
	if got, err := cat.InitMode(ctx, ""); err != nil || got != model.ModeSchema {
		t.Errorf("InitMode(\"\") after stamp = %q, %v; want schema", got, err)
	}
	// Re-requesting the stored mode is a no-op, not an error.
	if got, err := cat.InitMode(ctx, model.ModeSchema); err != nil || got != model.ModeSchema {
		t.Errorf("InitMode(schema) after stamp = %q, %v; want schema", got, err)
	}
}

// The mode is set once: explicitly requesting a different mode on an
// existing database is an error, in both directions. The default stamp
// counts — a database booted plain and later booted --catalog-mode=schema
// is a mismatch, not an upgrade.
func TestInitModeMismatchFails(t *testing.T) {
	cases := []struct {
		name             string
		stamped, request model.Mode
	}{
		{"schema database asked to be direct", model.ModeSchema, model.ModeDirect},
		{"direct database asked to be schema", model.ModeDirect, model.ModeSchema},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cat, _, ctx := newCatalog(t)
			if _, err := cat.InitMode(ctx, tc.stamped); err != nil {
				t.Fatal(err)
			}

			_, err := cat.InitMode(ctx, tc.request)
			if err == nil {
				t.Fatalf("InitMode(%s) on a %s database should fail", tc.request, tc.stamped)
			}
			if !strings.Contains(err.Error(), "set once") {
				t.Errorf("error should explain the set-once rule, got: %v", err)
			}

			// The failed request must not have moved the stored mode.
			if got, _ := cat.Mode(ctx); got != tc.stamped {
				t.Errorf("mode after failed InitMode = %q, want %q", got, tc.stamped)
			}
		})
	}
}

func TestParseMode(t *testing.T) {
	for _, valid := range []string{"direct", "schema"} {
		if _, err := model.ParseMode(valid); err != nil {
			t.Errorf("ParseMode(%q) failed: %v", valid, err)
		}
	}
	for _, invalid := range []string{"", "Direct", "managed", "yes"} {
		if _, err := model.ParseMode(invalid); err == nil {
			t.Errorf("ParseMode(%q) should fail", invalid)
		}
	}
}
