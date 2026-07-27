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

	"github.com/Southclaws/rad/cmd/rad/config"
	"github.com/Southclaws/rad/rad/codegen"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	"github.com/Southclaws/rad/rad/protocol"
	projectstate "github.com/Southclaws/rad/rad/state"
)

func TestSchemaMigrateDestructiveDefaultsToNo(t *testing.T) {
	var migrated bool
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		switch request.URL.Path {
		case "/schema/diff":
			fmt.Fprint(w, `{"current_version":1,"current_hash":"sha256:current","desired_hash":"sha256:desired","changes":[{"kind":"delete_column","summary":"delete users.name"}],"program":{},"destructive":[{"kind":"delete_column","summary":"column users.name will be deleted (1 rows contain a value)","rows":1}],"blocking":[]}`)
		case "/schema/migrate":
			migrated = true
			http.Error(w, "migration should not be called", http.StatusInternalServerError)
		default:
			http.NotFound(w, request)
		}
	}))
	defer server.Close()
	directory, configFile := schemaCLIProject(t, server.URL, acceptedSchemaFixture)

	cmd := schemaCmd()
	cmd.SetIn(strings.NewReader(""))
	cmd.SetOut(&bytes.Buffer{})
	cmd.SetArgs([]string{"migrate", "--config", configFile, "--no-generate"})
	err := cmd.Execute()
	if err == nil || !strings.Contains(err.Error(), "cancelled") {
		t.Fatalf("error = %v", err)
	}
	if migrated {
		t.Fatal("non-interactive input implied data-loss consent")
	}
	if _, err := os.Stat(filepath.Join(directory, config.DefaultStateDir, "schema.lock.json")); !os.IsNotExist(err) {
		t.Fatalf("local state changed before commit: %v", err)
	}
}

func TestSchemaMigrateWaitsForOnlinePublicationBeforeWritingLocalState(t *testing.T) {
	final := commandSchemaState(t, 2, "users")
	var transitionReads, schemaReads int
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		switch request.URL.Path {
		case "/schema/diff":
			fmt.Fprintf(w, `{"current_version":1,"current_hash":"sha256:old","desired_hash":%q,"changes":[{"kind":"start_column_replacement","summary":"replace column users.id","table":"users","column":"id"}],"program":{},"destructive":[],"blocking":[]}`, final.SchemaHash)
		case "/schema/migrate":
			fmt.Fprintf(w, `{"schema_version":1,"schema_hash":"sha256:old","schema":{"tables":[]},"changes":[{"kind":"start_column_replacement","summary":"replace column users.id","table":"users","column":"id"}],"desired_hash":%q,"state":"converging","transition_ids":["tr1"]}`, final.SchemaHash)
		case "/schema/transitions/tr1":
			transitionReads++
			fmt.Fprint(w, `{"kind":"transition","transition_id":"tr1","object_id":"column-1","transition_kind":"column_replacement","state":"ready","generation":2,"prerequisites":[],"retained_work_state":"normal","rows_scanned":1,"applied_delta":0,"delta_lag":0}`)
		case "/schema":
			schemaReads++
			if err := json.NewEncoder(w).Encode(final); err != nil {
				t.Error(err)
			}
		default:
			http.NotFound(w, request)
		}
	}))
	defer server.Close()
	directory, configFile := schemaCLIProject(t, server.URL, acceptedSchemaFixture)

	cmd := schemaCmd()
	var output bytes.Buffer
	cmd.SetOut(&output)
	cmd.SetArgs([]string{"migrate", "--config", configFile, "--no-generate"})
	if err := cmd.Execute(); err != nil {
		t.Fatal(err)
	}
	if transitionReads == 0 || schemaReads == 0 {
		t.Fatalf("wait reads: transition=%d schema=%d", transitionReads, schemaReads)
	}
	if !strings.Contains(output.String(), "Waiting for 1 online schema transition") ||
		!strings.Contains(output.String(), "Schema version 2 committed") {
		t.Fatalf("output = %q", output.String())
	}
	accepted, err := projectstate.New(filepath.Join(directory, config.DefaultStateDir)).Load()
	if err != nil {
		t.Fatal(err)
	}
	if accepted.Lock.SchemaVersion != final.SchemaVersion || accepted.Lock.SchemaHash != final.SchemaHash {
		t.Fatalf("accepted state = %+v, want final = %+v", accepted.Lock, final)
	}
}

