//! Bearer + `X-Tenant-Id` middleware.
//!
//! Two checks per request:
//!   1. `Authorization: Bearer <MARKETING_ADMIN_TOKEN>` must
//!      match. Constant-time compare so a probing caller
//!      can't time-attack the secret.
//!   2. `X-Tenant-Id` must be present + parse as a valid
//!      `TenantId` + the extension must have a store mounted
//!      for that tenant. Mismatch → 403 with a typed error
//!      body.
//!
//! The validated `TenantId` is stamped onto the request via
//! `Extensions` so downstream handlers extract via `Extension<TenantId>`
//! and trust it's already been authenticated.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use super::AdminState;
use crate::tenant::TenantId;

pub const AUTH_HEADER: &str = "Authorization";
pub const TENANT_HEADER: &str = "X-Tenant-Id";

/// Type alias for handlers extracting the auth-stamped tenant.
pub type AuthState = Arc<AdminState>;

pub async fn bearer_and_tenant_middleware(
    State(state): State<Arc<AdminState>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    // Health is allowlisted — operator's loopback probe must
    // succeed without a token (lets the daemon's supervisor
    // see "extension up" before the operator's first call).
    if req.uri().path() == "/healthz" {
        return next.run(req).await;
    }

    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string());
    let bearer = match bearer {
        Some(b) if constant_time_eq(b.as_bytes(), state.bearer_token.as_bytes()) => b,
        _ => {
            return error(
                StatusCode::UNAUTHORIZED,
                "missing_or_invalid_bearer",
                "Authorization: Bearer <token> required",
            );
        }
    };
    drop(bearer); // not needed downstream

    let tenant_raw = match req.headers().get(TENANT_HEADER).and_then(|v| v.to_str().ok()) {
        Some(s) => s.trim().to_string(),
        None => {
            return error(
                StatusCode::BAD_REQUEST,
                "missing_tenant_id",
                "X-Tenant-Id header is required",
            );
        }
    };
    let tenant_id = match TenantId::new(tenant_raw.clone()) {
        Ok(t) => t,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_tenant_id",
                &format!("X-Tenant-Id {tenant_raw:?} failed validation"),
            );
        }
    };
    if state.lookup_store(&tenant_id).is_none() {
        return error(
            StatusCode::FORBIDDEN,
            "tenant_unauthorised",
            &format!(
                "extension has no store mounted for tenant {:?}",
                tenant_id.as_str()
            ),
        );
    }
    req.extensions_mut().insert(tenant_id);
    next.run(req).await
}

/// Helper called by handlers that don't take `Extension<TenantId>`
/// directly (rare — most handlers do).
pub fn require_tenant_id<'a>(req: &'a Request<Body>) -> Option<&'a TenantId> {
    req.extensions().get::<TenantId>()
}

/// Constant-time byte compare so attackers can't time the
/// bearer.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({
            "ok": false,
            "error": { "code": code, "message": message }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::router;
    use crate::lead::LeadStore;
    use axum::body::Body;
    use axum::http::Request;
    use std::path::PathBuf;
    use tower::util::ServiceExt;

    async fn build_state() -> Arc<AdminState> {
        let s = LeadStore::open(
            PathBuf::from(":memory:"),
            TenantId::new("acme").unwrap(),
        )
        .await
        .unwrap();
        Arc::new(AdminState::new("secret".into()).with_store(Arc::new(s)))
    }

    fn request(uri: &str, bearer: Option<&str>, tenant: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().uri(uri);
        if let Some(t) = bearer {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        if let Some(t) = tenant {
            b = b.header(TENANT_HEADER, t);
        }
        b.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn healthz_bypasses_auth() {
        let state = build_state().await;
        let app = router(state);
        let resp = app
            .oneshot(request("/healthz", None, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_bearer_returns_401() {
        let state = build_state().await;
        let app = router(state);
        let resp = app
            .oneshot(request("/leads", None, Some("acme")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_bearer_returns_401() {
        let state = build_state().await;
        let app = router(state);
        let resp = app
            .oneshot(request("/leads", Some("WRONG"), Some("acme")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_tenant_returns_400() {
        let state = build_state().await;
        let app = router(state);
        let resp = app
            .oneshot(request("/leads", Some("secret"), None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn invalid_tenant_id_format_returns_400() {
        let state = build_state().await;
        let app = router(state);
        let resp = app
            .oneshot(request("/leads", Some("secret"), Some("BAD")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unmounted_tenant_returns_403() {
        let state = build_state().await;
        let app = router(state);
        let resp = app
            .oneshot(request("/leads", Some("secret"), Some("globex")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn happy_path_reaches_handler() {
        let state = build_state().await;
        let app = router(state);
        let resp = app
            .oneshot(request("/leads", Some("secret"), Some("acme")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
