//go:build e2e

package e2e

import (
	"context"
	"fmt"
	"math/rand"
	"os"
	"strings"
	"sync"
	"testing"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
	"github.com/Southclaws/rad/operator/cluster"
)

// The soak drives a small fleet through sustained writes and injected faults.
// Pod-level chaos is randomized directly; network chaos uses Chaos Mesh when
// its CRDs are installed (`task chaos:install`) and is skipped otherwise. The
// one invariant that must always hold: an acknowledged write is never lost.

const soakTenants = 8

type soakWriter struct {
	tenant string
	acked  []string
	misses int
	mutex  sync.Mutex
}

func TestSoakFleetUnderChaosLosesNoAcknowledgedWrite(t *testing.T) {
	if os.Getenv("RAD_SOAK") == "" {
		t.Skip("set RAD_SOAK=1 (task operator:test:soak) to run the chaos soak")
	}
	h := newHarness(t)
	sdk, err := cluster.New(
		cluster.WithContext(h.context),
		cluster.WithNamespace(h.namespace),
	)
	if err != nil {
		t.Fatal(err)
	}

	tenants := make([]string, 0, soakTenants)
	for index := range soakTenants {
		tenants = append(tenants, h.name(fmt.Sprintf("soak-%d", index)))
	}
	defer h.bundle(t, tenants...)
	h.makeBuckets(t, tenants...)

	t.Log("provisioning the fleet through the cluster SDK")
	for _, tenant := range tenants {
		h.makeSecret(t, tenant)
		if _, err := sdk.CreateDatabase(context.Background(), cluster.DatabaseSpec{
			Name:              tenant,
			Bucket:            tenant,
			Endpoint:          h.endpoint,
			CredentialsSecret: tenant,
			CatalogMode:       cluster.CatalogModeDirect,
			Hostname:          tenant + ".rad-e2e.localhost",
			Scheme:            "http",
		}); err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() {
			_ = sdk.DeleteDatabase(context.Background(), tenant)
			h.waitGone(t, &radv1alpha1.Database{
				ObjectMeta: metav1.ObjectMeta{Namespace: h.namespace, Name: tenant},
			})
		})
	}
	ready, cancelReady := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancelReady()
	for _, tenant := range tenants {
		if _, err := sdk.WaitReady(ready, tenant); err != nil {
			t.Fatal(err)
		}
	}
	for _, tenant := range tenants {
		if created := h.post(t, h.serviceURL(tenant), "/tables", createTableBody("soak")); !strings.Contains(created, `"soak"`) {
			t.Fatalf("%s create table: %s", tenant, created)
		}
	}

	writers := make([]*soakWriter, len(tenants))
	stop := make(chan struct{})
	var writing sync.WaitGroup
	for index, tenant := range tenants {
		writers[index] = &soakWriter{tenant: tenant}
		writing.Add(1)
		go writers[index].run(t, h, &writing, stop)
	}

	t.Log("phase: random writer kills")
	for range 6 {
		tenant := tenants[rand.Intn(len(tenants))]
		if output, err := h.kubectl(t, "delete", "pod", h.writerPod(tenant),
			"--grace-period=0", "--force", "--ignore-not-found"); err != nil {
			t.Logf("kill %s: %v %s", tenant, err, output)
		}
		time.Sleep(20 * time.Second)
	}

	if h.chaosMeshInstalled(t) {
		t.Log("phase: network degradation between the fleet and its object store")
		h.applyNetworkChaos(t)
		time.Sleep(2 * time.Minute)
		h.deleteNetworkChaos(t)
	} else {
		t.Log("chaos-mesh is not installed; skipping the network-degradation phase")
	}

	t.Log("phase: object store restart")
	if output, err := h.kubectl(t, "delete", "pod", "-l", "app="+rustfsName,
		"--grace-period=0", "--force", "--ignore-not-found"); err != nil {
		t.Fatalf("kill object store: %v %s", err, output)
	}
	time.Sleep(45 * time.Second)

	t.Log("phase: operator restart")
	if output, err := h.kubectl(t, "delete", "pod",
		"-l", "app.kubernetes.io/name=rad-operator", "--ignore-not-found"); err != nil {
		t.Fatalf("kill operator: %v %s", err, output)
	}
	time.Sleep(30 * time.Second)

	t.Log("phase: staged upgrade drill")
	h.upgradeDrill(t, sdk, tenants)

	close(stop)
	writing.Wait()

	t.Log("phase: verification")
	deadline, cancelVerify := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancelVerify()
	totalAcked, totalMisses := 0, 0
	for index, tenant := range tenants {
		if _, err := sdk.WaitReady(deadline, tenant); err != nil {
			t.Errorf("%s never returned to Ready: %v", tenant, err)
			continue
		}
		rows := h.post(t, h.serviceURL(tenant), "/execute", queryBody("soak"))
		writer := writers[index]
		writer.mutex.Lock()
		for _, id := range writer.acked {
			if !strings.Contains(rows, `"`+id+`"`) {
				t.Errorf("%s lost acknowledged write %s", tenant, id)
			}
		}
		// Isolation: rows carry their tenant's name, so any foreign name is a
		// cross-tenant leak.
		for _, other := range tenants {
			if other != tenant && strings.Contains(rows, other) {
				t.Errorf("%s returned rows from %s", tenant, other)
			}
		}
		totalAcked += len(writer.acked)
		totalMisses += writer.misses
		writer.mutex.Unlock()
	}
	t.Logf("soak complete: %d tenants, %d acknowledged writes verified, %d failed writes during faults",
		len(tenants), totalAcked, totalMisses)
	if totalAcked < soakTenants*10 {
		t.Fatalf("only %d acknowledged writes across the fleet; the workload never ran properly", totalAcked)
	}
}

