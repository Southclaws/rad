package concurrent

import (
	"context"
	"fmt"
	"log/slog"
	"net"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"

	radclient "github.com/Southclaws/rad/rad/client"
	"github.com/Southclaws/rad/rad/engine/01_kv/kvslate"
	catalog "github.com/Southclaws/rad/rad/engine/02_catalog"
	"github.com/Southclaws/rad/rad/engine/02_catalog/model"
	exec "github.com/Southclaws/rad/rad/engine/05_exec"
	frontend "github.com/Southclaws/rad/rad/engine/06_frontend"
	radserver "github.com/Southclaws/rad/rad/server"
	pgserver "github.com/Southclaws/rad/rad/server/pgwire"
)

var databaseSequence atomic.Uint64

type chaosDB struct {
	URL       string
	Postgres  string
	Control   *radclient.Client
	Catalog   *catalog.Catalog
	Auditor   *exec.Engine
	Harness   *exec.Engine
	http      *httptest.Server
	pg        *pgserver.Server
	pgDone    chan error
	store     *kvslate.Store
	db        *frontend.DB
	pgClients []*pgx.Conn
}

func newChaosDB(t *testing.T, options ...exec.Option) *chaosDB {
	t.Helper()
	ctx := context.Background()
	store, err := kvslate.Open(fmt.Sprintf("concurrent-%d", databaseSequence.Add(1)), "memory:///")
	if err != nil {
		t.Fatalf("open SlateDB: %v", err)
	}
	cat := catalog.New(store)
	mode, err := cat.InitMode(ctx, model.ModeDirect)
	if err != nil {
		_ = store.Close()
		t.Fatalf("initialise catalog mode: %v", err)
	}
	db := frontend.OpenWithOptions(store, options...)
	handler, err := radserver.New(db, cat)
	if err != nil {
		_ = db.Close()
		_ = store.Close()
		t.Fatalf("build HTTP server: %v", err)
	}
	httpServer := httptest.NewServer(handler)
	url := "rad://" + strings.TrimPrefix(httpServer.URL, "http://")
	control, err := radclient.Dial(url, radclient.WithTimeout(10*time.Second))
	if err != nil {
		httpServer.Close()
		_ = db.Close()
		_ = store.Close()
		t.Fatalf("dial HTTP server: %v", err)
	}

	postgres, err := pgserver.New(db, cat, mode, slog.New(slog.DiscardHandler))
	if err != nil {
		httpServer.Close()
		_ = db.Close()
		_ = store.Close()
		t.Fatalf("build PostgreSQL server: %v", err)
	}
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		httpServer.Close()
		_ = db.Close()
		_ = store.Close()
		t.Fatalf("listen for PostgreSQL: %v", err)
	}
	done := make(chan error, 1)
	go func() { done <- postgres.Serve(listener) }()

	h := &chaosDB{
		URL:      url,
		Postgres: listener.Addr().String(),
		Control:  control,
		Catalog:  cat,
		Auditor:  exec.New(store, catalog.New(store), exec.WithSchemaJobScheduling(false)),
		http:     httpServer,
		pg:       postgres,
		pgDone:   done,
		store:    store,
		db:       db,
	}
	harnessOptions := append([]exec.Option(nil), options...)
	harnessOptions = append(harnessOptions, exec.WithSchemaJobScheduling(false))
	h.Harness = exec.New(store, catalog.New(store), harnessOptions...)
	t.Cleanup(func() { h.close(t) })
	return h
}

func (d *chaosDB) httpClient(t *testing.T) *radclient.Client {
	t.Helper()
	client, err := radclient.Dial(d.URL, radclient.WithTimeout(10*time.Second))
	if err != nil {
		t.Fatalf("dial Rad: %v", err)
	}
	return client
}

func (d *chaosDB) postgresClient(t *testing.T) *pgx.Conn {
	t.Helper()
	conn, err := pgx.Connect(context.Background(), fmt.Sprintf(
		"postgres://chaos:chaos@%s/rad?sslmode=disable", d.Postgres,
	))
	if err != nil {
		t.Fatalf("dial PostgreSQL: %v", err)
	}
	d.pgClients = append(d.pgClients, conn)
	return conn
}

func waitSchemaTransitionReady(
	ctx context.Context,
	client *radclient.Client,
	transitionID string,
) (radclient.TransitionControl, error) {
	ticker := time.NewTicker(10 * time.Millisecond)
	defer ticker.Stop()
	for {
		transition, err := client.SchemaTransition(ctx, transitionID)
		if err != nil {
			return radclient.TransitionControl{}, err
		}
		switch transition.State {
		case radclient.TransitionReady:
			return transition, nil
		case radclient.TransitionFailed, radclient.TransitionCancelled:
			return radclient.TransitionControl{}, fmt.Errorf(
				"schema transition %q ended in state %q: %s",
				transitionID,
				transition.State,
				transition.LastError,
			)
		}
		select {
		case <-ctx.Done():
			return radclient.TransitionControl{}, ctx.Err()
		case <-ticker.C:
		}
	}
}

func (d *chaosDB) close(t *testing.T) {
	t.Helper()
	for _, conn := range d.pgClients {
		if !conn.IsClosed() {
			_ = conn.Close(context.Background())
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := d.pg.Shutdown(ctx); err != nil {
		t.Errorf("shutdown PostgreSQL server: %v", err)
	}
	select {
	case err := <-d.pgDone:
		if err != nil {
			t.Errorf("serve PostgreSQL: %v", err)
		}
	case <-ctx.Done():
		t.Error("PostgreSQL server did not stop")
	}
	d.http.Close()
	if err := d.Auditor.Close(); err != nil {
		t.Errorf("close auditor engine: %v", err)
	}
	if err := d.Harness.Close(); err != nil {
		t.Errorf("close deterministic harness engine: %v", err)
	}
	if err := d.db.Close(); err != nil {
		t.Errorf("close database engine: %v", err)
	}
	if err := d.store.Close(); err != nil {
		t.Errorf("close SlateDB: %v", err)
	}
}
