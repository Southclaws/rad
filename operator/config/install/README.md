# Operator installation

This is the complete installation tree for the Rad operator. It contains:

- `crd`: the generated `Database` API definition;
- `rbac`: the generated permissions and their namespace-scoped binding; and
- `manager`: the controller Deployment, metrics Service, and disruption budget.

Render or install the complete tree with:

```sh
kubectl kustomize config/install
kubectl --namespace rad-system apply -k config/install
```

The base is namespace-neutral. Select the installation namespace with
`kubectl --namespace`, the operator Taskfile's `NAMESPACE` variable, or a
Kustomize overlay. The current kubectl context supplies `default` when no
namespace is given. Pin the operator and Rad image references before a
production installation.
