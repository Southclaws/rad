package controller

import (
	"context"
	"maps"
	"strings"
	"testing"
	"time"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/meta"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
	"sigs.k8s.io/controller-runtime/pkg/client"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

func testClientAuthentication(issuer string) *radv1alpha1.JWTAuthentication {
	return &radv1alpha1.JWTAuthentication{
		Issuer:        issuer,
		Audience:      "rad-production",
		JWKSURL:       issuer + "keys",
		QueryScopes:   []radv1alpha1.OAuthScope{"rad:read", "rad:admin"},
		MutateScopes:  []radv1alpha1.OAuthScope{"rad:write"},
		CatalogScopes: []radv1alpha1.OAuthScope{"rad:catalog"},
		AdminScopes:   []radv1alpha1.OAuthScope{"rad:admin"},
	}
}

func TestDatabaseAuthenticationConfiguresWriterAndReaders(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Route.Scheme = "https"
	database.Spec.Readers = 1
	database.Spec.Authentication = testClientAuthentication("https://database-auth.example.com/")
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconcileReadySpec(t, reconciler, database)

	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	deployment := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha-reader"}, &appsv1.Deployment{})
	for role, container := range map[string]corev1.Container{
		"writer": statefulSet.Spec.Template.Spec.Containers[0],
		"reader": deployment.Spec.Template.Spec.Containers[0],
	} {
		values := environmentMap(container.Env)
		want := map[string]string{
			"RAD_ADMIN_ADDR":          "127.0.0.1:7238",
			"RAD_AUTH":                "jwt",
			"RAD_AUTH_ISSUER":         "https://database-auth.example.com/",
			"RAD_AUTH_AUDIENCE":       "rad-production",
			"RAD_AUTH_JWKS_URL":       "https://database-auth.example.com/keys",
			"RAD_AUTH_PROFILE":        "rfc9068",
			"RAD_AUTH_QUERY_SCOPES":   "rad:admin rad:read",
			"RAD_AUTH_MUTATE_SCOPES":  "rad:write",
			"RAD_AUTH_CATALOG_SCOPES": "rad:catalog",
			"RAD_AUTH_ADMIN_SCOPES":   "rad:admin",
		}
		for name, value := range want {
			if values[name] != value {
				t.Errorf("%s %s = %q, want %q", role, name, values[name], value)
			}
		}
		if hasNamedContainerPort(container.Ports, "admin") {
			t.Errorf("%s exposes the admin container port", role)
		}
	}

	service := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &corev1.Service{})
	if len(service.Spec.Ports) != 1 || service.Spec.Ports[0].Name != "http" {
		t.Fatalf("frontend service ports = %#v, want only HTTP", service.Spec.Ports)
	}
	if _, exists := statefulSet.Spec.Template.Annotations["prometheus.io/scrape"]; exists {
		t.Fatalf("authenticated workload has Prometheus scrape annotations: %#v", statefulSet.Spec.Template.Annotations)
	}
}

func TestDatabaseAuthenticationReplacesTheOperatorSetting(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Route.Scheme = "https"
	database.Spec.Authentication = testClientAuthentication("https://database-auth.example.com/")
	database.Spec.Authentication.Profile = radv1alpha1.JWTProfileCompatible
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconciler.DefaultAuthentication = testClientAuthentication("https://operator-auth.example.com/")
	reconcileReadySpec(t, reconciler, database)

	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	values := environmentMap(statefulSet.Spec.Template.Spec.Containers[0].Env)
	if values["RAD_AUTH_ISSUER"] != "https://database-auth.example.com/" {
		t.Fatalf("JWT issuer = %q, want the database setting", values["RAD_AUTH_ISSUER"])
	}
	if values["RAD_AUTH_PROFILE"] != "compatible" {
		t.Fatalf("JWT profile = %q, want the database setting", values["RAD_AUTH_PROFILE"])
	}
}

func TestDatabaseAuthenticationRemainsScopedToEachDatabase(t *testing.T) {
	alpha := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	alpha.Spec.Route.Scheme = "https"
	alpha.Spec.Readers = 1
	alpha.Spec.Authentication = testClientAuthentication("https://alpha-auth.example.com/")
	alpha.Spec.Authentication.Audience = "alpha-rad"

	beta := testDatabase("beta", "beta-bucket", "beta.rad.localhost", "beta-s3", time.Unix(2, 0))
	beta.Spec.Route.Scheme = "https"
	beta.Spec.Readers = 1
	beta.Spec.Authentication = testClientAuthentication("https://beta-auth.example.com/")
	beta.Spec.Authentication.Audience = "beta-rad"
	beta.Spec.Authentication.Profile = radv1alpha1.JWTProfileCompatible
	beta.Spec.Authentication.QueryScopes = []radv1alpha1.OAuthScope{"beta:read"}

	reconciler, kubernetesClient := testReconciler(t, alpha, beta, testSecret("alpha-s3"), testSecret("beta-s3"))
	reconcileReadySpec(t, reconciler, alpha)
	reconcileReadySpec(t, reconciler, beta)

	assertDatabaseAuthenticationWorkloads(t, kubernetesClient, alpha.Name, alpha.Spec.Authentication)
	assertDatabaseAuthenticationWorkloads(t, kubernetesClient, beta.Name, beta.Spec.Authentication)
}