func (w *soakWriter) run(t *testing.T, h *harness, writing *sync.WaitGroup, stop <-chan struct{}) {
	defer writing.Done()
	sequence := 0
	for {
		select {
		case <-stop:
			return
		case <-time.After(time.Second):
		}
		sequence++
		id := fmt.Sprintf("%s-row-%06d", w.tenant, sequence)
		body, err := h.curl(t, "-X", "POST", "-H", "content-type: application/json",
			"-d", insertBody("soak", id, w.tenant), h.serviceURL(w.tenant)+"/execute")
		w.mutex.Lock()
		if err == nil && strings.Contains(body, `"affected":1`) {
			w.acked = append(w.acked, id)
		} else {
			w.misses++
		}
		w.mutex.Unlock()
	}
}

// upgradeDrill changes the fleet image to a second tag, waits for every
// tenant to roll onto it and return to Ready, then restores the original so
// the cluster matches its manifests. Both directions are ordered single-writer
// rollouts under load.
func (h *harness) upgradeDrill(t *testing.T, sdk *cluster.Client, tenants []string) {
	t.Helper()
	original := h.fleetImage(t)
	upgraded := original + "-v2"
	if output, err := runHost(t, "docker", "tag", original, upgraded); err != nil {
		t.Fatalf("tag upgrade image: %v %s", err, output)
	}

	h.setFleetImage(t, upgraded)
	h.waitFleetOnImage(t, sdk, tenants, upgraded)
	t.Logf("fleet rolled to %s; restoring %s", upgraded, original)
	h.setFleetImage(t, original)
	h.waitFleetOnImage(t, sdk, tenants, original)
}

func (h *harness) fleetImage(t *testing.T) string {
	t.Helper()
	deployment := &appsv1.Deployment{}
	if err := h.client.Get(context.Background(),
		types.NamespacedName{Namespace: h.namespace, Name: "rad-operator"}, deployment); err != nil {
		t.Fatal(err)
	}
	for _, argument := range deployment.Spec.Template.Spec.Containers[0].Args {
		if image, ok := strings.CutPrefix(argument, "--rad-image="); ok {
			return image
		}
	}
	t.Fatal("the operator deployment carries no --rad-image argument")
	return ""
}

func (h *harness) setFleetImage(t *testing.T, image string) {
	t.Helper()
	deployment := &appsv1.Deployment{}
	key := types.NamespacedName{Namespace: h.namespace, Name: "rad-operator"}
	waitFor(t, time.Minute, "patch operator image argument", func() (bool, string) {
		if err := h.client.Get(context.Background(), key, deployment); err != nil {
			return false, err.Error()
		}
		arguments := deployment.Spec.Template.Spec.Containers[0].Args
		for index, argument := range arguments {
			if strings.HasPrefix(argument, "--rad-image=") {
				arguments[index] = "--rad-image=" + image
			}
		}
		if err := h.client.Update(context.Background(), deployment); err != nil {
			return false, err.Error()
		}
		return true, ""
	})
	if output, err := h.kubectl(t, "rollout", "status", "deployment/rad-operator", "--timeout=180s"); err != nil {
		t.Fatalf("operator rollout: %v %s", err, output)
	}
}

func (h *harness) waitFleetOnImage(t *testing.T, sdk *cluster.Client, tenants []string, image string) {
	t.Helper()
	for _, tenant := range tenants {
		waitFor(t, 5*time.Minute, tenant+" on "+image, func() (bool, string) {
			database, err := sdk.GetDatabase(context.Background(), tenant)
			if err != nil {
				return false, err.Error()
			}
			if database.ObservedImage != image {
				return false, "observed " + database.ObservedImage
			}
			return database.Ready, "rolled but not ready"
		})
	}
}

func (h *harness) chaosMeshInstalled(t *testing.T) bool {
	t.Helper()
	output, err := h.kubectl(t, "api-resources", "--api-group=chaos-mesh.org", "-o", "name")
	return err == nil && strings.Contains(output, "networkchaos")
}

const networkChaosManifest = `apiVersion: chaos-mesh.org/v1alpha1
kind: NetworkChaos
metadata:
  name: rad-soak-storage-delay
  namespace: %[1]s
spec:
  action: delay
  mode: all
  selector:
    namespaces: [%[1]s]
    labelSelectors:
      app: %[2]s
  delay:
    latency: 200ms
    jitter: 100ms
---
apiVersion: chaos-mesh.org/v1alpha1
kind: NetworkChaos
metadata:
  name: rad-soak-storage-loss
  namespace: %[1]s
spec:
  action: loss
  mode: all
  selector:
    namespaces: [%[1]s]
    labelSelectors:
      app: %[2]s
  loss:
    loss: "20"
`

func (h *harness) applyNetworkChaos(t *testing.T) {
	t.Helper()
	manifest := fmt.Sprintf(networkChaosManifest, h.namespace, rustfsName)
	if output, err := h.kubectlStdin(t, manifest, "apply", "-f", "-"); err != nil {
		t.Fatalf("apply network chaos: %v %s", err, output)
	}
}

func (h *harness) deleteNetworkChaos(t *testing.T) {
	t.Helper()
	if output, err := h.kubectl(t, "delete", "networkchaos",
		"rad-soak-storage-delay", "rad-soak-storage-loss", "--ignore-not-found"); err != nil {
		t.Fatalf("delete network chaos: %v %s", err, output)
	}
}
