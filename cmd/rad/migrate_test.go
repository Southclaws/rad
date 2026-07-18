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

	"github.com/spf13/cobra"
)

func TestProjectConfig(t *testing.T) {
	dir := t.TempDir()
	filename := filepath.Join(dir, defaultConfigFile)
	if err := os.WriteFile(filename, []byte("database_url: rad://configured\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	config, err := loadProjectConfig(filename)
	if err != nil {
		t.Fatal(err)
	}
	if config.DatabaseURL != "rad://configured" {
		t.Fatalf("database URL = %q", config.DatabaseURL)
	}
}

func TestProjectConfigRejectsInvalidFiles(t *testing.T) {
	for name, source := range map[string]string{
		"missing database_url": "{}\n",
		"invalid database_url": "database_url: http://localhost\n",
		"unknown field":        "database_url: rad://localhost\ndatabase: rad://other\n",
	} {
		t.Run(name, func(t *testing.T) {
			filename := filepath.Join(t.TempDir(), defaultConfigFile)
			if err := os.WriteFile(filename, []byte(source), 0o600); err != nil {
				t.Fatal(err)
			}
			if _, err := loadProjectConfig(filename); err == nil {
				t.Fatal("invalid project config was accepted")
			}
		})
	}
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
	dir := t.TempDir()
	file := filepath.Join(dir, defaultSchemaFile)
	if err := os.WriteFile(file, []byte(schema), 0o600); err != nil {
		t.Fatal(err)
	}
	configFile := filepath.Join(dir, defaultConfigFile)
	if err := os.WriteFile(configFile, []byte("database_url: "+target+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}

	cmd := migrateCmd()
	var output bytes.Buffer
	cmd.SetOut(&output)
	cmd.SetArgs([]string{"--config", configFile, "--file", file})
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
	for _, name := range []string{"db", "url"} {
		if flag := cmd.Flags().Lookup(name); flag != nil {
			t.Fatalf("migrate exposes removed flag --%s", name)
		}
	}
	for _, shorthand := range []string{"d", "u"} {
		if flag := cmd.Flags().ShorthandLookup(shorthand); flag != nil {
			t.Fatalf("migrate exposes removed shorthand -%s", shorthand)
		}
	}
}

func TestProjectFileDefaults(t *testing.T) {
	commands := []*cobra.Command{migrateCmd(), generateCmd(), validateCmd()}
	for _, command := range commands {
		if got := command.Flags().Lookup("file").DefValue; got != defaultSchemaFile {
			t.Fatalf("%s schema default = %q, want %q", command.Name(), got, defaultSchemaFile)
		}
	}
	if got := migrateCmd().Flags().Lookup("config").DefValue; got != defaultConfigFile {
		t.Fatalf("config default = %q, want %q", got, defaultConfigFile)
	}
	if defaultStateDir != "rad.state" {
		t.Fatalf("state directory = %q, want rad.state", defaultStateDir)
	}
}
