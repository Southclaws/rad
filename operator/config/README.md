# Kubernetes configuration

This directory has two intentionally separate trees:

- [`install`](install) contains the CRD, permissions, and controller resources
  required to run the Rad operator.
- [`examples`](examples) contains optional example tenant resources. Nothing in
  this directory is part of the operator installation.

Use the Kustomize entry point in the relevant directory rather than applying
individual files from here.

The manager Deployment carries no `priorityClassName`: priority classes are
cluster-scoped and owned by the cluster administrator. In production, patch one
in so reconciliation survives node pressure — tenant databases keep serving
without the operator, but repair, credential rollout, deletion progress, and
status updates stop:

```yaml
# kustomize patch against Deployment/rad-operator
spec:
  template:
    spec:
      priorityClassName: your-controllers-priority
```
