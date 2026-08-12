//go:build e2e

package e2e

import (
	"context"
	"fmt"
	"math/rand"
	"os"
	"os/exec"
	"strings"
	"testing"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/meta"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	"k8s.io/client-go/tools/clientcmd"
	"k8s.io/utils/ptr"
	"sigs.k8s.io/controller-runtime/pkg/client"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

// The suite drives one operator installation from the outside: real API
// server, real RustFS, real Rad pods. It creates its own RustFS instance so
// storage-fault tests never disturb other tenants in the namespace.

const (
	rustfsName  = "rustfs-e2e"
	rustfsImage = "rustfs/rustfs:1.0.0-beta.11"
	rustfsCreds = "rustfsadmin"
	curlPodName = "rad-e2e-curl"
	curlImage   = "curlimages/curl:8.10.1"
	mcImage     = "minio/mc:latest"
)

type harness struct {
	client    client.Client
	namespace string
	context   string
	endpoint  string
	suffix    string
}

func newHarness(t *testing.T) *harness {
	t.Helper()
	if os.Getenv("RAD_E2E") == "" {
		t.Skip("set RAD_E2E=1 to run the outside-in operator suite against the current kubecontext")
	}
	scheme := runtime.NewScheme()
	if err := clientgoscheme.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	if err := radv1alpha1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	rules := clientcmd.NewDefaultClientConfigLoadingRules()
	overrides := &clientcmd.ConfigOverrides{CurrentContext: os.Getenv("E2E_KUBE_CONTEXT")}
	config, err := clientcmd.NewNonInteractiveDeferredLoadingClientConfig(rules, overrides).ClientConfig()
	if err != nil {
		t.Fatal(err)
	}
	kube, err := client.New(config, client.Options{Scheme: scheme})
	if err != nil {
		t.Fatal(err)
	}
	namespace := os.Getenv("E2E_NAMESPACE")
	if namespace == "" {
		namespace = "default"
	}
	h := &harness{
		client:    kube,
		namespace: namespace,
		context:   os.Getenv("E2E_KUBE_CONTEXT"),
		suffix:    fmt.Sprintf("%06d", rand.Intn(1_000_000)),
	}
	h.endpoint = fmt.Sprintf("http://%s.%s.svc:9000", rustfsName, namespace)
	h.ensureRustFS(t)
	h.ensureCurlPod(t)
	return h
}

func (h *harness) name(base string) string {
	return base + "-" + h.suffix
}

func (h *harness) kubectl(t *testing.T, args ...string) (string, error) {
	t.Helper()
	full := []string{"-n", h.namespace}
	if h.context != "" {
		full = append([]string{"--context", h.context}, full...)
	}
	full = append(full, args...)
	output, err := exec.Command("kubectl", full...).CombinedOutput()
	return string(output), err
}

// kubectlStdin pipes a manifest to kubectl, for resources with no Go types in
// the test's scheme such as Chaos Mesh experiments.
func (h *harness) kubectlStdin(t *testing.T, stdin string, args ...string) (string, error) {
	t.Helper()
	full := []string{"-n", h.namespace}
	if h.context != "" {
		full = append([]string{"--context", h.context}, full...)
	}
	full = append(full, args...)
	command := exec.Command("kubectl", full...)
	command.Stdin = strings.NewReader(stdin)
	output, err := command.CombinedOutput()
	return string(output), err
}

func runHost(t *testing.T, name string, args ...string) (string, error) {
	t.Helper()
	output, err := exec.Command(name, args...).CombinedOutput()
	return string(output), err
}

// curl runs inside the cluster so requests traverse the tenant Service exactly
// as workload traffic would.
func (h *harness) curl(t *testing.T, args ...string) (string, error) {
	t.Helper()
	full := append([]string{"exec", curlPodName, "--", "curl", "-s", "-m", "15"}, args...)
	return h.kubectl(t, full...)
}

func (h *harness) serviceURL(database string) string {
	return fmt.Sprintf("http://rad-%s.%s.svc", database, h.namespace)
}

func (h *harness) ensureRustFS(t *testing.T) {
	t.Helper()
	labels := map[string]string{"app": rustfsName}
	service := &corev1.Service{
		ObjectMeta: metav1.ObjectMeta{Name: rustfsName, Namespace: h.namespace},
		Spec: corev1.ServiceSpec{
			Selector: labels,
			Ports:    []corev1.ServicePort{{Name: "s3", Port: 9000}},
		},
	}
	if err := h.client.Create(context.Background(), service); err != nil && !apierrors.IsAlreadyExists(err) {
		t.Fatal(err)
	}
	// Durable storage matters: the outage tests scale this deployment to zero,
	// and a recovered object store must still hold its buckets — losing them
	// turns a transient outage into a destroyed database.
	claim := &corev1.PersistentVolumeClaim{
		ObjectMeta: metav1.ObjectMeta{Name: rustfsName, Namespace: h.namespace},
		Spec: corev1.PersistentVolumeClaimSpec{
			AccessModes: []corev1.PersistentVolumeAccessMode{corev1.ReadWriteOnce},
			Resources: corev1.VolumeResourceRequirements{
				Requests: corev1.ResourceList{corev1.ResourceStorage: resource.MustParse("4Gi")},
			},
		},
	}
	if err := h.client.Create(context.Background(), claim); err != nil && !apierrors.IsAlreadyExists(err) {
		t.Fatal(err)
	}
	deployment := &appsv1.Deployment{
		ObjectMeta: metav1.ObjectMeta{Name: rustfsName, Namespace: h.namespace},
		Spec: appsv1.DeploymentSpec{
			Replicas: ptr.To[int32](1),
			Strategy: appsv1.DeploymentStrategy{Type: appsv1.RecreateDeploymentStrategyType},
			Selector: &metav1.LabelSelector{MatchLabels: labels},
			Template: corev1.PodTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{Labels: labels},
				Spec: corev1.PodSpec{
					Containers: []corev1.Container{{
						Name:  "rustfs",
						Image: rustfsImage,
						Args:  []string{"--address", "0.0.0.0:9000", "/data"},
						Env: []corev1.EnvVar{
							{Name: "RUSTFS_ACCESS_KEY", Value: rustfsCreds},
							{Name: "RUSTFS_SECRET_KEY", Value: rustfsCreds},
						},
						Ports: []corev1.ContainerPort{{ContainerPort: 9000}},
						VolumeMounts: []corev1.VolumeMount{{
							Name:      "data",
							MountPath: "/data",
						}},
					}},
					Volumes: []corev1.Volume{{
						Name: "data",
						VolumeSource: corev1.VolumeSource{
							PersistentVolumeClaim: &corev1.PersistentVolumeClaimVolumeSource{
								ClaimName: rustfsName,
							},
						},
					}},
				},
			},
		},
	}
	if err := h.client.Create(context.Background(), deployment); err != nil && !apierrors.IsAlreadyExists(err) {
		t.Fatal(err)
	}
	h.scaleRustFS(t, 1)
}

