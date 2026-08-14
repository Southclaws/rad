package controller

import (
	"os"
	"slices"
	"testing"

	appsv1 "k8s.io/api/apps/v1"
	policyv1 "k8s.io/api/policy/v1"
	rbacv1 "k8s.io/api/rbac/v1"
	"sigs.k8s.io/yaml"
)

func TestGeneratedRBACCanOnlyGetSecrets(t *testing.T) {
	role := &rbacv1.ClusterRole{}
	readManifest(t, "../../config/install/rbac/role.yaml", role)
	found := false
	for _, rule := range role.Rules {
		if !slices.Contains(rule.Resources, "secrets") {
			continue
		}
		found = true
		if len(rule.Verbs) != 1 || rule.Verbs[0] != "get" {
			t.Fatalf("Secret permissions = %v, want get only", rule.Verbs)
		}
	}
	if !found {
		t.Fatal("generated RBAC has no Secret get permission")
	}

	binding := &rbacv1.RoleBinding{}
	readManifest(t, "../../config/install/rbac/role_binding.yaml", binding)
	if binding.RoleRef.Kind != "ClusterRole" || binding.RoleRef.Name != "rad-operator" {
		t.Fatalf("RoleBinding reference = %#v", binding.RoleRef)
	}
}

func TestProductionManagerManifestIsHighlyAvailable(t *testing.T) {
	deployment := &appsv1.Deployment{}
	readManifest(t, "../../config/install/manager/manager.yaml", deployment)
	if deployment.Spec.Replicas == nil || *deployment.Spec.Replicas != 2 {
		t.Fatalf("operator replicas = %v, want 2", deployment.Spec.Replicas)
	}
	if len(deployment.Spec.Template.Spec.TopologySpreadConstraints) == 0 {
		t.Fatal("operator has no topology spread constraint")
	}
	container := deployment.Spec.Template.Spec.Containers[0]
	if !slices.Contains(container.Args, "--leader-elect=true") {
		t.Fatalf("operator args = %v", container.Args)
	}
	if slices.Contains(container.Args, "--watch-namespace=default") {
		t.Fatal("operator manifest hard-codes the default namespace")
	}
	var namespaceFromPod bool
	for _, variable := range container.Env {
		if variable.Name == "POD_NAMESPACE" && variable.ValueFrom != nil && variable.ValueFrom.FieldRef != nil && variable.ValueFrom.FieldRef.FieldPath == "metadata.namespace" {
			namespaceFromPod = true
		}
	}
	if !namespaceFromPod {
		t.Fatal("operator does not derive its watch namespace from its pod")
	}

	budget := &policyv1.PodDisruptionBudget{}
	readManifest(t, "../../config/install/manager/pdb.yaml", budget)
	if budget.Spec.MinAvailable == nil || budget.Spec.MinAvailable.IntValue() != 1 {
		t.Fatalf("operator PDB minAvailable = %v", budget.Spec.MinAvailable)
	}
}

func readManifest(t *testing.T, path string, target any) {
	t.Helper()
	contents, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := yaml.UnmarshalStrict(contents, target); err != nil {
		t.Fatalf("decode %s: %v", path, err)
	}
}
