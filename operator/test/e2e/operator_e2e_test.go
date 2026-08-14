//go:build e2e

package e2e

import (
	"context"
	"fmt"
	"strings"
	"testing"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	coordinationv1 "k8s.io/api/coordination/v1"
	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/meta"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

func createTableBody(table string) string {
	return fmt.Sprintf(
		`{"name":%q,"columns":[{"name":"id","type":"text"},{"name":"note","type":"text"}],"primary_key":["id"]}`,
		table)
}

func insertBody(table, id, note string) string {
	return fmt.Sprintf(
		`{"statements":[{"name":"w","kind":"create","table":%q,"relation":{"nodes":{"r":{"kind":"rows","scope":"r","columns":[{"name":"id","type":"text"},{"name":"note","type":"text"}],"rows":[[%q,%q]]}},"root":{"node":"r","cardinality":"many"}}}]}`,
		table, id, note)
}

func queryBody(table string) string {
	return fmt.Sprintf(
		`{"statements":[{"name":"all","kind":"query","relation":{"nodes":{"m":{"kind":"scan","table":%q,"scope":"m"},"o":{"kind":"order","input":"m","terms":[{"expr":{"kind":"col","scope":"m","column":"id"}}]}},"root":{"node":"o","cardinality":"many"}}}]}`,
		table)
}

// post retries transport-level failures: a Service can lag its pod's readiness
// by a moment while kube-proxy programs endpoints, and real clients reconnect.
func (h *harness) post(t *testing.T, base, path, body string) string {
	t.Helper()
	deadline := time.Now().Add(30 * time.Second)
	for {
		output, err := h.curl(t, "-X", "POST", "-H", "content-type: application/json",
			"-d", body, base+path)
		if err == nil {
			return output
		}
		if time.Now().After(deadline) {
			t.Fatalf("POST %s%s: %v\n%s", base, path, err, output)
		}
		time.Sleep(2 * time.Second)
	}
}

func (h *harness) seedMarker(t *testing.T, database, marker string) {
	t.Helper()
	base := h.serviceURL(database)
	created := h.post(t, base, "/tables", createTableBody("markers"))
	if !strings.Contains(created, `"markers"`) {
		t.Fatalf("create table: %s", created)
	}
	inserted := h.post(t, base, "/execute", insertBody("markers", marker, marker+" note"))
	if !strings.Contains(inserted, `"affected":1`) {
		t.Fatalf("insert: %s", inserted)
	}
}

func (h *harness) queryMarkers(t *testing.T, database string) string {
	t.Helper()
	return h.post(t, h.serviceURL(database), "/execute", queryBody("markers"))
}

func TestTwoTenantsServeIsolatedDataAndSurviveRestart(t *testing.T) {
	h := newHarness(t)
	alpha, beta := h.name("e2e-alpha"), h.name("e2e-beta")
	defer h.bundle(t, alpha, beta)
	h.makeBuckets(t, alpha, beta)
	h.makeSecret(t, alpha)
	h.makeSecret(t, beta)
	h.createDatabase(t, h.newDatabase(alpha, alpha, alpha))
	h.createDatabase(t, h.newDatabase(beta, beta, beta))
	h.waitReady(t, alpha)
	h.waitReady(t, beta)

	h.seedMarker(t, alpha, "alpha-marker")
	h.seedMarker(t, beta, "beta-marker")

	// Identical table names, disjoint contents.
	alphaRows := h.queryMarkers(t, alpha)
	betaRows := h.queryMarkers(t, beta)
	if !strings.Contains(alphaRows, "alpha-marker") || strings.Contains(alphaRows, "beta-marker") {
		t.Fatalf("alpha isolation violated: %s", alphaRows)
	}
	if !strings.Contains(betaRows, "beta-marker") || strings.Contains(betaRows, "alpha-marker") {
		t.Fatalf("beta isolation violated: %s", betaRows)
	}

	// The admin surface binds loopback: reachable through port-forward only,
	// never from another pod.
	writer := &corev1.Pod{}
	if err := h.client.Get(context.Background(),
		types.NamespacedName{Namespace: h.namespace, Name: h.writerPod(alpha)}, writer); err != nil {
		t.Fatal(err)
	}
	if output, err := h.curl(t, "-m", "3", "-o", "/dev/null", "-w", "%{http_code}",
		"http://"+writer.Status.PodIP+":7238/"); err == nil && output == "200" {
		t.Fatal("admin surface is reachable from the pod network")
	}

	// Catalog and data must reopen from S3 across a writer restart.
	pod := &corev1.Pod{ObjectMeta: metav1.ObjectMeta{Name: h.writerPod(alpha), Namespace: h.namespace}}
	if err := h.client.Delete(context.Background(), pod); err != nil {
		t.Fatal(err)
	}
	h.waitReady(t, alpha)
	if rows := h.queryMarkers(t, alpha); !strings.Contains(rows, "alpha-marker") {
		t.Fatalf("alpha data lost across restart: %s", rows)
	}
}

func TestDuplicateBucketAndHostnameClaimsAreRejected(t *testing.T) {
	h := newHarness(t)
	first, second, third := h.name("e2e-claim"), h.name("e2e-dupe-bucket"), h.name("e2e-dupe-host")
	defer h.bundle(t, first, second, third)
	h.makeBuckets(t, first)
	h.makeSecret(t, first)
	h.createDatabase(t, h.newDatabase(first, first, first))
	h.waitReady(t, first)

	duplicateBucket := h.newDatabase(second, first, first)
	h.createDatabase(t, duplicateBucket)
	h.waitCondition(t, second, radv1alpha1.ConditionClaimsAccepted,
		metav1.ConditionFalse, "BucketClaimConflict", 2*time.Minute)

	duplicateHost := h.newDatabase(third, h.name("e2e-other-bucket"), first)
	duplicateHost.Spec.Route.Hostname = first + ".rad-e2e.localhost"
	h.createDatabase(t, duplicateHost)
	h.waitCondition(t, third, radv1alpha1.ConditionClaimsAccepted,
		metav1.ConditionFalse, "HostnameClaimConflict", 2*time.Minute)

	// The healthy writer is unaffected by the rejected contenders.
	h.waitReady(t, first)
}

func TestMissingCredentialsHoldReadinessAndRecover(t *testing.T) {
	h := newHarness(t)
	name := h.name("e2e-nocreds")
	defer h.bundle(t, name)
	h.makeBuckets(t, name)
	h.createDatabase(t, h.newDatabase(name, name, name))
	h.waitCondition(t, name, radv1alpha1.ConditionCredentialsReady,
		metav1.ConditionFalse, "", 2*time.Minute)

	// Downstream conditions must not claim knowledge the controller lacks.
	database := &radv1alpha1.Database{}
	if err := h.client.Get(context.Background(),
		types.NamespacedName{Namespace: h.namespace, Name: name}, database); err != nil {
		t.Fatal(err)
	}
	workload := meta.FindStatusCondition(database.Status.Conditions, radv1alpha1.ConditionWorkloadReady)
	if workload != nil && workload.Status == metav1.ConditionTrue {
		t.Fatalf("WorkloadReady = %#v with no credentials", workload)
	}

	h.makeSecret(t, name)
	h.waitReady(t, name)
}

func TestSecretRotationRollsTheWriterInOrder(t *testing.T) {
	h := newHarness(t)
	name := h.name("e2e-rotate")
	defer h.bundle(t, name)
	h.makeBuckets(t, name)
	h.makeSecret(t, name)
	h.createDatabase(t, h.newDatabase(name, name, name))
	h.waitReady(t, name)

	statefulSet := &appsv1.StatefulSet{}
	key := types.NamespacedName{Namespace: h.namespace, Name: "rad-" + name}
	if err := h.client.Get(context.Background(), key, statefulSet); err != nil {
		t.Fatal(err)
	}
	before := statefulSet.Spec.Template.Annotations["radengine.dev/credentials-version"]

	// Rotation means a new Secret resource version. The projected keys must
	// stay valid — RustFS rejects a session token — so the bump comes from an
	// unprojected marker key, exactly like a reissued credential pair that
	// happens to carry extra metadata.
	secret := &corev1.Secret{}
	if err := h.client.Get(context.Background(),
		types.NamespacedName{Namespace: h.namespace, Name: name}, secret); err != nil {
		t.Fatal(err)
	}
	secret.StringData = map[string]string{"rotation-marker": h.suffix}
	if err := h.client.Update(context.Background(), secret); err != nil {
		t.Fatal(err)
	}

	waitFor(t, 4*time.Minute, "credential version rollout", func() (bool, string) {
		if err := h.client.Get(context.Background(), key, statefulSet); err != nil {
			return false, err.Error()
		}
		after := statefulSet.Spec.Template.Annotations["radengine.dev/credentials-version"]
		return after != "" && after != before, "credentials-version unchanged"
	})
	h.waitReady(t, name)
}

func TestWriterDeathUnderLoadLosesNoAcknowledgedWrite(t *testing.T) {
	h := newHarness(t)
	name := h.name("e2e-kill")
	defer h.bundle(t, name)
	h.makeBuckets(t, name)
	h.makeSecret(t, name)
	h.createDatabase(t, h.newDatabase(name, name, name))
	h.waitReady(t, name)
	if created := h.post(t, h.serviceURL(name), "/tables", createTableBody("chaos")); !strings.Contains(created, `"chaos"`) {
		t.Fatalf("create table: %s", created)
	}

	// One in-cluster shell writes sequentially and reports each acknowledged
	// row; the test kills the writer pod partway through.
	script := fmt.Sprintf(`for i in $(seq 1 200); do
body=$(curl -s -m 5 -X POST -H 'content-type: application/json' -d '{"statements":[{"name":"w","kind":"create","table":"chaos","relation":{"nodes":{"r":{"kind":"rows","scope":"r","columns":[{"name":"id","type":"text"},{"name":"note","type":"text"}],"rows":[["row-'"$i"'","chaos"]]}},"root":{"node":"r","cardinality":"many"}}}]}' %s/execute)
case "$body" in *'"affected":1'*) echo "ACK row-$i";; *) echo "MISS row-$i body=$body";; esac
done`, h.serviceURL(name))

	type writes struct {
		output string
		err    error
	}
	done := make(chan writes, 1)
	go func() {
		output, err := h.kubectl(t, "exec", curlPodName, "--", "sh", "-c", script)
		done <- writes{output, err}
	}()
	time.Sleep(5 * time.Second)
	if output, err := h.kubectl(t, "delete", "pod", h.writerPod(name), "--grace-period=0", "--force"); err != nil {
		t.Fatalf("kill writer: %v\n%s", err, output)
	}
	result := <-done
	if result.err != nil {
		t.Fatalf("write loop: %v\n%s", result.err, result.output)
	}
	var acked []string
	for line := range strings.SplitSeq(result.output, "\n") {
		if row, ok := strings.CutPrefix(strings.TrimSpace(line), "ACK "); ok {
			acked = append(acked, row)
		}
	}
	if len(acked) == 0 {
		t.Fatalf("no writes were acknowledged before the kill:\n%s", result.output)
	}

	h.waitReady(t, name)
	rows := h.post(t, h.serviceURL(name), "/execute", queryBody("chaos"))
	for _, row := range acked {
		if !strings.Contains(rows, `"`+row+`"`) {
			t.Errorf("acknowledged write %s lost", row)
		}
	}
	t.Logf("verified %d acknowledged writes across a forced writer kill", len(acked))
}

func TestRogueWriterIsFencedThenRecovers(t *testing.T) {
	h := newHarness(t)
	name := h.name("e2e-fence")
	defer h.bundle(t, name)
	h.makeBuckets(t, name)
	h.makeSecret(t, name)
	h.createDatabase(t, h.newDatabase(name, name, name))
	h.waitReady(t, name)
	h.seedMarker(t, name, "fence-marker")

	// The rogue clones the tenant's own container so image and configuration
	// contend for exactly the same Slate path.
	statefulSet := &appsv1.StatefulSet{}
	if err := h.client.Get(context.Background(),
		types.NamespacedName{Namespace: h.namespace, Name: "rad-" + name}, statefulSet); err != nil {
		t.Fatal(err)
	}
	container := statefulSet.Spec.Template.Spec.Containers[0].DeepCopy()
	container.Name = "rogue"
	container.StartupProbe = nil
	container.ReadinessProbe = nil
	container.LivenessProbe = nil
	rogue := &corev1.Pod{
		ObjectMeta: metav1.ObjectMeta{Name: h.name("rad-e2e-rogue"), Namespace: h.namespace},
		Spec: corev1.PodSpec{
			RestartPolicy: corev1.RestartPolicyNever,
			Containers:    []corev1.Container{*container},
		},
	}
	if err := h.client.Create(context.Background(), rogue); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { h.deleteIgnoreMissing(rogue) })

	// The displaced writer exits and restarts; its replacement fences the
	// rogue back and the rogue stays dead.
	waitFor(t, 4*time.Minute, "displaced writer restart", func() (bool, string) {
		restarts := h.podRestarts(t, h.writerPod(name))
		return restarts > 0, fmt.Sprintf("restarts=%d", restarts)
	})

	// The fencing reason must be readable from the terminated state, not just
	// the log stream: terminationMessagePolicy surfaces the last log line.
	displaced := &corev1.Pod{}
	if err := h.client.Get(context.Background(),
		types.NamespacedName{Namespace: h.namespace, Name: h.writerPod(name)}, displaced); err != nil {
		t.Fatal(err)
	}
	terminated := displaced.Status.ContainerStatuses[0].LastTerminationState.Terminated
	if terminated == nil || !strings.Contains(terminated.Message, "lost ownership") {
		t.Fatalf("termination state = %+v, want the fencing reason surfaced", terminated)
	}
	waitFor(t, 4*time.Minute, "rogue permanently fenced", func() (bool, string) {
		current := &corev1.Pod{}
		if err := h.client.Get(context.Background(),
			types.NamespacedName{Namespace: h.namespace, Name: rogue.Name}, current); err != nil {
			return false, err.Error()
		}
		return current.Status.Phase == corev1.PodFailed, string(current.Status.Phase)
	})
	h.waitReady(t, name)
	if rows := h.queryMarkers(t, name); !strings.Contains(rows, "fence-marker") {
		t.Fatalf("data lost across the fencing duel: %s", rows)
	}
}

