# Rad operator

The Rad operator reconciles one `Database` custom resource into one
single-writer Rad process. Each database uses an independent S3 bucket and is
published through the cluster's shared Ingress address.

The API is `radengine.dev/v1alpha1`. Kubernetes is the source of truth: the
controller has no separate database registry.

## Installation model

`config/install` is the complete installation tree: CRD, RBAC, and controller
resources. `config/examples` contains optional tenant examples and is never
included in an operator installation. Apply the complete tree with:

```sh
task deploy NAMESPACE=default
```

Use `task install` when a workflow needs to install only the CRD before the
controller. The [configuration guide](config/README.md) documents the layout
and the Kustomize entry points.

The production manifest runs two controller replicas with Lease-based leader
election and a PodDisruptionBudget. Only the leader reconciles. The controller
watches its own namespace, taken from `POD_NAMESPACE`; install another release
when a separate namespace needs an independent control plane.

For a `Database` named `example`, the controller creates:

- a single-replica `rad-example` StatefulSet;
- `rad-example` and `rad-example-headless` ClusterIP Services;
- a `rad-example` Ingress with the requested hostname and optional TLS;
- a `rad-example` PodDisruptionBudget;
- storage and hostname claim Leases;
- an optional ingress NetworkPolicy; and
- a tokenless `rad-example` ServiceAccount when static credentials are used.

No tenant Service has type `LoadBalancer`. Every Ingress uses the configured
shared IngressClass, so all hostnames converge on the same entrypoint.

## Cluster SDK

Kubernetes is the control-plane protocol: anything that can create a
`Database` resource can provision a database. A control plane that onboards
tenants programmatically uses the typed client in
`github.com/Southclaws/rad/operator/cluster` instead of raw manifests:

```go
client, err := cluster.New() // in-cluster ServiceAccount or kubeconfig

db, err := client.CreateDatabase(ctx, cluster.DatabaseSpec{
    Name:              "customer-123",
    Bucket:            "customer-123-rad",
    CredentialsSecret: "customer-123-s3", // or ServiceAccount for IRSA/STS
    Hostname:          "customer-123.rad.example.com",
})

db, err = client.WaitReady(ctx, "customer-123")
// db.URL is the tenant's external base URL.
```

`New` verifies through API discovery that the cluster serves
`radengine.dev` and returns `ErrNotInstalled` otherwise. Creation follows
Kubernetes semantics — accepted is not running — so readiness is a separate,
explicitly bounded wait, and a timeout names the unsatisfied dependency taken
from the CR's conditions.

There are no Rad control-plane credentials: authorization is Kubernetes RBAC
on `databases.radengine.dev` alone. A provisioning service needs
create/get/list/delete/watch on that resource and nothing on Pods, Secrets, or
StatefulSets — those stay the operator's privilege. Controller authors who
need the full resource surface use `operator/api/v1alpha1` directly.

## S3 identity and credential rotation

Static credentials are referenced rather than stored in the CRD:

```yaml
spec:
  storage:
    bucket: tenant-a-rad
    authentication:
      credentialsSecretRef:
        name: tenant-a-s3
```

The Secret is in the same namespace and uses AWS SDK environment names:

```yaml
stringData:
  AWS_ACCESS_KEY_ID: ...
  AWS_SECRET_ACCESS_KEY: ...
  AWS_SESSION_TOKEN: ... # optional STS session token
```

Each database may reference different credentials restricted to exactly its
bucket. The controller has `get`, but not `list` or `watch`, permission for
Secrets. It polls each referenced Secret and projects only the three supported
keys into the Rad container. A changed Secret resource version triggers an
ordered single-writer rollout.

Provider-native STS is selected with an existing ServiceAccount instead:

```yaml
spec:
  storage:
    authentication:
      serviceAccountName: tenant-a-rad
```

Configure that ServiceAccount with the cloud provider's workload-identity
mechanism. Rad's AWS SDK reads the projected web-identity token. Authentication
may be rotated between a Secret and a ServiceAccount without changing the
physical storage identity; bucket, prefix, region, endpoint, and catalog mode
remain immutable.

## Claims, routing, and TLS

Bucket plus endpoint and the external hostname are exclusive within the
operator namespace. Each claim is an atomically created Kubernetes Lease, so
simultaneous reconciles cannot both start writers. The first successful API
server create owns the claim; a conflicting database remains unready and any
route or writer it previously owned is quiesced. Claim names contain a
one-way digest rather than the bucket or hostname.

Route TLS can be terminated by the managed Ingress:

```yaml
spec:
  route:
    hostname: tenant-a.radengine.dev
    scheme: https
    tlsSecretName: tenant-a-tls
```

The controller validates that the referenced Secret is a non-empty
`kubernetes.io/tls` Secret before publishing the Ingress. Omit
`tlsSecretName` when HTTPS terminates before the cluster Ingress. Set
`scheme: http` only for local development.

