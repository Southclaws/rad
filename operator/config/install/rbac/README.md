# Operator permissions

This directory contains the operator ServiceAccount, its namespace-scoped
RoleBinding, and the generated ClusterRole.

The ClusterRole defines reusable permissions, but the RoleBinding grants them
only in the installation namespace. Secret access is deliberately limited to
`get`; the operator cannot list or watch Secrets.

`role.yaml` is generated from controller RBAC markers. Regenerate it with
`go generate ./...` rather than editing it directly.
