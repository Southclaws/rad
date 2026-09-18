# Controller manager

This directory contains the runtime resources for the operator itself:

- a two-replica Deployment using Lease-based leader election;
- a ClusterIP Service for Prometheus metrics; and
- a PodDisruptionBudget that preserves one controller replica.

The image references are installation defaults. Override them through a
Kustomize overlay and pin immutable release digests for production. Manager
flags such as the IngressClass and gateway NetworkPolicy selectors belong in
the same overlay.

The manager also accepts operator-wide public JWT settings through `RAD_AUTH`,
`RAD_AUTH_ISSUER`, `RAD_AUTH_AUDIENCE`, `RAD_AUTH_JWKS_URL`,
`RAD_AUTH_PROFILE`, and the four `RAD_AUTH_*_SCOPES` environment variables. A
database-specific `spec.authentication` replaces the complete operator
setting. Omission inherits the operator setting.
