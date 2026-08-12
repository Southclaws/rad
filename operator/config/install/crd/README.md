# Database CRD

This directory contains the cluster-scoped API definition required before a
`Database` can be created.

`radengine.dev_databases.yaml` is generated from the Go API markers. Do
not edit it by hand; regenerate it with `go generate ./...` or
`task operator:generate` from the repository root.

The local Kustomize entry point installs only the CRD:

```sh
kubectl apply -k config/install/crd
```
