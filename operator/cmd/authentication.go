package main

import (
	"fmt"
	"strings"

	radv1alpha1 "github.com/Southclaws/rad/operator/api/v1alpha1"
)

func authenticationFromValues(
	mode string,
	issuer string,
	audience string,
	jwksURL string,
	profile string,
	queryScopes string,
	mutateScopes string,
	catalogScopes string,
	adminScopes string,
) (*radv1alpha1.JWTAuthentication, error) {
	mode = strings.TrimSpace(mode)
	switch mode {
	case "none":
		if issuer != "" || audience != "" || jwksURL != "" || profile != "" || queryScopes != "" || mutateScopes != "" || catalogScopes != "" || adminScopes != "" {
			return nil, fmt.Errorf("JWT settings require auth mode jwt")
		}
		return nil, nil
	case "jwt":
		query, err := scopesFromValue(queryScopes, "auth-query-scopes")
		if err != nil {
			return nil, err
		}
		mutate, err := scopesFromValue(mutateScopes, "auth-mutate-scopes")
		if err != nil {
			return nil, err
		}
		catalog, err := scopesFromValue(catalogScopes, "auth-catalog-scopes")
		if err != nil {
			return nil, err
		}
		admin, err := scopesFromValue(adminScopes, "auth-admin-scopes")
		if err != nil {
			return nil, err
		}
		authentication := &radv1alpha1.JWTAuthentication{
			Issuer:        issuer,
			Audience:      audience,
			JWKSURL:       jwksURL,
			Profile:       radv1alpha1.JWTProfile(profile),
			QueryScopes:   query,
			MutateScopes:  mutate,
			CatalogScopes: catalog,
			AdminScopes:   admin,
		}
		if err := authentication.Validate(); err != nil {
			return nil, fmt.Errorf("invalid operator-wide authentication: %w", err)
		}
		return authentication, nil
	default:
		return nil, fmt.Errorf("unknown auth mode %q (none or jwt)", mode)
	}
}

func scopesFromValue(value, name string) ([]radv1alpha1.OAuthScope, error) {
	if value == "" {
		return nil, nil
	}
	parts := strings.Split(value, " ")
	scopes := make([]radv1alpha1.OAuthScope, len(parts))
	for index, part := range parts {
		scopes[index] = radv1alpha1.OAuthScope(part)
	}
	authentication := &radv1alpha1.JWTAuthentication{
		Issuer:      "https://validation.invalid",
		Audience:    "validation",
		QueryScopes: scopes,
	}
	if err := authentication.Validate(); err != nil {
		return nil, fmt.Errorf("%s must be a space-separated OAuth scope list", name)
	}
	return scopes, nil
}
