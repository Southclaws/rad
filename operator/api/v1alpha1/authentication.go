package v1alpha1

import (
	"fmt"
	"net/url"
)

// OAuthScope is one value in an OAuth scope list.
// +kubebuilder:validation:MinLength=1
// +kubebuilder:validation:Pattern=`^[\x21\x23-\x5B\x5D-\x7E]+$`
type OAuthScope string

// JWTProfile selects a JWT access-token validation profile.
type JWTProfile string

const (
	// JWTProfileRFC9068 requires the RFC 9068 access-token header and claims.
	JWTProfileRFC9068 JWTProfile = "rfc9068"
	// JWTProfileCompatible accepts conventional JWT access tokens.
	JWTProfileCompatible JWTProfile = "compatible"
)

// JWTAuthentication configures OAuth JWT access-token validation and scope
// authorization for one database.
// +kubebuilder:validation:XValidation:rule="(has(self.queryScopes) && size(self.queryScopes) > 0) || (has(self.mutateScopes) && size(self.mutateScopes) > 0) || (has(self.catalogScopes) && size(self.catalogScopes) > 0)",message="at least one non-empty scope list is required"
type JWTAuthentication struct {
	// Issuer is the exact trusted JWT issuer.
	// +kubebuilder:validation:MinLength=1
	// +kubebuilder:validation:Pattern=`^https://`
	Issuer string `json:"issuer"`

	// Audience identifies the Rad deployment in the JWT audience claim.
	// +kubebuilder:validation:MinLength=1
	Audience string `json:"audience"`

	// JWKSURL overrides OIDC discovery from the issuer.
	// +optional
	// +kubebuilder:validation:Pattern=`^https://`
	JWKSURL string `json:"jwksURL,omitempty"`

	// Profile selects the JWT access-token validation profile.
	// +optional
	// +kubebuilder:default=rfc9068
	// +kubebuilder:validation:Enum=rfc9068;compatible
	Profile JWTProfile `json:"profile,omitempty"`

	// QueryScopes contains the scopes that grant query access.
	// +optional
	// +listType=set
	// +kubebuilder:validation:MaxItems=64
	QueryScopes []OAuthScope `json:"queryScopes,omitempty"`

	// MutateScopes contains the scopes that grant data mutation access.
	// +optional
	// +listType=set
	// +kubebuilder:validation:MaxItems=64
	MutateScopes []OAuthScope `json:"mutateScopes,omitempty"`

	// CatalogScopes contains the scopes that grant catalog mutation access.
	// +optional
	// +listType=set
	// +kubebuilder:validation:MaxItems=64
	CatalogScopes []OAuthScope `json:"catalogScopes,omitempty"`
}

// Validate checks the constraints that the Rad process applies at startup.
func (authentication *JWTAuthentication) Validate() error {
	if authentication == nil {
		return nil
	}
	if err := validateAuthenticationURL("issuer", authentication.Issuer, false); err != nil {
		return err
	}
	if authentication.Audience == "" {
		return fmt.Errorf("audience is required")
	}
	if authentication.JWKSURL != "" {
		if err := validateAuthenticationURL("JWKS URL", authentication.JWKSURL, true); err != nil {
			return err
		}
	}
	if profile := authentication.EffectiveProfile(); profile != JWTProfileRFC9068 && profile != JWTProfileCompatible {
		return fmt.Errorf("profile must be rfc9068 or compatible")
	}
	if len(authentication.QueryScopes) == 0 && len(authentication.MutateScopes) == 0 && len(authentication.CatalogScopes) == 0 {
		return fmt.Errorf("at least one scope list is required")
	}
	for name, scopes := range map[string][]OAuthScope{
		"query scopes":   authentication.QueryScopes,
		"mutate scopes":  authentication.MutateScopes,
		"catalog scopes": authentication.CatalogScopes,
	} {
		if len(scopes) > 64 {
			return fmt.Errorf("%s must contain at most 64 values", name)
		}
		seen := make(map[OAuthScope]struct{}, len(scopes))
		for _, scope := range scopes {
			if !validOAuthScope(scope) {
				return fmt.Errorf("%s contains an invalid OAuth scope", name)
			}
			if _, exists := seen[scope]; exists {
				return fmt.Errorf("%s contains a duplicate OAuth scope", name)
			}
			seen[scope] = struct{}{}
		}
	}
	return nil
}

// EffectiveProfile returns the configured profile or the secure default.
func (authentication *JWTAuthentication) EffectiveProfile() JWTProfile {
	if authentication == nil || authentication.Profile == "" {
		return JWTProfileRFC9068
	}
	return authentication.Profile
}

func validateAuthenticationURL(name, value string, allowQuery bool) error {
	parsed, err := url.Parse(value)
	if err != nil {
		return fmt.Errorf("invalid %s: %w", name, err)
	}
	if parsed.Scheme != "https" {
		return fmt.Errorf("%s must use HTTPS", name)
	}
	if parsed.Hostname() == "" {
		return fmt.Errorf("%s must contain a host", name)
	}
	if parsed.User != nil {
		return fmt.Errorf("%s must not contain credentials", name)
	}
	if parsed.Fragment != "" {
		return fmt.Errorf("%s must not contain a fragment", name)
	}
	if !allowQuery && parsed.RawQuery != "" {
		return fmt.Errorf("%s must not contain a query", name)
	}
	return nil
}

func validOAuthScope(scope OAuthScope) bool {
	if scope == "" {
		return false
	}
	for _, character := range []byte(scope) {
		if character != 0x21 && (character < 0x23 || character > 0x5b) && (character < 0x5d || character > 0x7e) {
			return false
		}
	}
	return true
}
