use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};

use crate::auth::Authenticator;
use crate::engine::exec::Capability;
use crate::http::generated::types as wire;

pub(super) async fn require(
    State(authenticator): State<Arc<Authenticator>>,
    request: Request,
    next: Next,
) -> Response {
    require_for(authenticator, request, next, AuthorizationTarget::Public).await
}

pub(crate) async fn require_admin(
    State(authenticator): State<Arc<Authenticator>>,
    request: Request,
    next: Next,
) -> Response {
    require_for(authenticator, request, next, AuthorizationTarget::Admin).await
}

#[derive(Clone, Copy)]
enum AuthorizationTarget {
    Public,
    Admin,
}

async fn require_for(
    authenticator: Arc<Authenticator>,
    mut request: Request,
    next: Next,
    target: AuthorizationTarget,
) -> Response {
    if matches!(target, AuthorizationTarget::Public)
        && (request.method() == Method::OPTIONS || is_public_probe(request.uri().path()))
    {
        return next.run(request).await;
    }
    let token = match bearer_token(request.headers()) {
        Ok(token) => token,
        Err(reason) => {
            crate::telemetry::auth_request(reason.as_str());
            return unauthenticated(reason);
        }
    };
    let authenticated = match authenticator.authenticate(token).await {
        Ok(authenticated) => authenticated,
        Err(_) => {
            crate::telemetry::auth_request(AuthFailure::Invalid.as_str());
            return unauthenticated(AuthFailure::Invalid);
        }
    };
    crate::telemetry::auth_request("authenticated");
    let principal = authenticated.principal;
    let policy = authenticated.policy;
    request.extensions_mut().insert(principal.clone());
    request.extensions_mut().insert(policy);
    let mut context = crate::logging::request_context();
    context.principal_issuer.clone_from(&principal.issuer);
    context.principal_subject.clone_from(&principal.subject);
    let response = crate::logging::with_request_context(context, async move {
        let capability = match target {
            AuthorizationTarget::Public => {
                required_capability(request.method(), request.uri().path())
            }
            AuthorizationTarget::Admin => Some(Capability::Admin),
        };
        if let Some(capability) = capability {
            if !policy.allows(capability) {
                crate::telemetry::auth_authorization(capability.as_str(), "denied");
                return forbidden(capability);
            }
            crate::telemetry::auth_authorization(capability.as_str(), "allowed");
        } else if request.method() == Method::POST
            && request.uri().path() == "/execute"
            && !policy.allows_any()
        {
            crate::telemetry::auth_authorization("program", "denied");
            return forbidden_program();
        }
        next.run(request).await
    })
    .await;
    let mut response = response;
    response.extensions_mut().insert(principal);
    response
}

#[derive(Clone, Copy, Debug)]
enum AuthFailure {
    Missing,
    Invalid,
}

impl AuthFailure {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing_token",
            Self::Invalid => "invalid_token",
        }
    }
}

fn bearer_token(headers: &axum::http::HeaderMap) -> Result<&str, AuthFailure> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return Err(AuthFailure::Missing);
    };
    if values.next().is_some() {
        return Err(AuthFailure::Invalid);
    }
    let value = value.to_str().map_err(|_| AuthFailure::Invalid)?;
    let mut parts = value.split_ascii_whitespace();
    let scheme = parts.next().ok_or(AuthFailure::Invalid)?;
    let token = parts.next().ok_or(AuthFailure::Invalid)?;
    if !scheme.eq_ignore_ascii_case("Bearer") || token.is_empty() || parts.next().is_some() {
        return Err(AuthFailure::Invalid);
    }
    Ok(token)
}

fn unauthenticated(reason: AuthFailure) -> Response {
    let detail = match reason {
        AuthFailure::Missing => "A bearer access token is required.",
        AuthFailure::Invalid => "The bearer access token is invalid.",
    };
    let problem_reason = match reason {
        AuthFailure::Missing => wire::UnauthenticatedProblemReason::MissingToken,
        AuthFailure::Invalid => wire::UnauthenticatedProblemReason::InvalidToken,
    };
    let body = wire::Problem::UnauthenticatedProblem(wire::UnauthenticatedProblem {
        detail: Some(detail.into()),
        reason: problem_reason,
        status: 401,
        title: wire::UnauthenticatedProblemTitle::AuthenticationRequired,
        r#type: wire::UnauthenticatedProblemType::UrnRadProblemUnauthenticated,
    });
    let challenge = match reason {
        AuthFailure::Missing => HeaderValue::from_static("Bearer realm=\"rad\""),
        AuthFailure::Invalid => {
            HeaderValue::from_static("Bearer realm=\"rad\", error=\"invalid_token\"")
        }
    };
    let mut response = (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, challenge);
    response
}

