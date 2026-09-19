use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use jsonwebtoken::jwk::{
    AlgorithmParameters, EllipticCurve, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse,
};
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header, get_current_timestamp,
};
use reqwest::{Client, Url};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::Mutex;

const DISCOVERY_LIMIT: usize = 64 * 1024;
const JWKS_LIMIT: usize = 1024 * 1024;
const KEY_LIMIT: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const REFRESH_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const CLOCK_ALLOWANCE_SECONDS: u64 = 60;
const CLOUDFLARE_ACCESS_AUTHORIZATION_VALUE: &str = "authenticated";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum AuthConfig {
    #[default]
    None,
    Jwt(Box<JwtConfig>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JwtConfig {
    pub issuer: String,
    pub audience: String,
    pub jwks_url: Option<String>,
    pub profile: JwtProfile,
    pub scopes: ScopeConfig,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JwtProfile {
    #[default]
    Rfc9068,
    Compatible,
    CloudflareAccess,
}

impl FromStr for JwtProfile {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "rfc9068" => Ok(Self::Rfc9068),
            "compatible" => Ok(Self::Compatible),
            "cloudflare-access" => Ok(Self::CloudflareAccess),
            value => Err(Error::Configuration(format!(
                "unknown auth profile {value:?} (rfc9068, compatible, or cloudflare-access)"
            ))),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScopeConfig {
    query: HashSet<String>,
    mutate: HashSet<String>,
    catalog: HashSet<String>,
    admin: HashSet<String>,
}

impl ScopeConfig {
    pub fn from_values(
        query: Option<String>,
        mutate: Option<String>,
        catalog: Option<String>,
    ) -> Result<Self, Error> {
        Self::from_values_with_admin(query, mutate, catalog, None)
    }

    pub fn from_values_with_admin(
        query: Option<String>,
        mutate: Option<String>,
        catalog: Option<String>,
        admin: Option<String>,
    ) -> Result<Self, Error> {
        Ok(Self {
            query: configured_scopes(query, "auth query scopes")?,
            mutate: configured_scopes(mutate, "auth mutate scopes")?,
            catalog: configured_scopes(catalog, "auth catalog scopes")?,
            admin: configured_scopes(admin, "auth admin scopes")?,
        })
    }

    pub fn is_enabled(&self) -> bool {
        !self.query.is_empty()
            || !self.mutate.is_empty()
            || !self.catalog.is_empty()
            || !self.admin.is_empty()
    }

    fn execution_policy(&self, scopes: &HashSet<String>) -> crate::engine::exec::ExecutionPolicy {
        self.execution_policy_where(|accepted| !scopes.is_disjoint(accepted))
    }

    fn execution_policy_for_value(&self, value: &str) -> crate::engine::exec::ExecutionPolicy {
        self.execution_policy_where(|accepted| accepted.contains(value))
    }

    fn execution_policy_where(
        &self,
        mut grants: impl FnMut(&HashSet<String>) -> bool,
    ) -> crate::engine::exec::ExecutionPolicy {
        let mut policy = crate::engine::exec::ExecutionPolicy::deny_all();
        for (capability, accepted) in [
            (crate::engine::exec::Capability::Query, &self.query),
            (crate::engine::exec::Capability::Mutate, &self.mutate),
            (crate::engine::exec::Capability::Catalog, &self.catalog),
            (crate::engine::exec::Capability::Admin, &self.admin),
        ] {
            if grants(accepted) {
                policy.allow(capability);
            }
        }
        policy
    }

    fn contains_only(&self, value: &str) -> bool {
        [&self.query, &self.mutate, &self.catalog, &self.admin]
            .into_iter()
            .flatten()
            .all(|configured| configured == value)
    }
}

impl AuthConfig {
    pub fn from_values(
        mode: &str,
        issuer: Option<String>,
        audience: Option<String>,
        jwks_url: Option<String>,
        profile: Option<JwtProfile>,
        scopes: ScopeConfig,
    ) -> Result<Self, Error> {
        match mode {
            "none" => {
                if issuer.is_some()
                    || audience.is_some()
                    || jwks_url.is_some()
                    || profile.is_some()
                    || scopes.is_enabled()
                {
                    return Err(Error::Configuration(
                        "JWT settings require auth mode jwt".into(),
                    ));
                }
                Ok(Self::None)
            }
            "jwt" => {
                let issuer = required_setting(issuer, "auth issuer")?;
                let audience = required_setting(audience, "auth audience")?;
                let config = JwtConfig {
                    issuer,
                    audience,
                    jwks_url,
                    profile: profile.unwrap_or_default(),
                    scopes,
                };
                config.validate()?;
                Ok(Self::Jwt(Box::new(config)))
            }
            value => Err(Error::Configuration(format!(
                "unknown auth mode {value:?} (none or jwt)"
            ))),
        }
    }

    pub fn is_jwt(&self) -> bool {
        matches!(self, Self::Jwt(_))
    }

    pub fn validate(&self) -> Result<(), Error> {
        match self {
            Self::None => Ok(()),
            Self::Jwt(config) => config.validate(),
        }
    }
}

impl JwtConfig {
    pub fn validate(&self) -> Result<(), Error> {
        if self.audience.is_empty() {
            return Err(Error::Configuration("auth audience is required".into()));
        }
        validate_url(&self.issuer, UrlKind::Issuer)?;
        if let Some(jwks_url) = &self.jwks_url {
            validate_url(jwks_url, UrlKind::Jwks)?;
        }
        if !self.scopes.is_enabled() {
            return Err(Error::Configuration(
                "JWT authentication requires at least one auth scope setting".into(),
            ));
        }
        if self.profile == JwtProfile::CloudflareAccess
            && !self
                .scopes
                .contains_only(CLOUDFLARE_ACCESS_AUTHORIZATION_VALUE)
        {
            return Err(Error::Configuration(
                "auth scope settings must contain only \"authenticated\" for profile cloudflare-access"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Principal {
    pub issuer: String,
    pub subject: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedToken {
    pub principal: Principal,
    pub policy: crate::engine::exec::ExecutionPolicy,
}

#[derive(Clone)]
pub struct Authenticator {
    issuer: Arc<str>,
    audience: Arc<str>,
    profile: JwtProfile,
    scopes: ScopeConfig,
    jwks_url: Url,
    http: Client,
    keys: Arc<RwLock<Arc<KeySet>>>,
    refresh: Arc<Mutex<RefreshState>>,
}

impl Authenticator {
    pub async fn load(config: &JwtConfig) -> Result<Self, Error> {
        config.validate()?;
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(Error::HttpClient)?;
        let issuer = Url::parse(&config.issuer)
            .map_err(|error| Error::Configuration(format!("invalid auth issuer: {error}")))?;
        let jwks_url = match &config.jwks_url {
            Some(value) => Url::parse(value)
                .map_err(|error| Error::Configuration(format!("invalid JWKS URL: {error}")))?,
            None => discover_jwks_url(&http, &issuer, &config.issuer).await?,
        };
        let keys = fetch_key_set(&http, &jwks_url).await?;
        Ok(Self {
            issuer: config.issuer.clone().into(),
            audience: config.audience.clone().into(),
            profile: config.profile,
            scopes: config.scopes.clone(),
            jwks_url,
            http,
            keys: Arc::new(RwLock::new(Arc::new(keys))),
            refresh: Arc::new(Mutex::new(RefreshState::default())),
        })
    }

    pub async fn authenticate(&self, token: &str) -> Result<AuthenticatedToken, InvalidToken> {
        let decoded = self.decode_token(token).await?;
        let claims = decoded.claims();
        if claims.iss != self.issuer.as_ref() || !claims.audience.contains(self.audience.as_ref()) {
            return Err(InvalidToken);
        }
        let issuer = claims.iss.clone();
        let validated_times = (claims.expires_at, claims.not_before);
        let (subject, policy) = match decoded {
            ProfileClaims::OAuth(claims) => {
                if claims.sub.is_empty() {
                    return Err(InvalidToken);
                }
                let scopes = claims
                    .scope
                    .as_deref()
                    .map(parse_scope_list)
                    .transpose()
                    .map_err(|_| InvalidToken)?
                    .unwrap_or_default();
                (claims.sub, self.scopes.execution_policy(&scopes))
            }
            ProfileClaims::CloudflareAccess(claims) => {
                let subject = if claims.claims.sub.is_empty() {
                    claims.common_name.filter(|value| !value.is_empty())
                } else {
                    Some(claims.claims.sub)
                }
                .ok_or(InvalidToken)?;
                (
                    subject,
                    self.scopes
                        .execution_policy_for_value(CLOUDFLARE_ACCESS_AUTHORIZATION_VALUE),
                )
            }
        };
        let _validated_times = validated_times;
        Ok(AuthenticatedToken {
            principal: Principal { issuer, subject },
            policy,
        })
    }

    async fn decode_token(&self, token: &str) -> Result<ProfileClaims, InvalidToken> {
        let header = validated_header(token, self.profile)?;
        self.refresh_if_due().await;
        let kid = header.kid.as_deref().ok_or(InvalidToken)?;
        let mut key = self.key(kid, header.alg);
        if key.is_none() {
            self.refresh_for_unknown_key(kid).await;
            key = self.key(kid, header.alg);
        }
        let key = key.ok_or(InvalidToken)?;
        let mut validation = Validation::new(header.alg);
        let required_claims = match self.profile {
            JwtProfile::Rfc9068 => &["exp", "iss", "aud", "sub", "client_id", "iat", "jti"][..],
            JwtProfile::Compatible => &["exp", "iss", "aud", "sub"][..],
            JwtProfile::CloudflareAccess => &["exp", "iss", "aud", "sub", "iat", "type"][..],
        };
        validation.set_required_spec_claims(required_claims);
        validation.set_issuer(&[self.issuer.as_ref()]);
        validation.set_audience(&[self.audience.as_ref()]);
        validation.validate_nbf = true;
        validation.leeway = CLOCK_ALLOWANCE_SECONDS;
        decode_claims(token, &key, &validation, self.profile)
    }

    fn key(&self, kid: &str, algorithm: Algorithm) -> Option<DecodingKey> {
        let keys = self.keys.read().unwrap_or_else(|error| error.into_inner());
        keys.keys
            .get(kid)
            .filter(|key| key.algorithm == algorithm)
            .map(|key| key.decoding.clone())
    }

    async fn refresh_if_due(&self) {
        let due = {
            let keys = self.keys.read().unwrap_or_else(|error| error.into_inner());
            keys.loaded_at.elapsed() >= REFRESH_INTERVAL
        };
        if !due {
            return;
        }
        let mut state = self.refresh.lock().await;
        let still_due = {
            let keys = self.keys.read().unwrap_or_else(|error| error.into_inner());
            keys.loaded_at.elapsed() >= REFRESH_INTERVAL
        };
        if !still_due
            || state
                .last_attempt
                .is_some_and(|last| last.elapsed() < REFRESH_RETRY_INTERVAL)
        {
            return;
        }
        state.last_attempt = Some(Instant::now());
        self.refresh_locked().await;
    }

    async fn refresh_for_unknown_key(&self, kid: &str) {
        let mut state = self.refresh.lock().await;
        if self.key(kid, Algorithm::RS256).is_some()
            || self.key(kid, Algorithm::ES256).is_some()
            || self.key(kid, Algorithm::EdDSA).is_some()
        {
            return;
        }
        if state
            .last_unknown_attempt
            .is_some_and(|last| last.elapsed() < REFRESH_RETRY_INTERVAL)
        {
            return;
        }
        let now = Instant::now();
        state.last_attempt = Some(now);
        state.last_unknown_attempt = Some(now);
        self.refresh_locked().await;
    }

    async fn refresh_locked(&self) {
        match fetch_key_set(&self.http, &self.jwks_url).await {
            Ok(keys) => {
                *self.keys.write().unwrap_or_else(|error| error.into_inner()) = Arc::new(keys);
                crate::telemetry::auth_jwks_refresh("success");
            }
            Err(error) => {
                crate::telemetry::auth_jwks_refresh("error");
                tracing::warn!(
                    target: "rad",
                    event = "auth.jwks_refresh_failed",
                    component = "auth",
                    error = %error,
                    message = "JWKS refresh failed"
                );
            }
        }
    }
}

#[cfg(test)]
impl Authenticator {
    pub(crate) fn from_jwks_for_test(
        issuer: &str,
        audience: &str,
        document: JwkSet,
    ) -> Result<Self, Error> {
        Self::from_jwks_with_scopes_for_test(issuer, audience, document, ScopeConfig::default())
    }

    pub(crate) fn from_jwks_with_scopes_for_test(
        issuer: &str,
        audience: &str,
        document: JwkSet,
        scopes: ScopeConfig,
    ) -> Result<Self, Error> {
        let keys = validate_key_set(document)?;
        Ok(Self {
            issuer: issuer.into(),
            audience: audience.into(),
            profile: JwtProfile::Rfc9068,
            scopes,
            jwks_url: Url::parse("https://auth.example.com/jwks").unwrap(),
            http: Client::new(),
            keys: Arc::new(RwLock::new(Arc::new(keys))),
            refresh: Arc::new(Mutex::new(RefreshState {
                last_attempt: None,
                last_unknown_attempt: Some(Instant::now()),
            })),
        })
    }
}

#[derive(Debug, Error)]
#[error("invalid token")]
pub struct InvalidToken;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid authentication configuration: {0}")]
    Configuration(String),
    #[error("could not build the authentication HTTP client: {0}")]
    HttpClient(reqwest::Error),
    #[error("authentication metadata request failed: {0}")]
    Request(reqwest::Error),
    #[error("authentication metadata returned HTTP {0}")]
    HttpStatus(reqwest::StatusCode),
    #[error("authentication metadata exceeds {limit} bytes")]
    ResponseTooLarge { limit: usize },
    #[error("invalid OIDC discovery document: {0}")]
    Discovery(String),
    #[error("invalid JWKS document: {0}")]
    Jwks(String),
}

#[derive(Default)]
struct RefreshState {
    last_attempt: Option<Instant>,
    last_unknown_attempt: Option<Instant>,
}

#[derive(Debug)]
struct KeySet {
    keys: HashMap<String, VerificationKey>,
    loaded_at: Instant,
}

#[derive(Debug)]
struct VerificationKey {
    algorithm: Algorithm,
    decoding: DecodingKey,
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    #[serde(rename = "aud")]
    audience: Audience,
    #[serde(rename = "exp")]
    expires_at: u64,
    sub: String,
    #[serde(rename = "nbf")]
    not_before: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_scope")]
    scope: Option<String>,
}

#[derive(Deserialize)]
struct Rfc9068Claims {
    #[serde(flatten)]
    claims: Claims,
    client_id: String,
    #[serde(rename = "iat")]
    issued_at: u64,
    #[serde(rename = "jti")]
    token_id: String,
}

#[derive(Deserialize)]
struct CloudflareAccessClaims {
    #[serde(flatten)]
    claims: Claims,
    #[serde(rename = "iat")]
    issued_at: u64,
    #[serde(rename = "type")]
    token_type: String,
    common_name: Option<String>,
}

enum ProfileClaims {
    OAuth(Claims),
    CloudflareAccess(CloudflareAccessClaims),
}

impl ProfileClaims {
    fn claims(&self) -> &Claims {
        match self {
            Self::OAuth(claims) => claims,
            Self::CloudflareAccess(claims) => &claims.claims,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, expected: &str) -> bool {
        match self {
            Self::One(value) => value == expected,
            Self::Many(values) => values.iter().any(|value| value == expected),
        }
    }
}

#[derive(Deserialize)]
struct DiscoveryDocument {
    issuer: String,
    jwks_uri: String,
}

#[derive(Clone, Copy)]
enum UrlKind {
    Issuer,
    Jwks,
}

fn required_setting(value: Option<String>, name: &str) -> Result<String, Error> {
    value
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Configuration(format!("{name} is required for auth mode jwt")))
}

fn configured_scopes(value: Option<String>, name: &str) -> Result<HashSet<String>, Error> {
    value.map_or_else(
        || Ok(HashSet::new()),
        |value| {
            parse_scope_list(&value).map_err(|()| {
                Error::Configuration(format!("{name} must be a space-separated OAuth scope list"))
            })
        },
    )
}

fn parse_scope_list(value: &str) -> Result<HashSet<String>, ()> {
    if value.is_empty() {
        return Err(());
    }
    value
        .split(' ')
        .map(|scope| {
            if scope.is_empty()
                || !scope.bytes().all(|byte| {
                    byte == 0x21 || (0x23..=0x5b).contains(&byte) || (0x5d..=0x7e).contains(&byte)
                })
            {
                return Err(());
            }
            Ok(scope.to_owned())
        })
        .collect()
}

fn deserialize_scope<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

fn validate_url(value: &str, kind: UrlKind) -> Result<Url, Error> {
    let label = match kind {
        UrlKind::Issuer => "auth issuer",
        UrlKind::Jwks => "JWKS URL",
    };
    let url = Url::parse(value)
        .map_err(|error| Error::Configuration(format!("invalid {label}: {error}")))?;
    if url.scheme() != "https" {
        return Err(Error::Configuration(format!("{label} must use HTTPS")));
    }
    if url.host_str().is_none() {
        return Err(Error::Configuration(format!("{label} must contain a host")));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Configuration(format!(
            "{label} must not contain credentials"
        )));
    }
    if url.fragment().is_some() {
        return Err(Error::Configuration(format!(
            "{label} must not contain a fragment"
        )));
    }
    if matches!(kind, UrlKind::Issuer) && url.query().is_some() {
        return Err(Error::Configuration(
            "auth issuer must not contain a query".into(),
        ));
    }
    Ok(url)
}

fn discovery_url(issuer: &Url) -> Result<Url, Error> {
    let value = format!(
        "{}/.well-known/openid-configuration",
        issuer.as_str().trim_end_matches('/')
    );
    Url::parse(&value)
        .map_err(|error| Error::Configuration(format!("invalid discovery URL: {error}")))
}

async fn discover_jwks_url(http: &Client, issuer: &Url, exact_issuer: &str) -> Result<Url, Error> {
    let document: DiscoveryDocument =
        serde_json::from_slice(&get_bounded(http, discovery_url(issuer)?, DISCOVERY_LIMIT).await?)
            .map_err(|error| Error::Discovery(error.to_string()))?;
    if document.issuer != exact_issuer {
        return Err(Error::Discovery(
            "the discovery issuer does not match the configured issuer".into(),
        ));
    }
    validate_url(&document.jwks_uri, UrlKind::Jwks)
}

async fn fetch_key_set(http: &Client, jwks_url: &Url) -> Result<KeySet, Error> {
    let bytes = get_bounded(http, jwks_url.clone(), JWKS_LIMIT).await?;
    let document: JwkSet =
        serde_json::from_slice(&bytes).map_err(|error| Error::Jwks(error.to_string()))?;
    validate_key_set(document)
}

async fn get_bounded(http: &Client, url: Url, limit: usize) -> Result<Vec<u8>, Error> {
    let response = http.get(url).send().await.map_err(Error::Request)?;
    if !response.status().is_success() {
        return Err(Error::HttpStatus(response.status()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(Error::ResponseTooLarge { limit });
    }
    let mut output = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(Error::Request)?;
        let next_length = output
            .len()
            .checked_add(chunk.len())
            .ok_or(Error::ResponseTooLarge { limit })?;
        if next_length > limit {
            return Err(Error::ResponseTooLarge { limit });
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

fn validate_key_set(document: JwkSet) -> Result<KeySet, Error> {
    let mut seen = HashSet::new();
    let mut keys = HashMap::new();
    for jwk in document.keys {
        let Some(kid) = jwk.common.key_id.as_deref() else {
            continue;
        };
        if kid.is_empty() {
            continue;
        }
        if !seen.insert(kid.to_owned()) {
            return Err(Error::Jwks(format!("duplicate key ID {kid:?}")));
        }
        if seen.len() > KEY_LIMIT {
            return Err(Error::Jwks(format!(
                "JWKS contains more than {KEY_LIMIT} key IDs"
            )));
        }
        if !key_allows_verification(&jwk.common.public_key_use, &jwk.common.key_operations) {
            continue;
        }
        let Some(algorithm) = key_algorithm(&jwk.algorithm) else {
            continue;
        };
        if !jwk_algorithm_matches(jwk.common.key_algorithm, algorithm) {
            continue;
        }
        let decoding = DecodingKey::from_jwk(&jwk)
            .map_err(|error| Error::Jwks(format!("invalid key {kid:?}: {error}")))?;
        keys.insert(
            kid.to_owned(),
            VerificationKey {
                algorithm,
                decoding,
            },
        );
    }
    if keys.is_empty() {
        return Err(Error::Jwks(
            "JWKS contains no supported signature key".into(),
        ));
    }
    Ok(KeySet {
        keys,
        loaded_at: Instant::now(),
    })
}

fn key_allows_verification(
    public_key_use: &Option<PublicKeyUse>,
    key_operations: &Option<Vec<KeyOperations>>,
) -> bool {
    if public_key_use
        .as_ref()
        .is_some_and(|usage| usage != &PublicKeyUse::Signature)
    {
        return false;
    }
    key_operations
        .as_ref()
        .is_none_or(|operations| operations.contains(&KeyOperations::Verify))
}

fn key_algorithm(parameters: &AlgorithmParameters) -> Option<Algorithm> {
    match parameters {
        AlgorithmParameters::RSA(_) => Some(Algorithm::RS256),
        AlgorithmParameters::EllipticCurve(parameters)
            if parameters.curve == EllipticCurve::P256 =>
        {
            Some(Algorithm::ES256)
        }
        AlgorithmParameters::OctetKeyPair(parameters)
            if parameters.curve == EllipticCurve::Ed25519 =>
        {
            Some(Algorithm::EdDSA)
        }
        _ => None,
    }
}

fn jwk_algorithm_matches(value: Option<KeyAlgorithm>, expected: Algorithm) -> bool {
    value.is_none_or(|value| {
        matches!(
            (value, expected),
            (KeyAlgorithm::RS256, Algorithm::RS256)
                | (KeyAlgorithm::ES256, Algorithm::ES256)
                | (KeyAlgorithm::EdDSA, Algorithm::EdDSA)
        )
    })
}

fn decode_claims(
    token: &str,
    key: &DecodingKey,
    validation: &Validation,
    profile: JwtProfile,
) -> Result<ProfileClaims, InvalidToken> {
    match profile {
        JwtProfile::Compatible => decode::<Claims>(token, key, validation)
            .map(|token| ProfileClaims::OAuth(token.claims))
            .map_err(|_| InvalidToken),
        JwtProfile::Rfc9068 => {
            let token =
                decode::<Rfc9068Claims>(token, key, validation).map_err(|_| InvalidToken)?;
            if token.claims.client_id.is_empty()
                || token.claims.token_id.is_empty()
                || token.claims.issued_at
                    > get_current_timestamp().saturating_add(CLOCK_ALLOWANCE_SECONDS)
            {
                return Err(InvalidToken);
            }
            Ok(ProfileClaims::OAuth(token.claims.claims))
        }
        JwtProfile::CloudflareAccess => {
            let token = decode::<CloudflareAccessClaims>(token, key, validation)
                .map_err(|_| InvalidToken)?;
            if token.claims.token_type != "app"
                || token.claims.issued_at
                    > get_current_timestamp().saturating_add(CLOCK_ALLOWANCE_SECONDS)
            {
                return Err(InvalidToken);
            }
            Ok(ProfileClaims::CloudflareAccess(token.claims))
        }
    }
}

fn validated_header(
    token: &str,
    profile: JwtProfile,
) -> Result<jsonwebtoken::Header, InvalidToken> {
    if token.split('.').count() != 3 {
        return Err(InvalidToken);
    }
    let header = decode_header(token).map_err(|_| InvalidToken)?;
    if !matches!(
        header.alg,
        Algorithm::RS256 | Algorithm::ES256 | Algorithm::EdDSA
    ) || header.kid.as_deref().is_none_or(str::is_empty)
        || header.jku.is_some()
        || header.x5u.is_some()
        || header.jwk.is_some()
        || header.enc.is_some()
        || header.crit.is_some()
    {
        return Err(InvalidToken);
    }
    if profile == JwtProfile::CloudflareAccess && header.alg != Algorithm::RS256 {
        return Err(InvalidToken);
    }
    let valid_type = match (profile, header.typ.as_deref()) {
        (JwtProfile::Rfc9068, Some(value)) => ["at+jwt", "application/at+jwt"]
            .iter()
            .any(|allowed| value.eq_ignore_ascii_case(allowed)),
        (JwtProfile::Rfc9068, None) => false,
        (JwtProfile::Compatible, Some(value)) => ["JWT", "at+jwt", "application/at+jwt"]
            .iter()
            .any(|allowed| value.eq_ignore_ascii_case(allowed)),
        (JwtProfile::Compatible | JwtProfile::CloudflareAccess, None) => true,
        (JwtProfile::CloudflareAccess, Some(value)) => value.eq_ignore_ascii_case("JWT"),
    };
    if !valid_type {
        return Err(InvalidToken);
    }
    Ok(header)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex, OnceLock};
    use std::time::Duration;

    use axum::Router;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use jsonwebtoken::jwk::Jwk;
    use jsonwebtoken::jwk::JwkSet;
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode, get_current_timestamp};
    use rcgen::{KeyPair, PKCS_ECDSA_P256_SHA256};
    use rsa::pkcs1::EncodeRsaPrivateKey as _;
    use serde_json::json;
    use url::Url;

    use super::{
        AuthConfig, Authenticator, Error, JwtProfile, REFRESH_INTERVAL, RefreshState, ScopeConfig,
        discover_jwks_url, discovery_url, fetch_key_set, get_bounded, validate_key_set,
    };

    const ISSUER: &str = "https://auth.example.com";
    const AUDIENCE: &str = "rad-production";

    #[derive(Clone)]
    struct TestKey {
        encoding: EncodingKey,
        jwk: Jwk,
    }

    #[derive(Clone)]
    struct MetadataState {
        response: Arc<StdMutex<(StatusCode, String)>>,
        requests: Arc<AtomicUsize>,
        delay: Duration,
    }

    async fn metadata_response(State(state): State<MetadataState>) -> Response {
        state.requests.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(state.delay).await;
        let (status, body) = state.response.lock().unwrap().clone();
        (status, body).into_response()
    }

    async fn metadata_server(
        status: StatusCode,
        body: String,
        delay: Duration,
    ) -> (Url, MetadataState, tokio::task::JoinHandle<()>) {
        let state = MetadataState {
            response: Arc::new(StdMutex::new((status, body))),
            requests: Arc::new(AtomicUsize::new(0)),
            delay,
        };
        let app = Router::new()
            .route("/metadata", get(metadata_response))
            .fallback(metadata_response)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (
            Url::parse(&format!("http://{address}/metadata")).unwrap(),
            state,
            task,
        )
    }

    fn metadata_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    fn generated_test_key(algorithm: Algorithm, kid: &str) -> TestKey {
        let encoding = match algorithm {
            Algorithm::RS256 => {
                let key = rsa::RsaPrivateKey::new(&mut rand_08::thread_rng(), 2048).unwrap();
                let der = key.to_pkcs1_der().unwrap();
                EncodingKey::from_rsa_der(der.as_bytes())
            }
            Algorithm::ES256 => {
                let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
                EncodingKey::from_ec_der(&key.serialize_der())
            }
            Algorithm::EdDSA => {
                let mut der = vec![
                    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04,
                    0x22, 0x04, 0x20,
                ];
                der.extend_from_slice(&[7; 32]);
                EncodingKey::from_ed_der(&der)
            }
            _ => panic!("unsupported test algorithm"),
        };
        let mut jwk = Jwk::from_encoding_key(&encoding, algorithm).unwrap();
        jwk.common.key_id = Some(kid.to_owned());
        TestKey { encoding, jwk }
    }

    fn test_key(algorithm: Algorithm) -> &'static TestKey {
        static RSA: OnceLock<TestKey> = OnceLock::new();
        static EC: OnceLock<TestKey> = OnceLock::new();
        static ED: OnceLock<TestKey> = OnceLock::new();
        let cell = match algorithm {
            Algorithm::RS256 => &RSA,
            Algorithm::ES256 => &EC,
            Algorithm::EdDSA => &ED,
            _ => panic!("unsupported test algorithm"),
        };
        cell.get_or_init(|| generated_test_key(algorithm, &format!("{algorithm:?}-key")))
    }

    fn claims(audience: serde_json::Value) -> serde_json::Value {
        json!({
            "iss": ISSUER,
            "aud": audience,
            "exp": get_current_timestamp() + 3600,
            "sub": "principal-1",
            "client_id": "rad-client",
            "iat": get_current_timestamp(),
            "jti": "token-1"
        })
    }

    fn signed_token(
        algorithm: Algorithm,
        claims: &serde_json::Value,
        change_header: impl FnOnce(&mut Header),
    ) -> String {
        let key = test_key(algorithm);
        let mut header = Header::new(algorithm);
        header.kid.clone_from(&key.jwk.common.key_id);
        header.typ = Some("at+jwt".into());
        change_header(&mut header);
        encode(&header, claims, &key.encoding).unwrap()
    }

    fn signed_token_with_key(key: &TestKey, claims: &serde_json::Value) -> String {
        let algorithm = match &key.jwk.algorithm {
            jsonwebtoken::jwk::AlgorithmParameters::RSA(_) => Algorithm::RS256,
            jsonwebtoken::jwk::AlgorithmParameters::EllipticCurve(_) => Algorithm::ES256,
            jsonwebtoken::jwk::AlgorithmParameters::OctetKeyPair(_) => Algorithm::EdDSA,
            _ => panic!("unsupported test key"),
        };
        let mut header = Header::new(algorithm);
        header.kid.clone_from(&key.jwk.common.key_id);
        header.typ = Some("at+jwt".into());
        encode(&header, claims, &key.encoding).unwrap()
    }

    fn authenticator(algorithm: Algorithm) -> Authenticator {
        Authenticator::from_jwks_for_test(
            ISSUER,
            AUDIENCE,
            JwkSet {
                keys: vec![test_key(algorithm).jwk.clone()],
            },
        )
        .unwrap()
    }

    fn configured_authenticator(
        algorithm: Algorithm,
        profile: JwtProfile,
        scopes: ScopeConfig,
    ) -> Authenticator {
        let mut authenticator = authenticator(algorithm);
        authenticator.profile = profile;
        authenticator.scopes = scopes;
        authenticator
    }

    fn jwt_config(profile: Option<JwtProfile>, scopes: ScopeConfig) -> Box<super::JwtConfig> {
        let config = AuthConfig::from_values(
            "jwt",
            Some("https://auth.example.com".into()),
            Some("rad".into()),
            None,
            profile,
            scopes,
        )
        .unwrap();
        let AuthConfig::Jwt(config) = config else {
            panic!("expected JWT authentication");
        };
        config
    }

    fn allowed_capabilities(
        policy: &crate::engine::exec::ExecutionPolicy,
    ) -> (bool, bool, bool, bool) {
        (
            policy.allows(crate::engine::exec::Capability::Query),
            policy.allows(crate::engine::exec::Capability::Mutate),
            policy.allows(crate::engine::exec::Capability::Catalog),
            policy.allows(crate::engine::exec::Capability::Admin),
        )
    }

    fn refreshing_authenticator(document: JwkSet, jwks_url: Url, stale: bool) -> Authenticator {
        let mut keys = validate_key_set(document).unwrap();
        if stale {
            keys.loaded_at = std::time::Instant::now() - REFRESH_INTERVAL;
        }
        Authenticator {
            issuer: ISSUER.into(),
            audience: AUDIENCE.into(),
            profile: JwtProfile::Rfc9068,
            scopes: ScopeConfig::default(),
            jwks_url,
            http: metadata_client(),
            keys: Arc::new(std::sync::RwLock::new(Arc::new(keys))),
            refresh: Arc::new(tokio::sync::Mutex::new(RefreshState::default())),
        }
    }

    #[tokio::test]
    async fn authenticate_accepts_an_rsa_token() {
        let token = signed_token(Algorithm::RS256, &claims(json!(AUDIENCE)), |_| {});
        let authenticated = authenticator(Algorithm::RS256)
            .authenticate(&token)
            .await
            .unwrap();
        assert_eq!(authenticated.principal.subject, "principal-1");
    }

    #[tokio::test]
    async fn authenticate_accepts_a_p256_token() {
        let token = signed_token(Algorithm::ES256, &claims(json!(AUDIENCE)), |_| {});
        let authenticated = authenticator(Algorithm::ES256)
            .authenticate(&token)
            .await
            .unwrap();
        assert_eq!(authenticated.principal.issuer, ISSUER);
    }

    #[tokio::test]
    async fn authenticate_accepts_an_ed25519_token() {
        let token = signed_token(Algorithm::EdDSA, &claims(json!(AUDIENCE)), |_| {});
        assert!(
            authenticator(Algorithm::EdDSA)
                .authenticate(&token)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn authenticate_accepts_an_array_audience() {
        let token = signed_token(
            Algorithm::RS256,
            &claims(json!(["another-service", AUDIENCE])),
            |_| {},
        );
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn authenticate_maps_exact_scopes_to_capabilities() {
        let mut claims = claims(json!(AUDIENCE));
        claims["scope"] = json!("rad:read rad:admin");
        let token = signed_token(Algorithm::RS256, &claims, |_| {});
        let scopes = ScopeConfig::from_values_with_admin(
            Some("rad:read".into()),
            Some("rad:write".into()),
            Some("rad:catalog rad:admin".into()),
            Some("rad:admin".into()),
        )
        .unwrap();

        let authenticated = configured_authenticator(Algorithm::RS256, JwtProfile::Rfc9068, scopes)
            .authenticate(&token)
            .await
            .unwrap();

        assert_eq!(
            allowed_capabilities(&authenticated.policy),
            (true, false, true, true)
        );
    }

    #[tokio::test]
    async fn authenticate_treats_an_absent_scope_as_no_capabilities() {
        let token = signed_token(Algorithm::RS256, &claims(json!(AUDIENCE)), |_| {});
        let scopes = ScopeConfig::from_values(Some("rad:read".into()), None, None).unwrap();

        let authenticated = configured_authenticator(Algorithm::RS256, JwtProfile::Rfc9068, scopes)
            .authenticate(&token)
            .await
            .unwrap();

        assert!(!authenticated.policy.allows_any());
    }

    #[tokio::test]
    async fn authenticate_denies_all_without_scope_configuration() {
        let token = signed_token(Algorithm::RS256, &claims(json!(AUDIENCE)), |_| {});

        let authenticated = authenticator(Algorithm::RS256)
            .authenticate(&token)
            .await
            .unwrap();

        assert_eq!(
            authenticated.policy,
            crate::engine::exec::ExecutionPolicy::deny_all()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_an_array_scope_claim() {
        let mut claims = claims(json!(AUDIENCE));
        claims["scope"] = json!(["rad:read"]);
        let token = signed_token(Algorithm::RS256, &claims, |_| {});

        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_a_malformed_scope_claim() {
        let mut claims = claims(json!(AUDIENCE));
        claims["scope"] = json!("rad:read  rad:write");
        let token = signed_token(Algorithm::RS256, &claims, |_| {});

        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn compatible_profile_accepts_supported_token_types() {
        for token_type in [
            None,
            Some("JWT"),
            Some("at+jwt"),
            Some("application/at+jwt"),
        ] {
            let token = signed_token(Algorithm::RS256, &claims(json!(AUDIENCE)), |header| {
                header.typ = token_type.map(str::to_owned);
            });
            assert!(
                configured_authenticator(
                    Algorithm::RS256,
                    JwtProfile::Compatible,
                    ScopeConfig::default(),
                )
                .authenticate(&token)
                .await
                .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn cloudflare_access_profile_accepts_an_application_assertion() {
        let mut token_claims = claims(json!([AUDIENCE]));
        token_claims.as_object_mut().unwrap().remove("client_id");
        token_claims.as_object_mut().unwrap().remove("jti");
        token_claims["nbf"] = json!(get_current_timestamp());
        token_claims["type"] = json!("app");
        let token = signed_token(Algorithm::RS256, &token_claims, |header| {
            header.typ = None;
        });
        let scopes = ScopeConfig::from_values_with_admin(
            Some("authenticated".into()),
            Some("authenticated".into()),
            None,
            Some("authenticated".into()),
        )
        .unwrap();

        let authenticated =
            configured_authenticator(Algorithm::RS256, JwtProfile::CloudflareAccess, scopes)
                .authenticate(&token)
                .await
                .unwrap();

        assert_eq!(authenticated.principal.subject, "principal-1");
        assert_eq!(
            allowed_capabilities(&authenticated.policy),
            (true, true, false, true)
        );
    }

    #[tokio::test]
    async fn cloudflare_access_profile_accepts_a_service_assertion() {
        let mut token_claims = claims(json!([AUDIENCE]));
        token_claims.as_object_mut().unwrap().remove("client_id");
        token_claims.as_object_mut().unwrap().remove("jti");
        token_claims["sub"] = json!("");
        token_claims["type"] = json!("app");
        token_claims["common_name"] = json!("service-id.access");
        let token = signed_token(Algorithm::RS256, &token_claims, |header| {
            header.typ = Some("JWT".into());
        });

        let authenticated = configured_authenticator(
            Algorithm::RS256,
            JwtProfile::CloudflareAccess,
            ScopeConfig::from_values(Some("authenticated".into()), None, None).unwrap(),
        )
        .authenticate(&token)
        .await
        .unwrap();

        assert_eq!(authenticated.principal.subject, "service-id.access");
    }

    #[tokio::test]
    async fn cloudflare_access_profile_rejects_invalid_profile_values() {
        for (claim, value) in [
            ("type", json!("org")),
            ("iat", json!(get_current_timestamp() + 300)),
        ] {
            let mut token_claims = claims(json!([AUDIENCE]));
            token_claims["type"] = json!("app");
            token_claims[claim] = value;
            let token = signed_token(Algorithm::RS256, &token_claims, |header| {
                header.typ = Some("JWT".into());
            });

            assert!(
                configured_authenticator(
                    Algorithm::RS256,
                    JwtProfile::CloudflareAccess,
                    ScopeConfig::from_values(Some("authenticated".into()), None, None).unwrap(),
                )
                .authenticate(&token)
                .await
                .is_err(),
                "accepted an invalid {claim} claim"
            );
        }
    }

    #[tokio::test]
    async fn cloudflare_access_profile_requires_rs256() {
        let mut token_claims = claims(json!([AUDIENCE]));
        token_claims["type"] = json!("app");
        let token = signed_token(Algorithm::ES256, &token_claims, |header| {
            header.typ = Some("JWT".into());
        });

        assert!(
            configured_authenticator(
                Algorithm::ES256,
                JwtProfile::CloudflareAccess,
                ScopeConfig::from_values(Some("authenticated".into()), None, None).unwrap(),
            )
            .authenticate(&token)
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn rfc9068_profile_accepts_access_token_types() {
        for token_type in ["at+jwt", "application/at+jwt"] {
            let token = signed_token(Algorithm::RS256, &claims(json!(AUDIENCE)), |header| {
                header.typ = Some(token_type.into());
            });
            assert!(
                authenticator(Algorithm::RS256)
                    .authenticate(&token)
                    .await
                    .is_ok()
            );
        }
    }

    #[tokio::test]
    async fn rfc9068_profile_rejects_an_absent_token_type() {
        let token = signed_token(Algorithm::RS256, &claims(json!(AUDIENCE)), |header| {
            header.typ = None;
        });

        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rfc9068_profile_rejects_the_jwt_token_type() {
        let token = signed_token(Algorithm::RS256, &claims(json!(AUDIENCE)), |header| {
            header.typ = Some("JWT".into());
        });

        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rfc9068_profile_requires_profile_claims() {
        for claim in ["client_id", "iat", "jti"] {
            let mut token_claims = claims(json!(AUDIENCE));
            token_claims.as_object_mut().unwrap().remove(claim);
            let token = signed_token(Algorithm::RS256, &token_claims, |_| {});

            assert!(
                authenticator(Algorithm::RS256)
                    .authenticate(&token)
                    .await
                    .is_err(),
                "accepted a token without {claim}"
            );
        }
    }

    #[tokio::test]
    async fn rfc9068_profile_rejects_empty_identifiers() {
        for claim in ["client_id", "jti"] {
            let mut token_claims = claims(json!(AUDIENCE));
            token_claims[claim] = json!("");
            let token = signed_token(Algorithm::RS256, &token_claims, |_| {});

            assert!(
                authenticator(Algorithm::RS256)
                    .authenticate(&token)
                    .await
                    .is_err(),
                "accepted an empty {claim}"
            );
        }
    }

    #[tokio::test]
    async fn rfc9068_profile_rejects_a_future_issue_time() {
        let mut token_claims = claims(json!(AUDIENCE));
        token_claims["iat"] = json!(get_current_timestamp() + 300);
        let token = signed_token(Algorithm::RS256, &token_claims, |_| {});

        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn compatible_profile_does_not_require_rfc9068_fields() {
        let mut token_claims = claims(json!(AUDIENCE));
        for claim in ["client_id", "iat", "jti"] {
            token_claims.as_object_mut().unwrap().remove(claim);
        }
        let token = signed_token(Algorithm::RS256, &token_claims, |header| {
            header.typ = None;
        });

        assert!(
            configured_authenticator(
                Algorithm::RS256,
                JwtProfile::Compatible,
                ScopeConfig::default(),
            )
            .authenticate(&token)
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_a_wrong_issuer() {
        let mut claims = claims(json!(AUDIENCE));
        claims["iss"] = json!("https://other.example.com");
        let token = signed_token(Algorithm::RS256, &claims, |_| {});
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_a_wrong_audience() {
        let token = signed_token(Algorithm::RS256, &claims(json!("other-service")), |_| {});
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_an_expired_token() {
        let mut claims = claims(json!(AUDIENCE));
        claims["exp"] = json!(get_current_timestamp() - 120);
        let token = signed_token(Algorithm::RS256, &claims, |_| {});
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_a_premature_token() {
        let mut claims = claims(json!(AUDIENCE));
        claims["nbf"] = json!(get_current_timestamp() + 120);
        let token = signed_token(Algorithm::RS256, &claims, |_| {});
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_an_empty_subject() {
        let mut claims = claims(json!(AUDIENCE));
        claims["sub"] = json!("");
        let token = signed_token(Algorithm::RS256, &claims, |_| {});
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_a_missing_subject() {
        let mut claims = claims(json!(AUDIENCE));
        claims.as_object_mut().unwrap().remove("sub");
        let token = signed_token(Algorithm::RS256, &claims, |_| {});
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_a_missing_expiry() {
        let mut claims = claims(json!(AUDIENCE));
        claims.as_object_mut().unwrap().remove("exp");
        let token = signed_token(Algorithm::RS256, &claims, |_| {});
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_a_non_numeric_not_before_value() {
        let mut claims = claims(json!(AUDIENCE));
        claims["nbf"] = json!("tomorrow");
        let token = signed_token(Algorithm::RS256, &claims, |_| {});
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_an_unknown_key() {
        let token = signed_token(Algorithm::RS256, &claims(json!(AUDIENCE)), |header| {
            header.kid = Some("unknown-key".into());
        });
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_a_bad_signature() {
        let signing_key = generated_test_key(Algorithm::ES256, "ES256-key");
        let token = signed_token_with_key(&signing_key, &claims(json!(AUDIENCE)));
        assert!(
            authenticator(Algorithm::ES256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_a_hmac_token() {
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("RS256-key".into());
        let token = encode(
            &header,
            &claims(json!(AUDIENCE)),
            &EncodingKey::from_secret(b"secret"),
        )
        .unwrap();
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_an_unsupported_algorithm() {
        let key = test_key(Algorithm::RS256);
        let mut header = Header::new(Algorithm::PS256);
        header.kid = Some("RS256-key".into());
        let token = encode(&header, &claims(json!(AUDIENCE)), &key.encoding).unwrap();
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_an_unsigned_token() {
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate("eyJhbGciOiJub25lIiwia2lkIjoieCJ9.e30.")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_a_missing_key_id() {
        let token = signed_token(Algorithm::RS256, &claims(json!(AUDIENCE)), |header| {
            header.kid = None;
        });
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_a_token_key_url() {
        let token = signed_token(Algorithm::RS256, &claims(json!(AUDIENCE)), |header| {
            header.jku = Some("https://attacker.example.com/jwks".into());
        });
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_a_token_certificate_url() {
        let token = signed_token(Algorithm::RS256, &claims(json!(AUDIENCE)), |header| {
            header.x5u = Some("https://attacker.example.com/key".into());
        });
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_an_embedded_key() {
        let token = signed_token(Algorithm::RS256, &claims(json!(AUDIENCE)), |header| {
            header.jwk = Some(test_key(Algorithm::RS256).jwk.clone());
        });
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_an_unsupported_critical_header() {
        let token = signed_token(Algorithm::RS256, &claims(json!(AUDIENCE)), |header| {
            header.crit = Some(vec!["custom".into()]);
        });
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_an_unexpected_token_type() {
        let token = signed_token(Algorithm::RS256, &claims(json!(AUDIENCE)), |header| {
            header.typ = Some("id+jwt".into());
        });
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate(&token)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn authenticate_rejects_an_encrypted_token_shape() {
        assert!(
            authenticator(Algorithm::RS256)
                .authenticate("one.two.three.four.five")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unknown_key_refresh_loads_a_rotated_key() {
        let old_key = generated_test_key(Algorithm::ES256, "old-key");
        let new_key = generated_test_key(Algorithm::ES256, "new-key");
        let body = serde_json::to_string(&JwkSet {
            keys: vec![new_key.jwk.clone()],
        })
        .unwrap();
        let (url, state, server) = metadata_server(StatusCode::OK, body, Duration::ZERO).await;
        let authenticator = refreshing_authenticator(
            JwkSet {
                keys: vec![old_key.jwk],
            },
            url,
            false,
        );
        let token = signed_token_with_key(&new_key, &claims(json!(AUDIENCE)));

        assert!(authenticator.authenticate(&token).await.is_ok());
        assert_eq!(state.requests.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn concurrent_unknown_key_refreshes_are_coalesced() {
        let old_key = generated_test_key(Algorithm::ES256, "old-key");
        let new_key = generated_test_key(Algorithm::ES256, "new-key");
        let body = serde_json::to_string(&JwkSet {
            keys: vec![new_key.jwk.clone()],
        })
        .unwrap();
        let (url, state, server) =
            metadata_server(StatusCode::OK, body, Duration::from_millis(50)).await;
        let authenticator = refreshing_authenticator(
            JwkSet {
                keys: vec![old_key.jwk],
            },
            url,
            false,
        );
        let token = signed_token_with_key(&new_key, &claims(json!(AUDIENCE)));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let authenticator = authenticator.clone();
            let token = token.clone();
            tasks.push(tokio::spawn(async move {
                authenticator.authenticate(&token).await
            }));
        }

        for task in tasks {
            assert!(task.await.unwrap().is_ok());
        }
        assert_eq!(state.requests.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn unknown_key_refresh_obeys_the_retry_floor() {
        let old_key = generated_test_key(Algorithm::ES256, "old-key");
        let new_key = generated_test_key(Algorithm::ES256, "new-key");
        let (url, state, server) = metadata_server(
            StatusCode::SERVICE_UNAVAILABLE,
            String::new(),
            Duration::ZERO,
        )
        .await;
        let authenticator = refreshing_authenticator(
            JwkSet {
                keys: vec![old_key.jwk],
            },
            url,
            false,
        );
        let token = signed_token_with_key(&new_key, &claims(json!(AUDIENCE)));

        assert!(authenticator.authenticate(&token).await.is_err());
        assert!(authenticator.authenticate(&token).await.is_err());
        assert_eq!(state.requests.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn failed_refresh_keeps_the_last_valid_key_set() {
        let old_key = generated_test_key(Algorithm::ES256, "old-key");
        let token = signed_token_with_key(&old_key, &claims(json!(AUDIENCE)));
        let duplicate_keys = JwkSet {
            keys: vec![old_key.jwk.clone(), old_key.jwk.clone()],
        };
        let (url, state, server) = metadata_server(
            StatusCode::OK,
            serde_json::to_string(&duplicate_keys).unwrap(),
            Duration::ZERO,
        )
        .await;
        let authenticator = refreshing_authenticator(
            JwkSet {
                keys: vec![old_key.jwk],
            },
            url,
            true,
        );

        assert!(authenticator.authenticate(&token).await.is_ok());
        assert!(authenticator.authenticate(&token).await.is_ok());
        assert_eq!(state.requests.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn discovery_requires_an_exact_issuer() {
        let issuer = Url::parse("http://issuer.example.com/tenant").unwrap();
        let document = json!({
            "issuer": issuer.as_str(),
            "jwks_uri": "https://issuer.example.com/keys"
        });
        let (url, _, server) = metadata_server(
            StatusCode::OK,
            serde_json::to_string(&document).unwrap(),
            Duration::ZERO,
        )
        .await;
        let discovered = discover_jwks_url(&metadata_client(), &url, issuer.as_str()).await;
        assert_eq!(
            discovered.unwrap().as_str(),
            "https://issuer.example.com/keys"
        );
        server.abort();

        let document = json!({
            "issuer": "http://issuer.example.com/other",
            "jwks_uri": "https://issuer.example.com/keys"
        });
        let (url, _, server) = metadata_server(
            StatusCode::OK,
            serde_json::to_string(&document).unwrap(),
            Duration::ZERO,
        )
        .await;
        let error = discover_jwks_url(&metadata_client(), &url, issuer.as_str())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Discovery(_)));
        server.abort();
    }

    #[tokio::test]
    async fn metadata_requests_reject_http_errors_and_large_responses() {
        let (url, _, server) =
            metadata_server(StatusCode::BAD_GATEWAY, String::new(), Duration::ZERO).await;
        assert!(matches!(
            get_bounded(&metadata_client(), url, 16).await.unwrap_err(),
            Error::HttpStatus(StatusCode::BAD_GATEWAY)
        ));
        server.abort();

        let (url, _, server) =
            metadata_server(StatusCode::OK, "x".repeat(17), Duration::ZERO).await;
        assert!(matches!(
            get_bounded(&metadata_client(), url, 16).await.unwrap_err(),
            Error::ResponseTooLarge { limit: 16 }
        ));
        server.abort();
    }

    #[tokio::test]
    async fn key_set_fetch_rejects_malformed_json() {
        let (url, _, server) =
            metadata_server(StatusCode::OK, "not-json".into(), Duration::ZERO).await;
        assert!(matches!(
            fetch_key_set(&metadata_client(), &url).await.unwrap_err(),
            Error::Jwks(_)
        ));
        server.abort();
    }

    #[test]
    fn discovery_url_keeps_the_issuer_path() {
        let issuer = Url::parse("https://auth.example.com/tenant").unwrap();
        assert_eq!(
            discovery_url(&issuer).unwrap().as_str(),
            "https://auth.example.com/tenant/.well-known/openid-configuration"
        );
    }

    #[test]
    fn jwt_configuration_requires_https() {
        let error = AuthConfig::from_values(
            "jwt",
            Some("http://auth.example.com".into()),
            Some("rad".into()),
            None,
            None,
            ScopeConfig::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("must use HTTPS"));
    }

    #[test]
    fn jwt_configuration_requires_an_issuer_and_audience() {
        assert!(
            AuthConfig::from_values(
                "jwt",
                None,
                Some("rad".into()),
                None,
                None,
                ScopeConfig::default(),
            )
            .is_err()
        );
        assert!(
            AuthConfig::from_values(
                "jwt",
                Some("https://auth.example.com".into()),
                None,
                None,
                None,
                ScopeConfig::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn disabled_authentication_rejects_jwt_settings() {
        assert!(
            AuthConfig::from_values(
                "none",
                Some("https://auth.example.com".into()),
                None,
                None,
                None,
                ScopeConfig::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn disabled_authentication_rejects_scope_settings() {
        let scopes = ScopeConfig::from_values(Some("rad:read".into()), None, None).unwrap();

        assert!(AuthConfig::from_values("none", None, None, None, None, scopes).is_err());
    }

    #[test]
    fn jwt_configuration_uses_the_rfc9068_profile_by_default() {
        let scopes = ScopeConfig::from_values(Some("rad:read".into()), None, None).unwrap();
        let config = jwt_config(None, scopes);

        assert_eq!(config.profile, JwtProfile::Rfc9068);
    }

    #[test]
    fn jwt_configuration_accepts_the_compatible_profile() {
        let scopes = ScopeConfig::from_values(Some("rad:read".into()), None, None).unwrap();
        let config = jwt_config(Some(JwtProfile::Compatible), scopes);

        assert_eq!(config.profile, JwtProfile::Compatible);
    }

    #[test]
    fn jwt_configuration_accepts_the_cloudflare_access_profile() {
        let scopes = ScopeConfig::from_values_with_admin(
            Some("authenticated".into()),
            Some("authenticated".into()),
            Some("authenticated".into()),
            Some("authenticated".into()),
        )
        .unwrap();
        let config = jwt_config(Some(JwtProfile::CloudflareAccess), scopes);

        assert_eq!(config.profile, JwtProfile::CloudflareAccess);
    }

    #[test]
    fn cloudflare_access_profile_rejects_oauth_scope_names() {
        let error = AuthConfig::from_values(
            "jwt",
            Some("https://team.cloudflareaccess.com".into()),
            Some("application-audience".into()),
            Some("https://team.cloudflareaccess.com/cdn-cgi/access/certs".into()),
            Some(JwtProfile::CloudflareAccess),
            ScopeConfig::from_values(Some("rad:read".into()), None, None).unwrap(),
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid authentication configuration: auth scope settings must contain only \"authenticated\" for profile cloudflare-access"
        );
    }

    #[test]
    fn jwt_profile_rejects_an_unknown_value() {
        let error = "simple".parse::<JwtProfile>().unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid authentication configuration: unknown auth profile \"simple\" (rfc9068, compatible, or cloudflare-access)"
        );
    }

    #[test]
    fn disabled_authentication_rejects_a_profile() {
        assert!(
            AuthConfig::from_values(
                "none",
                None,
                None,
                None,
                Some(JwtProfile::Compatible),
                ScopeConfig::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn jwt_configuration_requires_a_scope_setting() {
        let error = AuthConfig::from_values(
            "jwt",
            Some("https://auth.example.com".into()),
            Some("rad".into()),
            None,
            None,
            ScopeConfig::default(),
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid authentication configuration: JWT authentication requires at least one auth scope setting"
        );
    }

    #[test]
    fn scope_configuration_uses_the_oauth_scope_list_syntax() {
        let scopes = ScopeConfig::from_values(
            Some("rad:read https://rad.example.com/admin".into()),
            None,
            None,
        )
        .unwrap();

        assert_eq!(scopes.query.len(), 2);
    }

    #[test]
    fn administration_scope_configuration_enables_jwt_authentication() {
        let scopes =
            ScopeConfig::from_values_with_admin(None, None, None, Some("rad:admin".into()))
                .unwrap();

        assert!(scopes.is_enabled());
        assert_eq!(scopes.admin.len(), 1);
    }

    #[test]
    fn scope_configuration_rejects_invalid_oauth_scope_lists() {
        for value in ["", "rad:read  rad:admin", "rad:read\trad:admin", "räd:read"] {
            assert!(ScopeConfig::from_values(Some(value.into()), None, None).is_err());
        }
    }

    #[test]
    fn jwt_configuration_rejects_url_credentials() {
        let error = AuthConfig::from_values(
            "jwt",
            Some("https://user:secret@auth.example.com".into()),
            Some("rad".into()),
            None,
            None,
            ScopeConfig::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("must not contain credentials"));
    }

    #[test]
    fn jwt_configuration_rejects_url_fragments_and_issuer_queries() {
        for issuer in [
            "https://auth.example.com/#fragment",
            "https://auth.example.com/?tenant=one",
        ] {
            assert!(
                AuthConfig::from_values(
                    "jwt",
                    Some(issuer.into()),
                    Some("rad".into()),
                    None,
                    None,
                    ScopeConfig::default(),
                )
                .is_err()
            );
        }
        assert!(
            AuthConfig::from_values(
                "jwt",
                Some("https://auth.example.com".into()),
                Some("rad".into()),
                Some("https://auth.example.com/keys#fragment".into()),
                None,
                ScopeConfig::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn key_set_rejects_duplicate_key_ids() {
        let document: JwkSet = serde_json::from_value(json!({
            "keys": [
                {"kty":"RSA", "kid":"one", "n":"AQ", "e":"AQAB"},
                {"kty":"RSA", "kid":"one", "n":"AQ", "e":"AQAB"}
            ]
        }))
        .unwrap();
        let error = validate_key_set(document).unwrap_err();
        assert!(matches!(error, Error::Jwks(_)));
    }

    #[test]
    fn key_set_rejects_symmetric_keys() {
        let document: JwkSet = serde_json::from_value(json!({
            "keys": [{"kty":"oct", "kid":"one", "k":"c2VjcmV0"}]
        }))
        .unwrap();
        let error = validate_key_set(document).unwrap_err();
        assert!(error.to_string().contains("no supported signature key"));
    }

    #[test]
    fn key_set_rejects_more_than_64_key_ids() {
        let key = test_key(Algorithm::ES256).jwk.clone();
        let keys = (0..65)
            .map(|index| {
                let mut key = key.clone();
                key.common.key_id = Some(format!("key-{index}"));
                key
            })
            .collect();
        let error = validate_key_set(JwkSet { keys }).unwrap_err();
        assert!(error.to_string().contains("more than 64 key IDs"));
    }
}
