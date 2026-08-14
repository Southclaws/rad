package main

import (
	"flag"
	"os"
	"strings"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	coordinationv1 "k8s.io/api/coordination/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	policyv1 "k8s.io/api/policy/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	utilruntime "k8s.io/apimachinery/pkg/util/runtime"
	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	"k8s.io/utils/ptr"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/cache"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/healthz"
	"sigs.k8s.io/controller-runtime/pkg/log/zap"
	metricsserver "sigs.k8s.io/controller-runtime/pkg/metrics/server"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
	databasecontroller "github.com/Southclaws/rad/operator/internal/controller"
)

func main() {
	var (
		metricsAddress          string
		healthAddress           string
		leaderElection          bool
		watchNamespace          string
		radImage                string
		ingressClass            string
		gatewayNamespace        string
		gatewayPodSelector      string
		dependencyPollInterval  time.Duration
		credentialPollInterval  time.Duration
		maxConcurrentReconciles int
	)
	loggerOptions := zap.Options{Development: false}
	loggerOptions.BindFlags(flag.CommandLine)
	flag.StringVar(&metricsAddress, "metrics-bind-address", ":8080", "Address for the controller metrics endpoint; use 0 to disable it.")
	flag.StringVar(&healthAddress, "health-probe-bind-address", ":8081", "Address for controller health probes.")
	flag.BoolVar(&leaderElection, "leader-elect", true, "Use a Lease to elect one active controller replica.")
	flag.StringVar(&watchNamespace, "watch-namespace", os.Getenv("POD_NAMESPACE"), "Namespace containing Database resources; defaults to POD_NAMESPACE.")
	flag.StringVar(&radImage, "rad-image", "ghcr.io/southclaws/rad:edge", "Rad container image reconciled into tenant StatefulSets.")
	flag.StringVar(&ingressClass, "ingress-class", "", "IngressClass assigned to tenant host routes; empty uses the cluster default.")
	flag.StringVar(&gatewayNamespace, "gateway-namespace", "", "Namespace allowed to reach tenant Rad pods; empty does not manage NetworkPolicies.")
	flag.StringVar(&gatewayPodSelector, "gateway-pod-selector", "", "Label selector restricting gateway pods within gateway-namespace.")
	flag.DurationVar(&dependencyPollInterval, "dependency-poll-interval", 30*time.Second, "Polling interval for missing credentials and conflicting claims.")
	flag.DurationVar(&credentialPollInterval, "credential-poll-interval", time.Minute, "Polling interval for credential and workload identity rotation.")
	flag.IntVar(&maxConcurrentReconciles, "max-concurrent-reconciles", 4, "Maximum Database reconciliations in flight.")
	flag.Parse()

	ctrl.SetLogger(zap.New(zap.UseFlagOptions(&loggerOptions)))
	setupLog := ctrl.Log.WithName("setup")
	radImage = strings.TrimSpace(radImage)
	ingressClass = strings.TrimSpace(ingressClass)
	gatewayNamespace = strings.TrimSpace(gatewayNamespace)
	gatewayPodSelector = strings.TrimSpace(gatewayPodSelector)
	if radImage == "" {
		setupLog.Error(nil, "rad-image must not be empty")
		os.Exit(2)
	}
	watchNamespace = strings.TrimSpace(watchNamespace)
	if watchNamespace == "" {
		setupLog.Error(nil, "watch-namespace must not be empty; run one operator installation per namespace")
		os.Exit(2)
	}
	if dependencyPollInterval <= 0 || credentialPollInterval <= 0 || maxConcurrentReconciles <= 0 {
		setupLog.Error(nil, "poll intervals and max-concurrent-reconciles must be positive")
		os.Exit(2)
	}
	var gatewaySelector *metav1.LabelSelector
	if gatewayPodSelector != "" {
		if gatewayNamespace == "" {
			setupLog.Error(nil, "gateway-pod-selector requires gateway-namespace")
			os.Exit(2)
		}
		var err error
		gatewaySelector, err = metav1.ParseToLabelSelector(gatewayPodSelector)
		if err != nil {
			setupLog.Error(err, "parse gateway-pod-selector")
			os.Exit(2)
		}
	}

	scheme := runtime.NewScheme()
	utilruntime.Must(clientgoscheme.AddToScheme(scheme))
	utilruntime.Must(appsv1.AddToScheme(scheme))
	utilruntime.Must(coordinationv1.AddToScheme(scheme))
	utilruntime.Must(corev1.AddToScheme(scheme))
	utilruntime.Must(networkingv1.AddToScheme(scheme))
	utilruntime.Must(policyv1.AddToScheme(scheme))
	utilruntime.Must(radv1alpha1.AddToScheme(scheme))

	options := ctrl.Options{
		Scheme:                        scheme,
		Metrics:                       metricsserver.Options{BindAddress: metricsAddress},
		HealthProbeBindAddress:        healthAddress,
		LeaderElection:                leaderElection,
		LeaderElectionID:              "rad-operator.radengine.dev",
		LeaderElectionReleaseOnCancel: true,
		GracefulShutdownTimeout:       ptr.To(20 * time.Second),
		Client: client.Options{Cache: &client.CacheOptions{DisableFor: []client.Object{
			&corev1.Secret{},
			&corev1.ServiceAccount{},
		}}},
	}
	options.Cache = cache.Options{DefaultNamespaces: map[string]cache.Config{watchNamespace: {}}}

	manager, err := ctrl.NewManager(ctrl.GetConfigOrDie(), options)
	if err != nil {
		setupLog.Error(err, "create manager")
		os.Exit(1)
	}
	if err := (&databasecontroller.DatabaseReconciler{
		Client:                  manager.GetClient(),
		APIReader:               manager.GetAPIReader(),
		Scheme:                  manager.GetScheme(),
		Recorder:                manager.GetEventRecorderFor("rad-operator"),
		RadImage:                radImage,
		IngressClass:            ingressClass,
		GatewayNamespace:        gatewayNamespace,
		GatewayPodSelector:      gatewaySelector,
		DependencyPollInterval:  dependencyPollInterval,
		CredentialPollInterval:  credentialPollInterval,
		MaxConcurrentReconciles: maxConcurrentReconciles,
	}).SetupWithManager(manager); err != nil {
		setupLog.Error(err, "create Database controller")
		os.Exit(1)
	}
	if err := manager.AddHealthzCheck("healthz", healthz.Ping); err != nil {
		setupLog.Error(err, "configure health check")
		os.Exit(1)
	}
	if err := manager.AddReadyzCheck("readyz", healthz.Ping); err != nil {
		setupLog.Error(err, "configure readiness check")
		os.Exit(1)
	}

	setupLog.Info("starting Rad operator", "namespace", watchNamespace, "radImage", radImage, "ingressClass", ingressClass)
	if err := manager.Start(ctrl.SetupSignalHandler()); err != nil {
		setupLog.Error(err, "manager stopped")
		os.Exit(1)
	}
}