func TestRetainedBucketReattachesThroughANewDatabase(t *testing.T) {
	h := newHarness(t)
	first, second := h.name("e2e-retain"), h.name("e2e-reattach")
	bucket := h.name("e2e-retained-bucket")
	defer h.bundle(t, first, second)
	h.makeBuckets(t, bucket)
	h.makeSecret(t, first)
	h.makeSecret(t, second)

	original := h.newDatabase(first, bucket, first)
	h.createDatabase(t, original)
	h.waitReady(t, first)
	h.seedMarker(t, first, "retained-marker")

	// Retain-only deletion: the CR and its claims go away, the objects stay.
	h.deleteIgnoreMissing(original)
	h.waitGone(t, original)

	replacement := h.newDatabase(second, bucket, second)
	h.createDatabase(t, replacement)
	h.waitReady(t, second)
	if rows := h.queryMarkers(t, second); !strings.Contains(rows, "retained-marker") {
		t.Fatalf("retained data not visible through the replacement CR: %s", rows)
	}
}

func TestOperatorLeaderFailoverKeepsReconciling(t *testing.T) {
	h := newHarness(t)
	lease := &coordinationv1.Lease{}
	err := h.client.Get(context.Background(),
		types.NamespacedName{Namespace: h.namespace, Name: "rad-operator.radengine.dev"}, lease)
	if apierrors.IsNotFound(err) {
		t.Skip("leader election is not enabled in this installation")
	}
	if err != nil {
		t.Fatal(err)
	}
	holder := strings.SplitN(*lease.Spec.HolderIdentity, "_", 2)[0]

	name := h.name("e2e-failover")
	defer h.bundle(t, name)
	h.makeBuckets(t, name)
	h.makeSecret(t, name)
	h.createDatabase(t, h.newDatabase(name, name, name))

	leader := &corev1.Pod{ObjectMeta: metav1.ObjectMeta{Name: holder, Namespace: h.namespace}}
	if err := h.client.Delete(context.Background(), leader,
		client.GracePeriodSeconds(0)); err != nil && !apierrors.IsNotFound(err) {
		t.Fatal(err)
	}
	// The replacement leader must finish reconciling the new database.
	h.waitReady(t, name)
}