func (h *harness) scaleRustFS(t *testing.T, replicas int32) {
	t.Helper()
	deployment := &appsv1.Deployment{}
	key := types.NamespacedName{Namespace: h.namespace, Name: rustfsName}
	waitFor(t, time.Minute, "scale "+rustfsName, func() (bool, string) {
		if err := h.client.Get(context.Background(), key, deployment); err != nil {
			return false, err.Error()
		}
		deployment.Spec.Replicas = ptr.To(replicas)
		if err := h.client.Update(context.Background(), deployment); err != nil {
			return false, err.Error()
		}
		return true, ""
	})
	waitFor(t, 3*time.Minute, fmt.Sprintf("%s replicas=%d", rustfsName, replicas), func() (bool, string) {
		if err := h.client.Get(context.Background(), key, deployment); err != nil {
			return false, err.Error()
		}
		if replicas == 0 {
			pods := &corev1.PodList{}
			if err := h.client.List(context.Background(), pods,
				client.InNamespace(h.namespace), client.MatchingLabels{"app": rustfsName}); err != nil {
				return false, err.Error()
			}
			return len(pods.Items) == 0, fmt.Sprintf("%d pods remain", len(pods.Items))
		}
		return deployment.Status.ReadyReplicas == replicas,
			fmt.Sprintf("ready=%d", deployment.Status.ReadyReplicas)
	})
}

