// Package operator owns durable code-generation entry points for the Kubernetes
// API, CRD, and least-privilege RBAC manifests.
package operator

//go:generate go run sigs.k8s.io/controller-tools/cmd/controller-gen@v0.21.0 object:headerFile= paths=./api/...
//go:generate go run sigs.k8s.io/controller-tools/cmd/controller-gen@v0.21.0 crd:crdVersions=v1 paths=./api/... output:crd:artifacts:config=config/install/crd
//go:generate go run sigs.k8s.io/controller-tools/cmd/controller-gen@v0.21.0 rbac:roleName=rad-operator paths=./internal/controller/... output:rbac:artifacts:config=config/install/rbac
