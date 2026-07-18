package main

import (
	"bytes"
	"fmt"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/spf13/cobra"

	"github.com/Southclaws/rad/rad/codegen"
	catalogschema "github.com/Southclaws/rad/rad/engine/02_catalog/schema"
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
	var diffReceived, migrateReceived bool
	schema := "tables:\n  - id: 1\n    name: accounts\n    columns:\n      - id: 1\n        name: id\n        type: string\n    primary_key: [id]\n"
	parsed, err := catalogschema.Parse("rad.schema.yaml", []byte(schema))
	if err != nil {
		t.Fatal(err)
	}
	hash, err := parsed.Canonical().Hash()
	if err != nil {
		t.Fatal(err)
	}
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		switch r.URL.Path {
		case "/schema/diff":
			diffReceived = true
			fmt.Fprintf(w, `{"current_version":0,"current_hash":"sha256:empty","desired_hash":%q,"changes":[{"kind":"create_table","summary":"create table accounts","table":"accounts"}],"program":{},"destructive":[],"blocking":[]}`, hash)
		case "/schema/migrate":
			migrateReceived = true
			fmt.Fprintf(w, `{"schema_version":1,"schema_hash":%q,"schema":{"tables":[{"id":1,"name":"accounts","columns":[{"id":1,"name":"id","type":"text"}],"primary_key":["id"]}]},"changes":[{"kind":"create_table","summary":"create table accounts","table":"accounts"}]}`, hash)
		default:
			http.Error(w, "unexpected request", http.StatusNotFound)
		}
	}))
	defer server.Close()

	httpURL, err := url.Parse(server.URL)
	if err != nil {
		t.Fatal(err)
	}
	target := "rad://" + httpURL.Host
	dir := t.TempDir()
	file := filepath.Join(dir, defaultSchemaFile)
	if err := os.WriteFile(file, []byte(schema), 0o600); err != nil {
		t.Fatal(err)
	}
	configFile := filepath.Join(dir, defaultConfigFile)
	if err := os.WriteFile(configFile, []byte("database_url: "+target+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}

	cmd := schemaCmd()
	var output bytes.Buffer
	cmd.SetOut(&output)
	cmd.SetArgs([]string{"migrate", "--config", configFile, "--file", file})
	if err := cmd.Execute(); err != nil {
		t.Fatal(err)
	}

	if !diffReceived || !migrateReceived {
		t.Fatalf("requests: diff=%t migrate=%t", diffReceived, migrateReceived)
	}
	if !strings.Contains(output.String(), "Schema version 1 committed") {
		t.Fatalf("output = %q", output.String())
	}
	if _, err := os.Stat(filepath.Join(dir, defaultStateDir, "changelog", "00000001.rad.schema.yaml")); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(dir, "generated", codegen.GoClientFilename)); err != nil {
		t.Fatalf("generated client: %v", err)
	}
}

func TestMigrateCmdHasNoStorageFlag(t *testing.T) {
	cmd := schemaCmd()
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
	schema := schemaCmd()
	commands := []*cobra.Command{schema, generateCmd(), validateCmd()}
	for _, command := range commands {
		flags := command.Flags()
		if command == schema {
			flags = command.PersistentFlags()
		}
		if got := flags.Lookup("file").DefValue; got != defaultSchemaFile {
			t.Fatalf("%s schema default = %q, want %q", command.Name(), got, defaultSchemaFile)
		}
	}
	if got := schema.PersistentFlags().Lookup("config").DefValue; got != defaultConfigFile {
		t.Fatalf("config default = %q, want %q", got, defaultConfigFile)
	}
	if defaultStateDir != "rad.state" {
		t.Fatalf("state directory = %q, want rad.state", defaultStateDir)
	}
}
