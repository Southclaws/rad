package v1alpha1

import "testing"

func TestJWTAuthenticationValidation(t *testing.T) {
	valid := JWTAuthentication{
		Issuer:       "https://auth.example.com/tenant",
		Audience:     "rad-production",
		JWKSURL:      "https://auth.example.com/keys?set=rad",
		Profile:      JWTProfileCompatible,
		QueryScopes:  []OAuthScope{"rad:read", "rad:admin"},
		MutateScopes: []OAuthScope{"rad:write"},
	}
	if err := valid.Validate(); err != nil {
		t.Fatalf("valid authentication returned %v", err)
	}
	cloudflareAccess := JWTAuthentication{
		Issuer:       "https://team.cloudflareaccess.com",
		Audience:     "application-audience",
		JWKSURL:      "https://team.cloudflareaccess.com/cdn-cgi/access/certs",
		Profile:      JWTProfileCloudflareAccess,
		QueryScopes:  []OAuthScope{"authenticated"},
		MutateScopes: []OAuthScope{"authenticated"},
	}
	if err := cloudflareAccess.Validate(); err != nil {
		t.Fatalf("valid Cloudflare Access authentication returned %v", err)
	}

	tests := map[string]JWTAuthentication{
		"HTTP issuer": {
			Issuer: "http://auth.example.com", Audience: "rad", QueryScopes: []OAuthScope{"rad:read"},
		},
		"issuer credentials": {
			Issuer: "https://user:pass@auth.example.com", Audience: "rad", QueryScopes: []OAuthScope{"rad:read"},
		},
		"issuer fragment": {
			Issuer: "https://auth.example.com/#fragment", Audience: "rad", QueryScopes: []OAuthScope{"rad:read"},
		},
		"issuer query": {
			Issuer: "https://auth.example.com/?tenant=one", Audience: "rad", QueryScopes: []OAuthScope{"rad:read"},
		},
		"empty audience": {
			Issuer: "https://auth.example.com", QueryScopes: []OAuthScope{"rad:read"},
		},
		"no scopes": {
			Issuer: "https://auth.example.com", Audience: "rad",
		},
		"invalid scope": {
			Issuer: "https://auth.example.com", Audience: "rad", QueryScopes: []OAuthScope{"rad read"},
		},
		"duplicate scope": {
			Issuer: "https://auth.example.com", Audience: "rad", QueryScopes: []OAuthScope{"rad:read", "rad:read"},
		},
		"HTTP JWKS URL": {
			Issuer: "https://auth.example.com", Audience: "rad", JWKSURL: "http://auth.example.com/keys", QueryScopes: []OAuthScope{"rad:read"},
		},
		"unknown profile": {
			Issuer: "https://auth.example.com", Audience: "rad", Profile: "simple", QueryScopes: []OAuthScope{"rad:read"},
		},
		"Cloudflare Access OAuth scope": {
			Issuer: "https://team.cloudflareaccess.com", Audience: "rad", Profile: JWTProfileCloudflareAccess, QueryScopes: []OAuthScope{"rad:read"},
		},
	}
	for name, authentication := range tests {
		t.Run(name, func(t *testing.T) {
			if err := authentication.Validate(); err == nil {
				t.Fatal("invalid authentication passed validation")
			}
		})
	}
}

func TestJWTAuthenticationDefaultsToRFC9068(t *testing.T) {
	authentication := JWTAuthentication{}

	if authentication.EffectiveProfile() != JWTProfileRFC9068 {
		t.Fatalf("profile = %q", authentication.EffectiveProfile())
	}
}
