# Controller manager

This directory contains the runtime resources for the operator itself:

- a two-replica Deployment using Lease-based leader election;
- a ClusterIP Service for Prometheus metrics; and
- a PodDisruptionBudget that preserves one controller replica.

The image references are installation defaults. Override them through a
Kustomize overlay and pin immutable release digests for production. Manager
flags such as the IngressClass and gateway NetworkPolicy selectors belong in
the same overlay.