func TestOperatorAuthenticationAppliesTheSamePolicyToEachDatabase(t *testing.T) {
	alpha := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	alpha.Spec.Route.Scheme = "https"
	alpha.Spec.Readers = 1
	beta := testDatabase("beta", "beta-bucket", "beta.rad.localhost", "beta-s3", time.Unix(2, 0))
	beta.Spec.Route.Scheme = "https"
	beta.Spec.Readers = 1

	authentication := testClientAuthentication("https://operator-auth.example.com/")
	reconciler, kubernetesClient := testReconciler(t, alpha, beta, testSecret("alpha-s3"), testSecret("beta-s3"))
	reconciler.DefaultAuthentication = authentication
	reconcileReadySpec(t, reconciler, alpha)
	reconcileReadySpec(t, reconciler, beta)

	assertDatabaseAuthenticationWorkloads(t, kubernetesClient, alpha.Name, authentication)
	assertDatabaseAuthenticationWorkloads(t, kubernetesClient, beta.Name, authentication)
}

func TestDatabaseCannotDisableTheOperatorAuthenticationSetting(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	database.Spec.Route.Scheme = "https"
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconciler.DefaultAuthentication = testClientAuthentication("https://operator-auth.example.com/")
	reconcileReadySpec(t, reconciler, database)

	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})
	values := environmentMap(statefulSet.Spec.Template.Spec.Containers[0].Env)
	if values["RAD_AUTH"] != "jwt" || values["RAD_AUTH_ISSUER"] != "https://operator-auth.example.com/" {
		t.Fatalf("operator authentication environment = %#v", values)
	}
}

func TestOperatorAuthenticationQuiescesAnHTTPRoute(t *testing.T) {
	database := testDatabase("alpha", "alpha-bucket", "alpha.rad.localhost", "alpha-s3", time.Unix(1, 0))
	reconciler, kubernetesClient := testReconciler(t, database, testSecret("alpha-s3"))
	reconcileReadySpec(t, reconciler, database)
	getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{})

	reconciler.DefaultAuthentication = testClientAuthentication("https://operator-auth.example.com/")
	reconcileReadySpec(t, reconciler, database)

	if err := kubernetesClient.Get(context.Background(), types.NamespacedName{Namespace: testNamespace, Name: "rad-alpha"}, &appsv1.StatefulSet{}); err == nil {
		t.Fatal("Rad StatefulSet exists with JWT authentication on an HTTP route")
	}
	observed := getObject(t, kubernetesClient, client.ObjectKeyFromObject(database), &radv1alpha1.Database{})
	condition := meta.FindStatusCondition(observed.Status.Conditions, radv1alpha1.ConditionAuthenticationReady)
	if condition == nil || condition.Status != metav1.ConditionFalse || condition.Reason != "AuthenticationRequiresHTTPS" {
		t.Fatalf("authentication condition = %#v", condition)
	}
}

func hasNamedContainerPort(ports []corev1.ContainerPort, name string) bool {
	for _, port := range ports {
		if port.Name == name {
			return true
		}
	}
	return false
}

func assertDatabaseAuthenticationWorkloads(
	t *testing.T,
	kubernetesClient client.Client,
	databaseName string,
	authentication *radv1alpha1.JWTAuthentication,
) {
	t.Helper()
	statefulSet := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: resourceName(databaseName)}, &appsv1.StatefulSet{})
	deployment := getObject(t, kubernetesClient, types.NamespacedName{Namespace: testNamespace, Name: readerResourceName(databaseName)}, &appsv1.Deployment{})
	assertClientAuthenticationEnvironment(t, databaseName+" writer", statefulSet.Spec.Template.Spec.Containers[0].Env, authentication)
	assertClientAuthenticationEnvironment(t, databaseName+" reader", deployment.Spec.Template.Spec.Containers[0].Env, authentication)
}

func assertClientAuthenticationEnvironment(
	t *testing.T,
	workload string,
	environment []corev1.EnvVar,
	authentication *radv1alpha1.JWTAuthentication,
) {
	t.Helper()
	want := map[string]string{
		"RAD_ADMIN_ADDR":    "127.0.0.1:7238",
		"RAD_AUTH":          "jwt",
		"RAD_AUTH_ISSUER":   authentication.Issuer,
		"RAD_AUTH_AUDIENCE": authentication.Audience,
		"RAD_AUTH_PROFILE":  string(authentication.EffectiveProfile()),
	}
	if authentication.JWKSURL != "" {
		want["RAD_AUTH_JWKS_URL"] = authentication.JWKSURL
	}
	for name, scopes := range map[string][]radv1alpha1.OAuthScope{
		"RAD_AUTH_QUERY_SCOPES":   authentication.QueryScopes,
		"RAD_AUTH_MUTATE_SCOPES":  authentication.MutateScopes,
		"RAD_AUTH_CATALOG_SCOPES": authentication.CatalogScopes,
		"RAD_AUTH_ADMIN_SCOPES":   authentication.AdminScopes,
	} {
		if value := scopeEnvironmentValue(scopes); value != "" {
			want[name] = value
		}
	}

	got := map[string]string{}
	for _, variable := range environment {
		if variable.Name == "RAD_ADMIN_ADDR" || strings.HasPrefix(variable.Name, "RAD_AUTH") {
			got[variable.Name] = variable.Value
		}
	}
	if !maps.Equal(got, want) {
		t.Errorf("%s authentication environment = %#v, want %#v", workload, got, want)
	}
}
