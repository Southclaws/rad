// Package cluster is the product-level Go client for Rad databases on
// Kubernetes.
//
// Kubernetes is the control-plane protocol: this package manipulates
// Database custom resources through the Kubernetes API, and the Rad
// operator reconciles them into running databases. There is no separate Rad
// control-plane endpoint or credential — access is governed entirely by
// Kubernetes RBAC on the `databases.radengine.dev` resource, so a
// caller needs no permission on Pods, Secrets, or StatefulSets.
//
// Callers that build their own controllers should use the raw API types in
// `operator/api/v1alpha1` instead; this package trades the full resource
// surface for an interface shaped like the operations a control plane
// actually performs:
//
//	client, err := cluster.New()
//	db, err := client.CreateDatabase(ctx, cluster.DatabaseSpec{
//		Name:              "customer-123",
//		Bucket:            "customer-123-rad",
//		CredentialsSecret: "customer-123-s3",
//		Hostname:          "customer-123.rad.example.com",
//	})
//	db, err = client.WaitReady(ctx, "customer-123")
//
// Creation follows Kubernetes semantics: CreateDatabase returns once the
// desired state is accepted, not once the database runs. WaitReady observes
// the operator's Ready condition.
//
// The package never mutates process-global state. In binaries that configure
// no controller-runtime logger, its underlying client prints a one-time
// notice; route or silence it with
// `log.SetLogger` from sigs.k8s.io/controller-runtime/pkg/log.
package cluster

import (
	"errors"
	"fmt"
	"os"
	"strings"

	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/client-go/discovery"
	"k8s.io/client-go/rest"
	"k8s.io/client-go/tools/clientcmd"
	"sigs.k8s.io/controller-runtime/pkg/client"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

// ErrNotInstalled reports that the connected cluster does not serve the Rad
// API. Installation topology is deliberately not inspected — the only fact a
// client needs is whether `radengine.dev` is discoverable.
var ErrNotInstalled = errors.New("the Rad database API is not installed in this cluster")

// ErrNotFound reports that no database with the requested name exists.
var ErrNotFound = errors.New("database not found")

// ErrAlreadyExists reports that a database with the requested name exists.
var ErrAlreadyExists = errors.New("database already exists")

type Client struct {
	kube      client.Client
	namespace string
}

type options struct {
	restConfig  *rest.Config
	kubeconfig  string
	kubeContext string
	namespace   string
}

type Option func(*options)

// WithRESTConfig connects with an already-built Kubernetes configuration,
// bypassing kubeconfig and in-cluster resolution.
func WithRESTConfig(config *rest.Config) Option {
	return func(o *options) { o.restConfig = config }
}

// WithKubeconfig reads the given kubeconfig file instead of the default
// resolution order.
func WithKubeconfig(path string) Option {
	return func(o *options) { o.kubeconfig = path }
}

// WithContext selects a kubeconfig context by name.
func WithContext(name string) Option {
	return func(o *options) { o.kubeContext = name }
}

// WithNamespace targets the namespace containing the Database resources.
// The default is the connection's own namespace: the ServiceAccount namespace
// in-cluster, or the kubeconfig context's namespace outside.
func WithNamespace(namespace string) Option {
	return func(o *options) { o.namespace = namespace }
}

// New connects to Kubernetes and verifies the Rad API is served, returning
// [ErrNotInstalled] when it is not. Inside a pod the ServiceAccount is used;
// outside, kubeconfig resolution applies ($KUBECONFIG, then ~/.kube/config).
func New(opts ...Option) (*Client, error) {
	var resolved options
	for _, opt := range opts {
		opt(&resolved)
	}
	config, namespace, err := connect(&resolved)
	if err != nil {
		return nil, err
	}
	if err := verifyInstalled(config); err != nil {
		return nil, err
	}
	scheme := runtime.NewScheme()
	if err := radv1alpha1.AddToScheme(scheme); err != nil {
		return nil, err
	}
	kube, err := client.New(config, client.Options{Scheme: scheme})
	if err != nil {
		return nil, fmt.Errorf("create Kubernetes client: %w", err)
	}
	return &Client{kube: kube, namespace: namespace}, nil
}

func connect(resolved *options) (*rest.Config, string, error) {
	if resolved.restConfig != nil {
		return resolved.restConfig, defaultString(resolved.namespace, "default"), nil
	}
	if resolved.kubeconfig == "" && resolved.kubeContext == "" {
		if config, err := rest.InClusterConfig(); err == nil {
			return config, defaultString(resolved.namespace, inClusterNamespace()), nil
		}
	}
	rules := clientcmd.NewDefaultClientConfigLoadingRules()
	if resolved.kubeconfig != "" {
		rules.ExplicitPath = resolved.kubeconfig
	}
	loader := clientcmd.NewNonInteractiveDeferredLoadingClientConfig(
		rules, &clientcmd.ConfigOverrides{CurrentContext: resolved.kubeContext})
	config, err := loader.ClientConfig()
	if err != nil {
		return nil, "", fmt.Errorf("resolve Kubernetes configuration: %w", err)
	}
	namespace := resolved.namespace
	if namespace == "" {
		namespace, _, _ = loader.Namespace()
	}
	return config, defaultString(namespace, "default"), nil
}

func inClusterNamespace() string {
	contents, err := os.ReadFile("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(contents))
}

func defaultString(value, fallback string) string {
	if value == "" {
		return fallback
	}
	return value
}

func verifyInstalled(config *rest.Config) error {
	discoverer, err := discovery.NewDiscoveryClientForConfig(config)
	if err != nil {
		return fmt.Errorf("create discovery client: %w", err)
	}
	groupVersion := radv1alpha1.GroupVersion.String()
	resources, err := discoverer.ServerResourcesForGroupVersion(groupVersion)
	if err != nil {
		return fmt.Errorf("%w: %s is not served", ErrNotInstalled, groupVersion)
	}
	for _, resource := range resources.APIResources {
		if resource.Kind == "Database" {
			return nil
		}
	}
	return fmt.Errorf("%w: %s serves no Database resource", ErrNotInstalled, groupVersion)
}
