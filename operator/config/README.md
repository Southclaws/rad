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

Set an operator-wide JWT policy by patching the manager Deployment environment:

```yaml
spec:
  template:
    spec:
      containers:
        - name: manager
          env:
            - name: RAD_AUTH
              value: jwt
            - name: RAD_AUTH_ISSUER
              value: https://auth.example.com/
            - name: RAD_AUTH_AUDIENCE
              value: rad-production
            - name: RAD_AUTH_PROFILE
              value: rfc9068
            - name: RAD_AUTH_QUERY_SCOPES
              value: rad:read rad:admin
            - name: RAD_AUTH_MUTATE_SCOPES
              value: rad:write rad:admin
            - name: RAD_AUTH_CATALOG_SCOPES
              value: rad:catalog rad:admin
            - name: RAD_AUTH_ADMIN_SCOPES
              value: rad:admin
```

The matching manager flags use lower-case names, such as `--auth-issuer` and
`--auth-profile`. The profile defaults to `rfc9068`. Set it to `compatible` for
authorization servers that do not issue RFC 9068 access tokens. Set it to
`cloudflare-access` for Cloudflare Access application tokens. Each scope
setting for this profile can contain only `authenticated`. At least one scope
setting is required in JWT mode. Missing capability settings deny those
capabilities. A `Database` with `spec.authentication` replaces this default.
A `Database` without that field inherits this default and cannot disable it.