func TestSchemaPullProtectsAndBacksUpLocalChanges(t *testing.T) {
	serverState := commandSchemaState(t, 2, "accounts")
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		if request.URL.Path != "/schema" {
			http.NotFound(w, request)
			return
		}
		w.Header().Set("Content-Type", "application/json")
		if err := json.NewEncoder(w).Encode(serverState); err != nil {
			t.Fatal(err)
		}
	}))
	defer server.Close()
	directory, configFile := schemaCLIProject(t, server.URL, acceptedSchemaFixture)

	refuse := schemaCmd()
	refuse.SetOut(&bytes.Buffer{})
	refuse.SetArgs([]string{"pull", "--config", configFile, "--no-generate"})
	if err := refuse.Execute(); err == nil || !strings.Contains(err.Error(), "--force") {
		t.Fatalf("pull error = %v", err)
	}

	force := schemaCmd()
	force.SetOut(&bytes.Buffer{})
	force.SetArgs([]string{"pull", "--config", configFile, "--force"})
	if err := force.Execute(); err != nil {
		t.Fatal(err)
	}
	backups, err := filepath.Glob(filepath.Join(directory, config.DefaultStateDir, "backups", "*.rad.schema.yaml"))
	if err != nil || len(backups) != 1 {
		t.Fatalf("backups = %v, %v", backups, err)
	}
	accepted, err := projectstate.New(filepath.Join(directory, config.DefaultStateDir)).Load()
	if err != nil {
		t.Fatal(err)
	}
	if accepted.Lock.SchemaVersion != 2 || accepted.Lock.SchemaHash != serverState.SchemaHash {
		t.Fatalf("accepted state = %+v", accepted.Lock)
	}
	desired, err := os.ReadFile(filepath.Join(directory, config.DefaultSchemaFile))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(desired), "name: accounts") {
		t.Fatalf("pulled desired schema = %s", desired)
	}
	if _, err := os.Stat(filepath.Join(directory, "generated", codegen.GoClientFilename)); err != nil {
		t.Fatalf("generated client: %v", err)
	}
}

func schemaCLIProject(t *testing.T, serverURL, desired string) (string, string) {
	t.Helper()
	parsed, err := url.Parse(serverURL)
	if err != nil {
		t.Fatal(err)
	}
	directory := t.TempDir()
	configFile := filepath.Join(directory, config.DefaultConfigFile)
	if err := os.WriteFile(configFile, []byte("database_url: rad://"+parsed.Host+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(directory, config.DefaultSchemaFile), []byte(desired), 0o600); err != nil {
		t.Fatal(err)
	}
	return directory, configFile
}

func commandSchemaState(t *testing.T, version uint64, table string) protocol.SchemaState {
	t.Helper()
	definition := model.TableDef{
		ID: 1, Name: table,
		Columns:    []model.ColumnDef{{ID: 1, Name: "id", Type: model.TypeText}},
		PrimaryKey: []string{"id"},
	}
	hash, err := model.SchemaFromDefinitions([]model.TableDef{definition}).Hash()
	if err != nil {
		t.Fatal(err)
	}
	return protocol.SchemaState{
		SchemaVersion: version, SchemaHash: hash,
		Schema: protocol.SchemaDocument{Tables: []protocol.TableDef{{
			ID: 1, Name: table,
			Columns:    []protocol.ColumnDef{{ID: 1, Name: "id", Type: "text"}},
			PrimaryKey: []string{"id"},
		}}},
	}
}