func TestUnreachableStorageHoldsStartupWithoutRestartLoop(t *testing.T) {
	h := newHarness(t)
	name := h.name("e2e-slowstart")
	defer h.bundle(t, name)
	defer h.scaleRustFS(t, 1)
	h.makeBuckets(t, name)
	h.makeSecret(t, name)
	h.scaleRustFS(t, 0)
	h.createDatabase(t, h.newDatabase(name, name, name))

	pod := &corev1.Pod{}
	key := types.NamespacedName{Namespace: h.namespace, Name: h.writerPod(name)}
	waitFor(t, 3*time.Minute, "writer pod running", func() (bool, string) {
		if err := h.client.Get(context.Background(), key, pod); err != nil {
			return false, err.Error()
		}
		return pod.Status.Phase == corev1.PodRunning && pod.Status.PodIP != "", string(pod.Status.Phase)
	})

	// The probe endpoints answer while nothing else is published, and the
	// preflight names why startup is held.
	base := "http://" + pod.Status.PodIP + ":7237"
	waitFor(t, 2*time.Minute, "startup probe answering", func() (bool, string) {
		output, err := h.curl(t, "-o", "/dev/null", "-w", "%{http_code}", base+"/startupz")
		if err != nil {
			return false, output
		}
		return output == "503", output
	})
	waitFor(t, 2*time.Minute, "startup hold diagnosed", func() (bool, string) {
		body, err := h.curl(t, base+"/startupz")
		if err != nil {
			return false, body
		}
		return strings.Contains(body, "storage_unreachable"), body
	})
	if output, _ := h.curl(t, "-o", "/dev/null", "-w", "%{http_code}", base+"/livez"); output != "200" {
		t.Fatalf("livez = %s while storage is unreachable, want 200", output)
	}
	if output, _ := h.curl(t, "-o", "/dev/null", "-w", "%{http_code}", base+"/healthz"); output != "404" {
		t.Fatalf("healthz = %s before storage opened, want unpublished", output)
	}
	if restarts := h.podRestarts(t, h.writerPod(name)); restarts != 0 {
		t.Fatalf("startup hold restarted the writer %d times", restarts)
	}

	h.scaleRustFS(t, 1)
	h.waitReady(t, name)
}

