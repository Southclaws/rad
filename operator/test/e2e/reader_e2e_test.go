//go:build e2e

package e2e

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"testing"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	policyv1 "k8s.io/api/policy/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

func (h *harness) readerServiceURL(database string) string {
	return fmt.Sprintf("http://rad-%s-reader.%s.svc", database, h.namespace)
}

// One attempt, and the response is the result whatever its status. The
// retrying [harness.post] fails the test, which a rejection check must not do.
func (h *harness) curlPost(t *testing.T, base, path, body string) (string, error) {
	t.Helper()
	return h.curl(t, "-X", "POST", "-H", "content-type: application/json", "-d", body, base+path)
}

// A single writer and several readers share one bucket. The properties that
// matter in anger are that each Service reaches only its own role, that a
// reader observes what the writer committed, and above all that a reader never
// takes the Slate writer lease: a fenced writer would stop serving.
func TestReadersServeTheSameDataWithoutFencingTheWriter(t *testing.T) {
	h := newHarness(t)
	name := h.name("e2e-readers")
	defer h.bundle(t, name)
	h.makeBuckets(t, name)
	h.makeSecret(t, name)
	database := h.newDatabase(name, name, name)
	database.Spec.Readers = 2
	h.createDatabase(t, database)
	h.waitReady(t, name)
	h.seedMarker(t, name, "reader-marker")

	h.waitReadyReaders(t, name, 2)

	// Each Service must reach only its own role. Rad reports the access it
	// holds, so sampling one Service repeatedly proves the routing rather than
	// the label plumbing.
	h.assertServiceRole(t, h.serviceURL(name), "write")
	h.assertServiceRole(t, h.readerServiceURL(name), "read")

	// A reader opens the same objects, so it must observe the committed row.
	// Propagation is bounded by the reader's manifest poll, not immediate.
	waitFor(t, 2*time.Minute, "readers observe the committed row", func() (bool, string) {
		rows, err := h.curlPost(t, h.readerServiceURL(name), "/execute", queryBody("markers"))
		if err != nil {
			return false, err.Error()
		}
		return strings.Contains(rows, "reader-marker"), rows
	})

	// A reader holds no write access. It must refuse rather than accept and
	// lose the write, or worse contend for the writer's lease.
	rejected, err := h.curlPost(t, h.readerServiceURL(name), "/execute", insertBody("markers", "reader-write", "must not persist"))
	if err == nil && strings.Contains(rejected, `"affected":1`) {
		t.Fatalf("a reader accepted a write: %s", rejected)
	}
	rows := h.queryMarkers(t, name)
	if strings.Contains(rows, "reader-write") {
		t.Fatalf("a reader's write reached storage: %s", rows)
	}

	// The writer must be untouched by all of it. A reader that wrote the
	// fence key would take the lease and stop the writer.
	if restarts := h.podRestarts(t, h.writerPod(name)); restarts != 0 {
		t.Fatalf("writer restarted %d times while readers ran", restarts)
	}
	h.waitReady(t, name)

	// Scaling to zero withdraws the readers and their resources.
	current := &radv1alpha1.Database{}
	if err := h.client.Get(context.Background(),
		types.NamespacedName{Namespace: h.namespace, Name: name}, current); err != nil {
		t.Fatal(err)
	}
	current.Spec.Readers = 0
	if err := h.client.Update(context.Background(), current); err != nil {
		t.Fatal(err)
	}
	readerName := "rad-" + name + "-reader"
	waitFor(t, 2*time.Minute, "reader resources removed", func() (bool, string) {
		remaining := ""
		for _, object := range []client.Object{
			&appsv1.Deployment{ObjectMeta: metav1.ObjectMeta{Name: readerName, Namespace: h.namespace}},
			&corev1.Service{ObjectMeta: metav1.ObjectMeta{Name: readerName, Namespace: h.namespace}},
			&policyv1.PodDisruptionBudget{ObjectMeta: metav1.ObjectMeta{Name: readerName, Namespace: h.namespace}},
		} {
			err := h.client.Get(context.Background(), client.ObjectKeyFromObject(object), object)
			if !apierrors.IsNotFound(err) {
				remaining += fmt.Sprintf("%T ", object)
			}
		}
		return remaining == "", remaining
	})

	// The writer still serves everything it did before readers existed.
	if rows := h.queryMarkers(t, name); !strings.Contains(rows, "reader-marker") {
		t.Fatalf("writer lost data while readers were withdrawn: %s", rows)
	}
}

// Sample a Service until enough responses confirm the expected role. One
// response naming the other role is a routing fault and fails immediately; a
// response naming neither means the instance is not serving /healthz yet, which
// is ordinary while a pod joins.
func (h *harness) assertServiceRole(t *testing.T, url, role string) {
	t.Helper()
	const samples = 6
	other := "read"
	if role == "read" {
		other = "write"
	}
	confirmed := 0
	waitFor(t, 3*time.Minute, fmt.Sprintf("%s answers with %s access", url, role), func() (bool, string) {
		body, err := h.curl(t, url+"/healthz")
		if err != nil {
			return false, fmt.Sprintf("%v (%s)", err, body)
		}
		if strings.Contains(body, `"access":"`+other+`"`) {
			t.Fatalf("%s reached a %s instance: %s", url, other, body)
		}
		if !strings.Contains(body, `"access":"`+role+`"`) {
			return false, body
		}
		confirmed++
		return confirmed >= samples, fmt.Sprintf("%d/%d confirmed", confirmed, samples)
	})
}

// Sum of every family's retained executions, which is how relayed evidence
// shows up at the writer.
func totalRetainedExecutions(statistics string) int {
	var payload struct {
		Models []struct {
			RetainedExecutions int `json:"retainedExecutions"`
		} `json:"models"`
	}
	if err := json.Unmarshal([]byte(statistics), &payload); err != nil {
		return -1
	}
	total := 0
	for _, model := range payload.Models {
		total += model.RetainedExecutions
	}
	return total
}

func (h *harness) waitReadyReaders(t *testing.T, name string, want int32) {
	t.Helper()
	key := types.NamespacedName{Namespace: h.namespace, Name: name}
	waitFor(t, 5*time.Minute, fmt.Sprintf("%d ready readers", want), func() (bool, string) {
		database := &radv1alpha1.Database{}
		if err := h.client.Get(context.Background(), key, database); err != nil {
			return false, err.Error()
		}
		if database.Status.ReaderServiceName != "rad-"+name+"-reader" {
			return false, "reader service name = " + database.Status.ReaderServiceName
		}
		return database.Status.ReadyReaders == want,
			fmt.Sprintf("%d/%d ready", database.Status.ReadyReaders, database.Status.DesiredReaders)
	})
}
