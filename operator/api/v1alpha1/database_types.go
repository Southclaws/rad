package v1alpha1

import (
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// InternalTransportStatus is the observed shape of the reader-to-writer
// channel.
type InternalTransportStatus struct {
	// Mode is tls or plaintext.
	Mode string `json:"mode,omitempty"`
	// Provider names what issued the certificate: cert-manager, user, or none.
	Provider string `json:"provider,omitempty"`
	// CertificateSecret holds the serving certificate, when there is one.
	CertificateSecret string `json:"certificateSecret,omitempty"`
	// WriterAddress is the URL readers are configured to report to.
	WriterAddress string `json:"writerAddress,omitempty"`
}

const (
	ConditionClaimsAccepted   = "ClaimsAccepted"
	ConditionCredentialsReady = "CredentialsReady"
	ConditionWorkloadReady    = "WorkloadReady"
	ConditionRouteReady       = "RouteReady"
	ConditionReady            = "Ready"
	// ConditionInternalTLSReady reports the statistics channel's transport
	// security. It is deliberately separate from Ready: losing the channel
	// costs the fleet a better planner, not availability.
	ConditionInternalTLSReady = "InternalTLSReady"
)

// CatalogMode is the immutable catalog authority selected when Rad first opens
// a bucket.
// +kubebuilder:validation:Enum=direct;schema
type CatalogMode string

const (
	CatalogModeDirect CatalogMode = "direct"
	CatalogModeSchema CatalogMode = "schema"
)

// DeletionPolicy controls what happens to external storage when the Kubernetes
// object is deleted. The first API intentionally supports retention only.
// +kubebuilder:validation:Enum=Retain
type DeletionPolicy string

const DeletionPolicyRetain DeletionPolicy = "Retain"

// LogLevel selects the minimum operational event severity.
// +kubebuilder:validation:Enum=error;warn;info;debug
type LogLevel string

// LogFormat selects the standard error event projection.
// +kubebuilder:validation:Enum=text;json;logfmt
type LogFormat string

const (
	LogLevelError LogLevel = "error"
	LogLevelWarn  LogLevel = "warn"
	LogLevelInfo  LogLevel = "info"
	LogLevelDebug LogLevel = "debug"

	LogFormatText   LogFormat = "text"
	LogFormatJSON   LogFormat = "json"
	LogFormatLogfmt LogFormat = "logfmt"
)

// Logging configures Rad events for all database pods.
type Logging struct {
	// +kubebuilder:default=info
	Level LogLevel `json:"level,omitempty"`

	// +kubebuilder:default=json
	Format LogFormat `json:"format,omitempty"`

	// +kubebuilder:default=false
	Programs bool `json:"programs,omitempty"`
}

// DiagnosticLevel selects the maximum program diagnostic document level.
// +kubebuilder:validation:Enum=off;summary;detailed;full
type DiagnosticLevel string

const (
	DiagnosticLevelOff      DiagnosticLevel = "off"
	DiagnosticLevelSummary  DiagnosticLevel = "summary"
	DiagnosticLevelDetailed DiagnosticLevel = "detailed"
	DiagnosticLevelFull     DiagnosticLevel = "full"
)

// Telemetry configures Rad traces, metrics, and response diagnostics.
type Telemetry struct {
	// Endpoint is the base OTLP HTTP endpoint. An empty value disables export.
	// +optional
	// +kubebuilder:validation:Pattern=`^$|^https?://`
	Endpoint string `json:"endpoint,omitempty"`

	// Diagnostics limits the level that an HTTP request can select.
	// +kubebuilder:default=summary
	Diagnostics DiagnosticLevel `json:"diagnostics,omitempty"`

	// Metrics enables OTEL metric export and the metrics HTTP route.
	// +optional
	// +kubebuilder:default=true
	Metrics *bool `json:"metrics,omitempty"`
}

// SlateObjectCache configures the local raw object cache for each database pod.
type SlateObjectCache struct {
	// Enabled creates an ephemeral cache directory for each pod.
	// +kubebuilder:default=false
	Enabled bool `json:"enabled,omitempty"`

	// +kubebuilder:default=16384
	// +kubebuilder:validation:Minimum=1
	SizeMiB int32 `json:"sizeMiB,omitempty"`

	// +kubebuilder:default=4096
	// +kubebuilder:validation:Minimum=1
	PartSizeKiB int32 `json:"partSizeKiB,omitempty"`

	// +kubebuilder:default=false
	CacheOnFlush bool `json:"cacheOnFlush,omitempty"`

	// +kubebuilder:default=false
	CacheOnCompaction bool `json:"cacheOnCompaction,omitempty"`

	// +kubebuilder:default=none
	// +kubebuilder:validation:Enum=none;l0;all
	Preload string `json:"preload,omitempty"`
}

// Slate configures storage behavior for all database pods.
// +kubebuilder:validation:XValidation:rule="self.l0MaxSSTsPerKey <= self.l0MaxSSTs",message="l0MaxSSTsPerKey must not exceed l0MaxSSTs"
// +kubebuilder:validation:XValidation:rule="self.maxUnflushedMiB >= self.l0SSTSizeMiB",message="maxUnflushedMiB must be at least l0SSTSizeMiB"
type Slate struct {
	// +kubebuilder:default=128
	// +kubebuilder:validation:Minimum=16
	DecodedCacheSizeMiB int32 `json:"decodedCacheSizeMiB,omitempty"`

	// +kubebuilder:default=false
	ScanCacheBlocks bool `json:"scanCacheBlocks,omitempty"`

	// +kubebuilder:default=256
	// +kubebuilder:validation:Minimum=1
	ScanReadAheadKiB int32 `json:"scanReadAheadKiB,omitempty"`

	// +kubebuilder:default=4
	// +kubebuilder:validation:Minimum=1
	ScanMaxFetchTasks int32 `json:"scanMaxFetchTasks,omitempty"`

	// +kubebuilder:default=100
	// +kubebuilder:validation:Minimum=1
	FlushIntervalMilliseconds int32 `json:"flushIntervalMilliseconds,omitempty"`

	// +kubebuilder:default=64
	// +kubebuilder:validation:Minimum=1
	L0SSTSizeMiB int32 `json:"l0SSTSizeMiB,omitempty"`

	// +kubebuilder:default=4096
	// +kubebuilder:validation:Minimum=1
	MaxWALFlushesBeforeL0Flush int64 `json:"maxWALFlushesBeforeL0Flush,omitempty"`

	// +kubebuilder:default=8
	// +kubebuilder:validation:Minimum=1
	L0MaxSSTs int32 `json:"l0MaxSSTs,omitempty"`

	// +kubebuilder:default=8
	// +kubebuilder:validation:Minimum=1
	L0MaxSSTsPerKey int32 `json:"l0MaxSSTsPerKey,omitempty"`

	// +kubebuilder:default=4
	// +kubebuilder:validation:Minimum=1
	L0FlushParallelism int32 `json:"l0FlushParallelism,omitempty"`

	// +kubebuilder:default=1024
	// +kubebuilder:validation:Minimum=1
	MaxUnflushedMiB int32 `json:"maxUnflushedMiB,omitempty"`

	// +kubebuilder:default=1000
	// +kubebuilder:validation:Minimum=1
	MinFilterKeys int32 `json:"minFilterKeys,omitempty"`

	// +kubebuilder:default=10
	// +kubebuilder:validation:Minimum=1
	BloomBitsPerKey int32 `json:"bloomBitsPerKey,omitempty"`

	// +kubebuilder:default=4
	// +kubebuilder:validation:Enum=1;2;4;8;16;32;64
	SSTBlockSizeKiB int32 `json:"sstBlockSizeKiB,omitempty"`

	// +optional
	ObjectCache SlateObjectCache `json:"objectCache,omitempty"`
}

// S3Authentication selects one credential source. A Secret contains AWS SDK
// environment variable names; a ServiceAccount enables provider-native
// workload identity such as AssumeRoleWithWebIdentity.
// +kubebuilder:validation:XValidation:rule="has(self.credentialsSecretRef) != has(self.serviceAccountName)",message="exactly one of credentialsSecretRef or serviceAccountName is required"
// +kubebuilder:validation:XValidation:rule="!has(self.credentialsSecretRef) || size(self.credentialsSecretRef.name) > 0",message="credentialsSecretRef.name must not be empty"
// +kubebuilder:validation:XValidation:rule="!has(self.serviceAccountName) || size(self.serviceAccountName) > 0",message="serviceAccountName must not be empty"
type S3Authentication struct {
	// CredentialsSecretRef names a Secret in the Database namespace. It must
	// contain AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY and may contain
	// AWS_SESSION_TOKEN.
	// +optional
	CredentialsSecretRef *corev1.LocalObjectReference `json:"credentialsSecretRef,omitempty"`

	// ServiceAccountName names an existing ServiceAccount in the Database
	// namespace configured for provider-native workload identity.
	// +optional
	ServiceAccountName *string `json:"serviceAccountName,omitempty"`
}

// S3Storage identifies one tenant-owned bucket. Bucket and endpoint together
// form an exclusive runtime claim among Database resources.
// +kubebuilder:validation:XValidation:rule="self.bucket == oldSelf.bucket && self.prefix == oldSelf.prefix && self.region == oldSelf.region && ((!has(self.endpoint) && !has(oldSelf.endpoint)) || (has(self.endpoint) && has(oldSelf.endpoint) && self.endpoint == oldSelf.endpoint))",message="storage location is immutable"
type S3Storage struct {
	// +kubebuilder:validation:MinLength=1
	Bucket string `json:"bucket"`

	// Prefix is the SlateDB object prefix within the bucket.
	// +kubebuilder:default=rad
	// +kubebuilder:validation:MinLength=1
	Prefix string `json:"prefix,omitempty"`

	// Region is passed to the S3 client.
	// +kubebuilder:default=us-east-1
	// +kubebuilder:validation:MinLength=1
	Region string `json:"region,omitempty"`

	// Endpoint selects an S3-compatible service such as RustFS. Omit it for AWS.
	// +optional
	// +kubebuilder:validation:Pattern=`^$|^https?://`
	Endpoint string `json:"endpoint,omitempty"`

	Authentication S3Authentication `json:"authentication"`
}

// Route describes the tenant hostname published through the cluster's
// shared ingress implementation.
// +kubebuilder:validation:XValidation:rule="!has(self.tlsSecretName) || self.scheme == 'https'",message="tlsSecretName requires scheme https"
type Route struct {
	// Hostname is matched without modifying Rad's root-relative HTTP API.
	// +kubebuilder:validation:MinLength=1
	// +kubebuilder:validation:MaxLength=253
	// +kubebuilder:validation:Pattern=`^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$`
	Hostname string `json:"hostname"`

	// Scheme is the externally advertised URL scheme. Traffic from the Ingress
	// to Rad remains HTTP.
	// +kubebuilder:default=https
	// +kubebuilder:validation:Enum=http;https
	Scheme string `json:"scheme,omitempty"`

	// TLSSecretName enables Ingress TLS with a Secret in the database namespace.
	// It may be omitted when TLS terminates before the cluster Ingress.
	// +optional
	TLSSecretName string `json:"tlsSecretName,omitempty"`
}

// InternalTLSMode selects how the instance-to-instance channel is secured.
// +kubebuilder:validation:Enum=auto;required;disabled
type InternalTLSMode string

const (
	// InternalTLSAuto uses a supplied certificate, else provisions one through
	// cert-manager when it is installed, else serves plain HTTP.
	InternalTLSAuto InternalTLSMode = "auto"
	// InternalTLSRequired refuses to serve the channel unencrypted, holding
	// the database unready rather than falling back.
	InternalTLSRequired InternalTLSMode = "required"
	// InternalTLSDisabled serves plain HTTP whatever is installed.
	InternalTLSDisabled InternalTLSMode = "disabled"
)

// IssuerReference names an existing cert-manager issuer, for a deployment with
// its own certificate authority.
type IssuerReference struct {
	// +kubebuilder:validation:MinLength=1
	Name string `json:"name"`

	// +kubebuilder:default=Issuer
	// +kubebuilder:validation:Enum=Issuer;ClusterIssuer
	Kind string `json:"kind,omitempty"`

	// +kubebuilder:default=cert-manager.io
	Group string `json:"group,omitempty"`
}

// InternalTLS configures transport security for the channel readers use to
// report statistics to the writer. It never affects client traffic.
// +kubebuilder:validation:XValidation:rule="!(has(self.secretName) && has(self.issuerRef))",message="secretName and issuerRef are mutually exclusive"
// +kubebuilder:validation:XValidation:rule="!(self.mode == 'disabled' && (has(self.secretName) || has(self.issuerRef)))",message="disabled internal TLS cannot also name a certificate source"
type InternalTLS struct {
	// +kubebuilder:default=auto
	Mode InternalTLSMode `json:"mode,omitempty"`

	// SecretName is a kubernetes.io/tls Secret holding tls.crt, tls.key, and
	// ca.crt. The operator validates it but never owns or modifies it.
	// +optional
	SecretName string `json:"secretName,omitempty"`

	// IssuerRef delegates issuance to an existing cert-manager issuer instead
	// of the per-database authority the operator would otherwise create.
	// +optional
	IssuerRef *IssuerReference `json:"issuerRef,omitempty"`
}

// DatabaseSpec is the desired state for one Rad process and one physical S3
// bucket.
type DatabaseSpec struct {
	// The physical storage fields are immutable, while authentication may be
	// rotated or migrated between supported identity mechanisms.
	Storage S3Storage `json:"storage"`

	// CatalogMode is immutable after the database is initialized.
	// +kubebuilder:default=schema
	// +kubebuilder:validation:XValidation:rule="self == oldSelf",message="catalogMode is immutable"
	CatalogMode CatalogMode `json:"catalogMode,omitempty"`

	Route Route `json:"route"`

	// Readers is the number of read-only Rad instances. A Rad instance holds no
	// data: every instance reads the same S3 objects, so a reader adds read
	// capacity rather than a copy of the database. Readers are addressable
	// through their own Service; the published route always reaches the writer,
	// because a client that reads immediately after writing would otherwise
	// observe reader lag.
	// +kubebuilder:default=0
	// +kubebuilder:validation:Minimum=0
	// +kubebuilder:validation:Maximum=32
	Readers int32 `json:"readers,omitempty"`

	// CaptureWorkloadCorpus records canonical programs with literal values for
	// bounded offline replay. The writer and each reader apply the same policy.
	// +kubebuilder:default=false
	CaptureWorkloadCorpus bool `json:"captureWorkloadCorpus,omitempty"`

	// Logging applies the same event policy to the writer and all readers.
	// +optional
	// +kubebuilder:default={level: info, format: json, programs: false}
	Logging Logging `json:"logging,omitempty"`

	// Telemetry applies the same export and diagnostic policy to all pods.
	// +optional
	// +kubebuilder:default={diagnostics: summary, metrics: true}
	Telemetry Telemetry `json:"telemetry,omitempty"`

	// Slate applies the same storage settings to all pods.
	// +optional
	Slate Slate `json:"slate,omitempty"`

	// InternalTLS secures the reader-to-writer statistics channel. It has no
	// effect while no readers exist, because the channel does not exist.
	// +optional
	// +kubebuilder:default={mode: auto}
	InternalTLS InternalTLS `json:"internalTLS,omitempty"`

	// Resources applies to every Rad container, writer and reader alike. Empty
	// resources use the controller's conservative defaults.
	// +optional
	Resources corev1.ResourceRequirements `json:"resources,omitempty"`

	// DeletionPolicy is Retain in v1alpha1: deleting the CRD never deletes S3
	// objects or the bucket.
	// +kubebuilder:default=Retain
	DeletionPolicy DeletionPolicy `json:"deletionPolicy,omitempty"`

	// TerminationGracePeriodSeconds bounds graceful Rad shutdown before
	// Kubernetes forcefully terminates the writer.
	// +kubebuilder:default=120
	// +kubebuilder:validation:Minimum=30
	// +kubebuilder:validation:Maximum=3600
	TerminationGracePeriodSeconds int64 `json:"terminationGracePeriodSeconds,omitempty"`
}

// DatabaseStatus is the controller-observed state. Kubernetes resources are
// derived and may be reconstructed from spec after a controller restart.
type DatabaseStatus struct {
	ObservedGeneration int64  `json:"observedGeneration,omitempty"`
	URL                string `json:"url,omitempty"`
	ServiceName        string `json:"serviceName,omitempty"`
	StatefulSetName    string `json:"statefulSetName,omitempty"`
	ReadyReplicas      int32  `json:"readyReplicas,omitempty"`
	DesiredImage       string `json:"desiredImage,omitempty"`
	ObservedImage      string `json:"observedImage,omitempty"`
	// ReaderServiceName is empty while no readers are requested.
	ReaderServiceName string `json:"readerServiceName,omitempty"`
	DesiredReaders    int32  `json:"desiredReaders,omitempty"`
	ReadyReaders      int32  `json:"readyReaders,omitempty"`

	// InternalTransport describes how readers reach the writer, so an operator
	// can see whether the channel is encrypted without inspecting pods.
	// +optional
	InternalTransport *InternalTransportStatus `json:"internalTransport,omitempty"`
	// +listType=map
	// +listMapKey=type
	Conditions []metav1.Condition `json:"conditions,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=rad
// +kubebuilder:printcolumn:name="Ready",type="string",JSONPath=".status.conditions[?(@.type=='Ready')].status"
// +kubebuilder:printcolumn:name="URL",type="string",JSONPath=".status.url"
// +kubebuilder:printcolumn:name="Bucket",type="string",JSONPath=".spec.storage.bucket"
// +kubebuilder:printcolumn:name="Readers",type="integer",JSONPath=".status.readyReaders"
// +kubebuilder:printcolumn:name="Age",type="date",JSONPath=".metadata.creationTimestamp"
type Database struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   DatabaseSpec   `json:"spec,omitempty"`
	Status DatabaseStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true
type DatabaseList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []Database `json:"items"`
}

func init() {
	SchemeBuilder.Register(&Database{}, &DatabaseList{})
}