An optional NetworkPolicy restricts direct tenant-pod ingress to a gateway
namespace and, optionally, selected gateway pods:

```text
--gateway-namespace=ingress-system
--gateway-pod-selector=app.kubernetes.io/name=traefik
```

Without `--gateway-namespace`, the operator does not manage a NetworkPolicy.
Cluster ingress authentication, authorization, certificates, and public DNS
remain deployment responsibilities.

## Lifecycle and failure handling

The StatefulSet has a hardened container security context, resource defaults,
and a configurable graceful termination window.
`spec.terminationGracePeriodSeconds` defaults to 120.

Each probe asks Rad a different question, so Kubernetes can tell a process that
needs restarting from a healthy one that should temporarily receive no traffic.
`api/openapi.yaml` is the contract for all three:

| Kubernetes probe | Rad endpoint | Purpose                                   |
| ---------------- | ------------ | ----------------------------------------- |
| startup          | `/startupz`  | Bound initial storage and catalog startup |
| readiness        | `/readyz`    | Control Service and Ingress traffic       |
| liveness         | `/livez`     | Restart a genuinely unhealthy process     |

Liveness deliberately ignores storage: a writer restarted for an object-store
outage comes back to the same outage, so storage faults withdraw traffic through
readiness instead. On a shutdown signal Rad withdraws readiness, keeps serving
for `RAD_SHUTDOWN_DRAIN_MS`, and only then stops listening. The operator sets
that window to one readiness cycle, capped at half the grace period, so the
endpoints controller has moved traffic before requests can fail.

Deletion removes the route and route claim first, drains deletion through the
single writer, then removes Services and the storage claim. The finalizer never
deletes the S3 bucket or its objects; `v1alpha1` supports `Retain` only.
Create/update conflicts use bounded Kubernetes retries. Reconciliation is
concurrency-limited, and the operator indexes claim Leases by database UID so
credential polling does not scan the database registry or all claim owners.

The controller records the desired and observed Rad image in status. Change
the controller's `--rad-image` deliberately and observe StatefulSet readiness
during rollout; the API does not allow tenant authors to supply arbitrary
images that could read their credential Secret.

## Status and observability

`kubectl get rad` shows readiness, URL, bucket, and age. Conditions distinguish
claim acceptance, credentials, workload readiness, route readiness, and final
readiness. Dependency failures also emit Kubernetes warning Events; successful
claim acquisition, rollouts, readiness, and finalization emit normal Events.

The manager serves controller-runtime metrics plus:

- `rad_operator_database_ready{namespace,database}`;
- `rad_operator_claim_conflicts_total{kind}`; and
- `rad_operator_reconcile_errors_total`.

The `rad-operator-metrics` Service and standard Prometheus pod annotations
publish port 8080 for scraping.

The manager exposes `/healthz` and `/readyz` on port 8081. These describe the
controller process, not any tenant database; tenant Rad processes expose their
own probes on port 7237.

## Configuration

Important manager flags are:

```text
--watch-namespace
--rad-image
--ingress-class
--gateway-namespace
--gateway-pod-selector
--dependency-poll-interval
--credential-poll-interval
--max-concurrent-reconciles
--leader-elect
```

The generated ClusterRole is bound with a namespace-scoped RoleBinding. This
allows shared generated RBAC while keeping its effective access in the
installation namespace.

## Development and qualification

Code generation is durable through `//go:generate` directives at the operator
module root:

```sh
task generate      # deepcopy, CRD, and RBAC
task manifests     # render installation, examples, and ignored local trees
task test          # race-enabled unit tests
task test:env      # envtest: reconciliation, admission, and the cluster SDK
task test:e2e      # outside-in suite against the current kubecontext
task mutation
task verify
```

The root Taskfile imports these under `operator:`, for example
`task operator:verify`. The Docker Desktop proof of concept and its operational
notes live under ignored `hack/poc`.

Testing is tiered by what each layer can prove. Unit tests cover reconciliation
decisions in isolation. Envtest runs a real kube-apiserver and etcd — CEL
validation and defaulting, claim atomicity, idempotent convergence (a converged
reconcile writes nothing, since spurious child updates roll the single writer),
drift repair, teardown ordering, controller restart recovery, and the manager's
watch wiring — and is where most operator behaviour belongs. Envtest runs no
kubelet or garbage collector: Deployments never produce Pods, and foreground
deletion never completes on its own (the suite substitutes that one GC
behaviour explicitly). Everything that needs a whole cluster — probes consumed
by a kubelet, writer fencing, image rollout, RBAC in anger — lives in the kind
e2e suite (`test/e2e`), which CI runs on operator-affecting changes. PRs run
envtest against the current supported Kubernetes; pushes to `main` sweep the
two prior minors as well (`task test:env ENVTEST_K8S=1.35.x!`).

Rad still needs an authenticated bucket identity marker or storage preflight
before the operator can report storage ownership more strongly than Kubernetes
intent.
