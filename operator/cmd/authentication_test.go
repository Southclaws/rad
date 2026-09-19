package main

import "testing"

func TestAuthenticationFromValuesRequiresExplicitJWTSettings(t *testing.T) {
	_, err := authenticationFromValues(
		"jwt",
		"https://auth.example.com/",
		"rad-production",
		"",
		"",
		"",
		"",
		"",
		"",
	)
	if err == nil {
		t.Fatal("JWT mode accepted no scope settings")
	}
}

func TestAuthenticationFromValuesParsesOAuthScopeLists(t *testing.T) {
	authentication, err := authenticationFromValues(
		"jwt",
		"https://auth.example.com/",
		"rad-production",
		"https://auth.example.com/keys",
		"",
		"rad:read rad:admin",
		"rad:write rad:admin",
		"rad:catalog rad:admin",
		"rad:admin",
	)
	if err != nil {
		t.Fatal(err)
	}
	if len(authentication.QueryScopes) != 2 || authentication.QueryScopes[1] != "rad:admin" {
		t.Fatalf("query scopes = %v", authentication.QueryScopes)
	}
	if len(authentication.AdminScopes) != 1 || authentication.AdminScopes[0] != "rad:admin" {
		t.Fatalf("admin scopes = %v", authentication.AdminScopes)
	}
	if authentication.EffectiveProfile() != "rfc9068" {
		t.Fatalf("profile = %q", authentication.EffectiveProfile())
	}
}

func TestAuthenticationFromValuesRejectsJWTSettingsInNoneMode(t *testing.T) {
	_, err := authenticationFromValues("none", "https://auth.example.com/", "", "", "", "", "", "", "")
	if err == nil {
		t.Fatal("none mode accepted JWT settings")
	}
}

func TestAuthenticationFromValuesRejectsInvalidScopeSpacing(t *testing.T) {
	_, err := authenticationFromValues(
		"jwt",
		"https://auth.example.com/",
		"rad-production",
		"",
		"",
		"rad:read  rad:admin",
		"",
		"",
		"",
	)
	if err == nil {
		t.Fatal("JWT mode accepted invalid scope spacing")
	}
}

func TestAuthenticationFromValuesAcceptsTheCompatibleProfile(t *testing.T) {
	authentication, err := authenticationFromValues(
		"jwt",
		"https://auth.example.com/",
		"rad-production",
		"",
		"compatible",
		"rad:read",
		"",
		"",
		"",
	)
	if err != nil {
		t.Fatal(err)
	}
	if authentication.Profile != "compatible" {
		t.Fatalf("profile = %q", authentication.Profile)
	}
}

func TestAuthenticationFromValuesAcceptsTheCloudflareAccessProfile(t *testing.T) {
	authentication, err := authenticationFromValues(
		"jwt",
		"https://team.cloudflareaccess.com",
		"application-audience",
		"https://team.cloudflareaccess.com/cdn-cgi/access/certs",
		"cloudflare-access",
		"authenticated",
		"authenticated",
		"authenticated",
		"authenticated",
	)
	if err != nil {
		t.Fatal(err)
	}
	if authentication.Profile != "cloudflare-access" {
		t.Fatalf("profile = %q", authentication.Profile)
	}
}

func TestAuthenticationFromValuesRejectsAnUnknownProfile(t *testing.T) {
	_, err := authenticationFromValues(
		"jwt",
		"https://auth.example.com/",
		"rad-production",
		"",
		"simple",
		"rad:read",
		"",
		"",
		"",
	)
	if err == nil {
		t.Fatal("JWT mode accepted an unknown profile")
	}
}
