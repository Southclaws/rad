# Example tenant resources

These files demonstrate one `Database` and its static S3 credential Secret.
They are examples only and are not included by `config/install`.

Replace the hostname, bucket, and credential values before applying them. For
production, prefer a secret manager or workload-identity ServiceAccount rather
than committing credential values.

Render both examples with:

```sh
kubectl kustomize config/examples
```

Apply them only after the operator installation is ready.