func (h *harness) ensureCurlPod(t *testing.T) {
	t.Helper()
	pod := &corev1.Pod{
		ObjectMeta: metav1.ObjectMeta{Name: curlPodName, Namespace: h.namespace},
		Spec: corev1.PodSpec{
			Containers: []corev1.Container{{
				Name:    "curl",
				Image:   curlImage,
				Command: []string{"sleep", "infinity"},
			}},
		},
	}
	if err := h.client.Create(context.Background(), pod); err != nil && !apierrors.IsAlreadyExists(err) {
		t.Fatal(err)
	}
	waitFor(t, 2*time.Minute, "curl pod ready", func() (bool, string) {
		current := &corev1.Pod{}
		if err := h.client.Get(context.Background(),
			types.NamespacedName{Namespace: h.namespace, Name: curlPodName}, current); err != nil {
			return false, err.Error()
		}
		for _, condition := range current.Status.Conditions {
			if condition.Type == corev1.PodReady && condition.Status == corev1.ConditionTrue {
				return true, ""
			}
		}
		return false, string(current.Status.Phase)
	})
}

// makeBuckets creates buckets on the suite's RustFS through a Job so the suite
// needs no S3 client of its own.
func (h *harness) makeBuckets(t *testing.T, buckets ...string) {
	t.Helper()
	script := "set -eu\nuntil mc alias set e2e http://" + rustfsName + ":9000 " +
		rustfsCreds + " " + rustfsCreds + "; do sleep 2; done\n"
	for _, bucket := range buckets {
		script += "mc mb --ignore-existing e2e/" + bucket + "\n"
	}
	name := h.name("rad-e2e-buckets")
	job := &batchv1.Job{
		ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: h.namespace},
		Spec: batchv1.JobSpec{
			BackoffLimit:            ptr.To[int32](6),
			TTLSecondsAfterFinished: ptr.To[int32](300),
			Template: corev1.PodTemplateSpec{
				Spec: corev1.PodSpec{
					RestartPolicy: corev1.RestartPolicyOnFailure,
					Containers: []corev1.Container{{
						Name:    "mc",
						Image:   mcImage,
						Command: []string{"sh", "-c", script},
					}},
				},
			},
		},
	}
	if err := h.client.Create(context.Background(), job); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { h.deleteIgnoreMissing(job) })
	waitFor(t, 3*time.Minute, "bucket job "+name, func() (bool, string) {
		current := &batchv1.Job{}
		if err := h.client.Get(context.Background(),
			types.NamespacedName{Namespace: h.namespace, Name: name}, current); err != nil {
			return false, err.Error()
		}
		return current.Status.Succeeded > 0, fmt.Sprintf("succeeded=%d", current.Status.Succeeded)
	})
}

func (h *harness) makeSecret(t *testing.T, name string) {
	t.Helper()
	secret := &corev1.Secret{
		ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: h.namespace},
		StringData: map[string]string{
			"AWS_ACCESS_KEY_ID":     rustfsCreds,
			"AWS_SECRET_ACCESS_KEY": rustfsCreds,
		},
	}
	if err := h.client.Create(context.Background(), secret); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { h.deleteIgnoreMissing(secret) })
}

func (h *harness) newDatabase(name, bucket, secret string) *radv1alpha1.Database {
	return &radv1alpha1.Database{
		ObjectMeta: metav1.ObjectMeta{Name: name, Namespace: h.namespace},
		Spec: radv1alpha1.DatabaseSpec{
			Storage: radv1alpha1.S3Storage{
				Bucket:   bucket,
				Prefix:   "rad",
				Region:   "us-east-1",
				Endpoint: h.endpoint,
				Authentication: radv1alpha1.S3Authentication{
					CredentialsSecretRef: &corev1.LocalObjectReference{Name: secret},
				},
			},
			CatalogMode: radv1alpha1.CatalogModeDirect,
			Route: radv1alpha1.Route{
				Hostname: name + ".rad-e2e.localhost",
				Scheme:   "http",
			},
			DeletionPolicy: radv1alpha1.DeletionPolicyRetain,
		},
	}
}

