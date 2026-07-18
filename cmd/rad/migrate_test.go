package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestMigrateURL(t *testing.T) {
	t.Run("flag wins", func(t *testing.T) {
		t.Setenv("RAD_URL", "rad://environment")
		if got := migrateURL("rad://flag"); got != "rad://flag" {
			t.Fatalf("migrateURL() = %q, want rad://flag", got)
		}
	})

	t.Run("environment fallback", func(t *testing.T) {
		t.Setenv("RAD_URL", "rad://environment")
		if got := migrateURL(""); got != "rad://environment" {
			t.Fatalf("migrateURL() = %q, want rad://environment", got)
		}
	})

	t.Run("localhost default", func(t *testing.T) {
		t.Setenv("RAD_URL", "")
		if got := migrateURL(""); got != defaultMigrateURL {
			t.Fatalf("migrateURL() = %q, want %q", got, defaultMigrateURL)
		}
	})
}

func TestMigrateCmdSendsSchemaToServer(t *testing.T) {
	var received string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.URL.Path != "/migrate" {
			http.Error(w, "unexpected request", http.StatusNotFound)
			return
		}
		var request struct {
			Schema string `json:"schema"`
		}
		if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
			http.Error(w, err.Error(), http.StatusBadRequest)
			return
		}
		received = request.Schema
		w.Header().Set("Content-Type", "application/json")
		fmt.Fprint(w, `{"steps":["create table accounts"]}`)
	}))
	defer server.Close()

	httpURL, err := url.Parse(server.URL)
	if err != nil {
		t.Fatal(err)
	}
	target := "rad://" + httpURL.Host
	schema := "tables:\n  - id: 1\n    name: accounts\n"
	file := filepath.Join(t.TempDir(), "schema.rad")
	if err := os.WriteFile(file, []byte(schema), 0o600); err != nil {
		t.Fatal(err)
	}

	cmd := migrateCmd()
	var output bytes.Buffer
	cmd.SetOut(&output)
	cmd.SetArgs([]string{"--url", target, "--file", file})
	if err := cmd.Execute(); err != nil {
		t.Fatal(err)
	}

	if received != schema {
		t.Fatalf("server received schema %q, want %q", received, schema)
	}
	if !strings.Contains(output.String(), "applied 1 steps to "+target) {
		t.Fatalf("output = %q", output.String())
	}
}

func TestMigrateCmdHasNoStorageFlag(t *testing.T) {
	cmd := migrateCmd()
	if flag := cmd.Flags().Lookup("db"); flag != nil {
		t.Fatalf("migrate exposes storage flag --db")
	}
	if flag := cmd.Flags().ShorthandLookup("d"); flag != nil {
		t.Fatalf("migrate exposes storage shorthand -d")
	}
}
