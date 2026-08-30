package cluster

import (
	"context"
	"fmt"
	"time"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/api/meta"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

// CatalogMode selects who owns the database's catalog: `direct` exposes
// imperative catalog mutation over the API, `schema` (the default) gives
// ownership to rad.schema.yaml migrations. It is immutable once the database
// initializes its storage.
type CatalogMode string

// LogLevel selects the minimum Rad operational event severity.
type LogLevel string

// LogFormat selects the Rad event projection.
type LogFormat string

// DiagnosticLevel selects the maximum Rad response diagnostic level.
type DiagnosticLevel string

const (
	CatalogModeDirect CatalogMode = "direct"
	CatalogModeSchema CatalogMode = "schema"

	LogLevelError LogLevel = "error"
	LogLevelWarn  LogLevel = "warn"
	LogLevelInfo  LogLevel = "info"
	LogLevelDebug LogLevel = "debug"

	LogFormatText   LogFormat = "text"
	LogFormatJSON   LogFormat = "json"
	LogFormatLogfmt LogFormat = "logfmt"

	DiagnosticLevelOff      DiagnosticLevel = "off"
	DiagnosticLevelSummary  DiagnosticLevel = "summary"
	DiagnosticLevelDetailed DiagnosticLevel = "detailed"
	DiagnosticLevelFull     DiagnosticLevel = "full"
)

// DatabaseSpec is the product-level description of one tenant database. The
// zero values of the optional fields follow the API defaults.
type DatabaseSpec struct {
	// Name identifies the database within the operator's namespace and
	// derives every managed resource name.
	Name string

	// Bucket is the tenant-owned S3 bucket. Together with Endpoint it is an
	// exclusive claim: no two databases may share it.
	Bucket string
	// Prefix within the bucket; defaults to "rad".
	Prefix string
	// Region for the S3 client; defaults to "us-east-1".
	Region string
	// Endpoint selects an S3-compatible service; empty means AWS.
	Endpoint string

	// CredentialsSecret names a Secret holding AWS_ACCESS_KEY_ID and
	// AWS_SECRET_ACCESS_KEY. Exactly one of CredentialsSecret and
	// ServiceAccount must be set.
	CredentialsSecret string
	// ServiceAccount names an existing ServiceAccount configured for
	// provider-native workload identity such as EKS IRSA.
	ServiceAccount string

	// CatalogMode defaults to schema and is immutable after initialization.
	CatalogMode CatalogMode

	// Readers is the number of read-only instances, zero by default. Readers
	// serve queries from the same S3 objects as the writer and are reached
	// through [Database.ReaderService]; the external URL always reaches the
	// writer.
	Readers int32

	// LogLevel defaults to info.
	LogLevel LogLevel
	// LogFormat defaults to JSON in operator workloads.
	LogFormat LogFormat
	// LogPrograms emits program events independently from LogLevel.
	LogPrograms bool

	// OTelEndpoint selects the base OTLP HTTP endpoint. Empty disables export.
	OTelEndpoint string
	// Diagnostics defaults to summary.
	Diagnostics DiagnosticLevel
	// MetricsEnabled defaults to true when it is nil.
	MetricsEnabled *bool
	// CacheSizeMiB defaults to 128.
	CacheSizeMiB int32

	// InternalTLSMode secures the channel readers use to report statistics to
	// the writer: "auto" (the default) provisions a certificate through
	// cert-manager when it is installed and otherwise runs the channel
	// authenticated but unencrypted, "required" refuses to run it unencrypted,
	// "disabled" never encrypts it. It has no effect without readers.
	InternalTLSMode string
	// InternalTLSSecret supplies a kubernetes.io/tls Secret holding tls.crt,
	// tls.key, and ca.crt instead of provisioning one. The operator validates
	// it and never modifies it.
	InternalTLSSecret string

	// Hostname is the external hostname routed to this database through the
	// cluster's shared ingress.
	Hostname string
	// Scheme is the externally advertised URL scheme; defaults to https.
	Scheme string
	// TLSSecret enables ingress TLS from a kubernetes.io/tls Secret. Omit it
	// when TLS terminates before the cluster ingress.
	TLSSecret string
}

// Database is the observed state of one tenant database.
type Database struct {
	Name      string
	Namespace string
	Created   time.Time

	Bucket   string
	Endpoint string
	Hostname string

	// URL is the external base URL once the route is published.
	URL string
	// Ready reports the operator's final readiness condition: claims held,
	// credentials resolved, the single writer serving, and the route
	// published.
	Ready bool
	// Reason and Message describe the Ready condition, which names the first
	// unsatisfied dependency while Ready is false.
	Reason  string
	Message string

	Conditions []Condition

	DesiredImage  string
	ObservedImage string

	// ReaderService is the in-cluster Service name that addresses the readers,
	// empty while none are requested. Readers do not gate [Database.Ready]:
	// they add read capacity rather than availability.
	ReaderService  string
	DesiredReaders int32
	ReadyReaders   int32

	LogLevel       LogLevel
	LogFormat      LogFormat
	LogPrograms    bool
	OTelEndpoint   string
	Diagnostics    DiagnosticLevel
	MetricsEnabled bool
	CacheSizeMiB   int32

	// InternalTransportMode is "tls" or "plaintext", empty without readers.
	// It describes the statistics channel, never client traffic.
	InternalTransportMode string
	// InternalTransportProvider names what issued the certificate:
	// "cert-manager", "user", or "none".
	InternalTransportProvider string
}

// Condition is one controller observation, mirroring the Kubernetes
// convention without requiring apimachinery types downstream.
type Condition struct {
	Type    string
	Status  string
	Reason  string
	Message string
}

// CreateDatabase submits the desired state and returns once Kubernetes
// accepts it. The database is not yet running: reconciliation is
// asynchronous, so follow with [Client.WaitReady]. A name collision returns
// [ErrAlreadyExists].
func (c *Client) CreateDatabase(ctx context.Context, spec DatabaseSpec) (Database, error) {
	resource, err := c.resource(spec)
	if err != nil {
		return Database{}, err
	}
	if err := c.kube.Create(ctx, resource); err != nil {
		if apierrors.IsAlreadyExists(err) {
			return Database{}, fmt.Errorf("%w: %s", ErrAlreadyExists, spec.Name)
		}
		return Database{}, fmt.Errorf("create database %s: %w", spec.Name, err)
	}
	return databaseView(resource), nil
}

// GetDatabase returns the database's desired identity and observed status,
// or [ErrNotFound].
func (c *Client) GetDatabase(ctx context.Context, name string) (Database, error) {
	resource := &radv1alpha1.Database{}
	key := types.NamespacedName{Namespace: c.namespace, Name: name}
	if err := c.kube.Get(ctx, key, resource); err != nil {
		if apierrors.IsNotFound(err) {
			return Database{}, fmt.Errorf("%w: %s", ErrNotFound, name)
		}
		return Database{}, fmt.Errorf("get database %s: %w", name, err)
	}
	return databaseView(resource), nil
}

// ListDatabases returns every database in the client's namespace.
func (c *Client) ListDatabases(ctx context.Context) ([]Database, error) {
	list := &radv1alpha1.DatabaseList{}
	if err := c.kube.List(ctx, list, client.InNamespace(c.namespace)); err != nil {
		return nil, fmt.Errorf("list databases: %w", err)
	}
	databases := make([]Database, 0, len(list.Items))
	for index := range list.Items {
		databases = append(databases, databaseView(&list.Items[index]))
	}
	return databases, nil
}

// DeleteDatabase removes the database's Kubernetes resources and releases its
// claims. S3 data is always retained; reattach it later by creating a new
// database against the same bucket. Deleting an absent database returns
// [ErrNotFound].
func (c *Client) DeleteDatabase(ctx context.Context, name string) error {
	resource := &radv1alpha1.Database{
		ObjectMeta: metav1.ObjectMeta{Namespace: c.namespace, Name: name},
	}
	if err := c.kube.Delete(ctx, resource); err != nil {
		if apierrors.IsNotFound(err) {
			return fmt.Errorf("%w: %s", ErrNotFound, name)
		}
		return fmt.Errorf("delete database %s: %w", name, err)
	}
	return nil
}

// WaitReady blocks until the database's Ready condition holds, returning its
// final observed state. The wait is bounded only by ctx; on expiry the error
// carries the last observed condition so a timeout names the unsatisfied
// dependency.
func (c *Client) WaitReady(ctx context.Context, name string) (Database, error) {
	ticker := time.NewTicker(2 * time.Second)
	defer ticker.Stop()
	last := "no status observed yet"
	for {
		database, err := c.GetDatabase(ctx, name)
		if err == nil && database.Ready {
			return database, nil
		}
		if err != nil {
			last = err.Error()
		} else {
			last = fmt.Sprintf("%s: %s", database.Reason, database.Message)
		}
		select {
		case <-ctx.Done():
			return Database{}, fmt.Errorf("waiting for database %s: %w (last: %s)", name, ctx.Err(), last)
		case <-ticker.C:
		}
	}
}

func (c *Client) resource(spec DatabaseSpec) (*radv1alpha1.Database, error) {
	if spec.Name == "" {
		return nil, fmt.Errorf("database name is required")
	}
	authentication := radv1alpha1.S3Authentication{}
	switch {
	case spec.CredentialsSecret != "" && spec.ServiceAccount != "":
		return nil, fmt.Errorf("database %s: CredentialsSecret and ServiceAccount are mutually exclusive", spec.Name)
	case spec.CredentialsSecret != "":
		authentication.CredentialsSecretRef = &corev1.LocalObjectReference{Name: spec.CredentialsSecret}
	case spec.ServiceAccount != "":
		authentication.ServiceAccountName = &spec.ServiceAccount
	default:
		return nil, fmt.Errorf("database %s: one of CredentialsSecret or ServiceAccount is required", spec.Name)
	}
	diagnostics := spec.Diagnostics
	if diagnostics == "" {
		diagnostics = DiagnosticLevelSummary
	}
	metrics := spec.MetricsEnabled
	if metrics == nil {
		enabled := true
		metrics = &enabled
	}
	cacheSizeMiB := spec.CacheSizeMiB
	if cacheSizeMiB == 0 {
		cacheSizeMiB = 128
	}
	return &radv1alpha1.Database{
		ObjectMeta: metav1.ObjectMeta{Namespace: c.namespace, Name: spec.Name},
		Spec: radv1alpha1.DatabaseSpec{
			Storage: radv1alpha1.S3Storage{
				Bucket:         spec.Bucket,
				Prefix:         spec.Prefix,
				Region:         spec.Region,
				Endpoint:       spec.Endpoint,
				Authentication: authentication,
			},
			CatalogMode: radv1alpha1.CatalogMode(spec.CatalogMode),
			Readers:     spec.Readers,
			Logging: radv1alpha1.Logging{
				Level:    radv1alpha1.LogLevel(spec.LogLevel),
				Format:   radv1alpha1.LogFormat(spec.LogFormat),
				Programs: spec.LogPrograms,
			},
			Telemetry: radv1alpha1.Telemetry{
				Endpoint:    spec.OTelEndpoint,
				Diagnostics: radv1alpha1.DiagnosticLevel(diagnostics),
				Metrics:     metrics,
			},
			Cache: radv1alpha1.Cache{SizeMiB: cacheSizeMiB},
			InternalTLS: radv1alpha1.InternalTLS{
				Mode:       radv1alpha1.InternalTLSMode(spec.InternalTLSMode),
				SecretName: spec.InternalTLSSecret,
			},
			Route: radv1alpha1.Route{
				Hostname:      spec.Hostname,
				Scheme:        spec.Scheme,
				TLSSecretName: spec.TLSSecret,
			},
		},
	}, nil
}

func databaseView(resource *radv1alpha1.Database) Database {
	database := Database{
		Name:           resource.Name,
		Namespace:      resource.Namespace,
		Created:        resource.CreationTimestamp.Time,
		Bucket:         resource.Spec.Storage.Bucket,
		Endpoint:       resource.Spec.Storage.Endpoint,
		Hostname:       resource.Spec.Route.Hostname,
		URL:            resource.Status.URL,
		DesiredImage:   resource.Status.DesiredImage,
		ObservedImage:  resource.Status.ObservedImage,
		ReaderService:  resource.Status.ReaderServiceName,
		DesiredReaders: resource.Status.DesiredReaders,
		ReadyReaders:   resource.Status.ReadyReaders,
		LogLevel:       LogLevel(resource.Spec.Logging.Level),
		LogFormat:      LogFormat(resource.Spec.Logging.Format),
		LogPrograms:    resource.Spec.Logging.Programs,
		OTelEndpoint:   resource.Spec.Telemetry.Endpoint,
		Diagnostics:    DiagnosticLevel(resource.Spec.Telemetry.Diagnostics),
		MetricsEnabled: resource.Spec.Telemetry.Metrics == nil || *resource.Spec.Telemetry.Metrics,
		CacheSizeMiB:   resource.Spec.Cache.SizeMiB,
	}
	if transport := resource.Status.InternalTransport; transport != nil {
		database.InternalTransportMode = transport.Mode
		database.InternalTransportProvider = transport.Provider
	}
	for _, condition := range resource.Status.Conditions {
		database.Conditions = append(database.Conditions, Condition{
			Type:    condition.Type,
			Status:  string(condition.Status),
			Reason:  condition.Reason,
			Message: condition.Message,
		})
	}
	if ready := meta.FindStatusCondition(resource.Status.Conditions, radv1alpha1.ConditionReady); ready != nil {
		database.Ready = ready.Status == metav1.ConditionTrue
		database.Reason = ready.Reason
		database.Message = ready.Message
	} else {
		database.Reason = "Pending"
		database.Message = "the operator has not observed this database yet"
	}
	return database
}
