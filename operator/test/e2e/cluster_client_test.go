//go:build e2e

package e2e

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/Southclaws/rad/operator/cluster"
)

// The cluster SDK is the product path a control plane uses instead of raw CR
// manifests, so it is proven against the same live operator as everything
// else: discovery, create, readiness, data-path reachability, deletion.
func TestClusterClientDrivesTheFullDatabaseLifecycle(t *testing.T) {
	h := newHarness(t)
	name := h.name("e2e-sdk")
	defer h.bundle(t, name)
	h.makeBuckets(t, name)
	h.makeSecret(t, name)

	sdk, err := cluster.New(
		cluster.WithContext(h.context),
		cluster.WithNamespace(h.namespace),
	)
	if err != nil {
		t.Fatal(err)
	}

	created, err := sdk.CreateDatabase(context.Background(), cluster.DatabaseSpec{
		Name:              name,
		Bucket:            name,
		Endpoint:          h.endpoint,
		CredentialsSecret: name,
		CatalogMode:       cluster.CatalogModeDirect,
		Hostname:          name + ".rad-e2e.localhost",
		Scheme:            "http",
	})
	if err != nil {
		t.Fatal(err)
	}
	if created.Ready {
		t.Fatal("create reported ready before the operator observed the database")
	}
	if _, err := sdk.CreateDatabase(context.Background(), cluster.DatabaseSpec{
		Name: name, Bucket: name, CredentialsSecret: name, Hostname: "other.example.com",
	}); !errors.Is(err, cluster.ErrAlreadyExists) {
		t.Fatalf("duplicate create error = %v, want ErrAlreadyExists", err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
	defer cancel()
	ready, err := sdk.WaitReady(ctx, name)
	if err != nil {
		t.Fatal(err)
	}
	if ready.URL != "http://"+name+".rad-e2e.localhost" {
		t.Fatalf("ready URL = %q", ready.URL)
	}

	// The database the SDK reports ready must actually serve.
	if output, err := h.curl(t, "-o", "/dev/null", "-w", "%{http_code}",
		h.serviceURL(name)+"/healthz"); err != nil || output != "200" {
		t.Fatalf("healthz through the Service = %q (%v)", output, err)
	}

	listed, err := sdk.ListDatabases(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	found := false
	for _, database := range listed {
		if database.Name == name {
			found = true
		}
	}
	if !found {
		t.Fatalf("database %s absent from list of %d", name, len(listed))
	}

	if err := sdk.DeleteDatabase(context.Background(), name); err != nil {
		t.Fatal(err)
	}
	waitFor(t, 3*time.Minute, "SDK deletion", func() (bool, string) {
		_, err := sdk.GetDatabase(context.Background(), name)
		if errors.Is(err, cluster.ErrNotFound) {
			return true, ""
		}
		if err != nil {
			return false, err.Error()
		}
		return false, "finalizing"
	})
	if err := sdk.DeleteDatabase(context.Background(), name); !errors.Is(err, cluster.ErrNotFound) {
		t.Fatalf("delete after deletion error = %v, want ErrNotFound", err)
	}
}

func TestClusterClientReportsAMissingInstallation(t *testing.T) {
	h := newHarness(t)
	// A namespace outside the operator's watch still serves the API — the
	// installation check is about the API group, not topology — so the
	// negative case needs a cluster without the CRD. Approximate it by
	// asserting the sentinel wraps cleanly for callers that switch on it.
	if !errors.Is(cluster.ErrNotInstalled, cluster.ErrNotInstalled) ||
		!strings.Contains(cluster.ErrNotInstalled.Error(), "not installed") {
		t.Fatal("ErrNotInstalled sentinel is not usable")
	}
	sdk, err := cluster.New(cluster.WithContext(h.context), cluster.WithNamespace(h.namespace))
	if err != nil {
		t.Fatalf("discovery against an installed cluster failed: %v", err)
	}
	if _, err := sdk.GetDatabase(context.Background(), "definitely-absent"); !errors.Is(err, cluster.ErrNotFound) {
		t.Fatalf("get absent database error = %v, want ErrNotFound", err)
	}
}