func (h *harness) createDatabase(t *testing.T, database *radv1alpha1.Database) {
	t.Helper()
	if err := h.client.Create(context.Background(), database); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		h.deleteIgnoreMissing(database)
		h.waitGone(t, database)
	})
}

func (h *harness) deleteIgnoreMissing(object client.Object) {
	_ = h.client.Delete(context.Background(), object)
}

func (h *harness) waitGone(t *testing.T, database *radv1alpha1.Database) {
	t.Helper()
	key := client.ObjectKeyFromObject(database)
	waitFor(t, 3*time.Minute, "deletion of "+database.Name, func() (bool, string) {
		current := &radv1alpha1.Database{}
		err := h.client.Get(context.Background(), key, current)
		if apierrors.IsNotFound(err) {
			return true, ""
		}
		if err != nil {
			return false, err.Error()
		}
		return false, "finalizing"
	})
}

func (h *harness) waitReady(t *testing.T, name string) {
	t.Helper()
	h.waitCondition(t, name, radv1alpha1.ConditionReady, metav1.ConditionTrue, "", 5*time.Minute)
}

func (h *harness) waitCondition(
	t *testing.T,
	name string,
	conditionType string,
	status metav1.ConditionStatus,
	reason string,
	timeout time.Duration,
) {
	t.Helper()
	key := types.NamespacedName{Namespace: h.namespace, Name: name}
	waitFor(t, timeout, fmt.Sprintf("%s %s=%s/%s", name, conditionType, status, reason), func() (bool, string) {
		database := &radv1alpha1.Database{}
		if err := h.client.Get(context.Background(), key, database); err != nil {
			return false, err.Error()
		}
		condition := meta.FindStatusCondition(database.Status.Conditions, conditionType)
		if condition == nil {
			return false, "condition absent"
		}
		if condition.Status != status {
			return false, fmt.Sprintf("%s/%s: %s", condition.Status, condition.Reason, condition.Message)
		}
		if reason != "" && condition.Reason != reason {
			return false, fmt.Sprintf("%s/%s", condition.Status, condition.Reason)
		}
		return true, ""
	})
}

func (h *harness) writerPod(database string) string {
	return "rad-" + database + "-0"
}

func (h *harness) podRestarts(t *testing.T, name string) int32 {
	t.Helper()
	pod := &corev1.Pod{}
	if err := h.client.Get(context.Background(),
		types.NamespacedName{Namespace: h.namespace, Name: name}, pod); err != nil {
		return -1
	}
	if len(pod.Status.ContainerStatuses) == 0 {
		return -1
	}
	return pod.Status.ContainerStatuses[0].RestartCount
}

// bundle dumps everything a failure investigation needs.
func (h *harness) bundle(t *testing.T, databases ...string) {
	t.Helper()
	if !t.Failed() {
		return
	}
	for _, name := range databases {
		if output, err := h.kubectl(t, "get", "database", name, "-o", "yaml"); err == nil {
			t.Logf("=== Database %s ===\n%s", name, output)
		}
		if output, err := h.kubectl(t, "logs", h.writerPod(name), "--tail", "40"); err == nil {
			t.Logf("=== Rad logs %s ===\n%s", name, output)
		}
	}
	if output, err := h.kubectl(t, "get", "events", "--sort-by=.lastTimestamp"); err == nil {
		lines := strings.Split(strings.TrimSpace(output), "\n")
		if len(lines) > 40 {
			lines = lines[len(lines)-40:]
		}
		t.Logf("=== Events ===\n%s", strings.Join(lines, "\n"))
	}
	if output, err := h.kubectl(t, "logs", "deployment/rad-operator", "--tail", "40"); err == nil {
		t.Logf("=== Operator logs ===\n%s", output)
	}
	if output, err := h.kubectl(t, "get", "pods", "-o", "wide"); err == nil {
		t.Logf("=== Pods ===\n%s", output)
	}
}

func waitFor(t *testing.T, timeout time.Duration, what string, poll func() (bool, string)) {
	t.Helper()
	deadline := time.Now().Add(timeout)
	var last string
	for {
		done, state := poll()
		if done {
			return
		}
		last = state
		if time.Now().After(deadline) {
			t.Fatalf("timed out waiting for %s: %s", what, last)
		}
		time.Sleep(2 * time.Second)
	}
}