func TestStorageOutageWithdrawsReadinessWithoutRestart(t *testing.T) {
	h := newHarness(t)
	name := h.name("e2e-outage")
	defer h.bundle(t, name)
	defer h.scaleRustFS(t, 1)
	h.makeBuckets(t, name)
	h.makeSecret(t, name)
	h.createDatabase(t, h.newDatabase(name, name, name))
	h.waitReady(t, name)
	h.seedMarker(t, name, "outage-marker")
	before := h.podRestarts(t, h.writerPod(name))

	h.scaleRustFS(t, 0)
	key := types.NamespacedName{Namespace: h.namespace, Name: name}
	waitFor(t, 3*time.Minute, "readiness withdrawn", func() (bool, string) {
		database := &radv1alpha1.Database{}
		if err := h.client.Get(context.Background(), key, database); err != nil {
			return false, err.Error()
		}
		return database.Status.ReadyReplicas == 0, fmt.Sprintf("readyReplicas=%d", database.Status.ReadyReplicas)
	})

	// Liveness holds: restarting cannot fix an absent object store.
	pod := &corev1.Pod{}
	if err := h.client.Get(context.Background(),
		types.NamespacedName{Namespace: h.namespace, Name: h.writerPod(name)}, pod); err != nil {
		t.Fatal(err)
	}
	base := "http://" + pod.Status.PodIP + ":7237"
	if output, _ := h.curl(t, "-o", "/dev/null", "-w", "%{http_code}", base+"/livez"); output != "200" {
		t.Fatalf("livez = %s during a storage outage, want 200", output)
	}
	if restarts := h.podRestarts(t, h.writerPod(name)); restarts != before {
		t.Fatalf("storage outage restarted the writer: %d -> %d", before, restarts)
	}

	h.scaleRustFS(t, 1)
	h.waitReady(t, name)
	if rows := h.queryMarkers(t, name); !strings.Contains(rows, "outage-marker") {
		t.Fatalf("data lost across the outage: %s", rows)
	}
}