fn required_capability(method: &Method, path: &str) -> Option<Capability> {
    if method.as_str() == "QUERY" && path == "/execute" {
        return Some(Capability::Query);
    }
    match (method, path) {
        (
            &Method::GET,
            "/info" | "/healthz" | "/statistics" | "/schema" | "/tables" | "/metrics",
        ) => Some(Capability::Query),
        (&Method::GET, path) if path.starts_with("/schema/transitions/") => Some(Capability::Query),
        (&Method::GET, "/schema/transitions")
        | (&Method::POST, "/schema/diff" | "/schema/compatibility") => Some(Capability::Query),
        (&Method::POST, "/schema/migrate") => Some(Capability::Catalog),
        (&Method::POST, path) if path.starts_with("/tables/") || path == "/tables" => {
            Some(Capability::Catalog)
        }
        (&Method::POST, path)
            if path.starts_with("/schema/transitions/") && path.ends_with("/cancel") =>
        {
            Some(Capability::Catalog)
        }
        (&Method::PATCH | &Method::DELETE, path) if path.starts_with("/tables/") => {
            Some(Capability::Catalog)
        }
        _ => None,
    }
}

fn forbidden(capability: Capability) -> Response {
    forbidden_response(format!(
        "The access token does not grant {} access.",
        capability.as_str()
    ))
}

fn forbidden_program() -> Response {
    forbidden_response("The access token does not grant program access.")
}

fn forbidden_response(detail: impl Into<String>) -> Response {
    let body = wire::Problem::ForbiddenProblem(wire::ForbiddenProblem {
        detail: Some(detail.into()),
        reason: wire::ForbiddenProblemReason::InsufficientScope,
        status: 403,
        title: wire::ForbiddenProblemTitle::AccessForbidden,
        r#type: wire::ForbiddenProblemType::UrnRadProblemForbidden,
    });
    let mut response = (StatusCode::FORBIDDEN, axum::Json(body)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"rad\", error=\"insufficient_scope\""),
    );
    response
}

fn is_public_probe(path: &str) -> bool {
    matches!(path, "/startupz" | "/readyz" | "/livez")
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, Method, header};

    use super::{AuthFailure, bearer_token, is_public_probe, required_capability};
    use crate::engine::exec::Capability;

    #[test]
    fn bearer_token_accepts_one_bearer_value() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer token"),
        );
        assert_eq!(bearer_token(&headers).unwrap(), "token");
    }

    #[test]
    fn bearer_token_reports_a_missing_value() {
        assert!(matches!(
            bearer_token(&HeaderMap::new()),
            Err(AuthFailure::Missing)
        ));
    }

    #[test]
    fn bearer_token_rejects_multiple_values() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer one"),
        );
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer two"),
        );
        assert!(matches!(bearer_token(&headers), Err(AuthFailure::Invalid)));
    }

    #[test]
    fn public_probes_are_exact() {
        assert!(is_public_probe("/startupz"));
        assert!(is_public_probe("/readyz"));
        assert!(is_public_probe("/livez"));
        assert!(!is_public_probe("/healthz"));
        assert!(!is_public_probe("/metrics"));
    }

    #[test]
    fn request_capabilities_distinguish_query_and_catalog_operations() {
        let cases = [
            (Method::GET, "/info", Some(Capability::Query)),
            (
                Method::from_bytes(b"QUERY").unwrap(),
                "/execute",
                Some(Capability::Query),
            ),
            (Method::POST, "/schema/diff", Some(Capability::Query)),
            (Method::POST, "/schema/migrate", Some(Capability::Catalog)),
            (Method::POST, "/tables", Some(Capability::Catalog)),
            (Method::PATCH, "/tables/users", Some(Capability::Catalog)),
            (Method::POST, "/execute", None),
        ];

        for (method, path, expected) in cases {
            assert_eq!(required_capability(&method, path), expected);
        }
    }
}
