//! Public tracking ingest routes (M15.23.a.3).
//!
//! Two GET handlers, both **un-authenticated** — they're hit by
//! recipients' email clients (pixel) + browsers (click) so the
//! bearer-auth middleware that guards every other admin route
//! cannot apply here. Forgery is gated by the HMAC token in
//! the URL.
//!
//! ## `GET /t/o/{tenant_id}/{msg_id}?tag={hmac}`
//!
//! 1. Verify HMAC against the same signer the outbound
//!    publisher used. Mismatch → 401 (no body) so we don't
//!    leak that the tag was rejected.
//! 2. Persist an `OpenEvent` (best-effort — store hiccup logs
//!    a warn but the response still serves the pixel).
//! 3. Return the 1×1 transparent GIF with `Cache-Control:
//!    no-store` so the recipient's cache doesn't suppress
//!    subsequent opens.
//!
//! ## `GET /t/c/{tenant_id}/{msg_id}/{link_id}?tag={hmac}`
//!
//! 1. Verify HMAC.
//! 2. `lookup_link` against the store. Missing row → 404
//!    (forged URL, expired record, cross-tenant).
//! 3. Persist a `ClickEvent` (best-effort, same posture as
//!    open).
//! 4. 302 to the original URL. `Cache-Control: no-store` to
//!    avoid the browser short-circuiting future clicks.

use std::sync::Arc;

use axum::{
    extract::{Extension, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use chrono::Utc;
use nexo_microapp_sdk::tracking::{
    ClickEvent, LinkId, MsgId, OpenEvent, TrackingToken,
    PIXEL_GIF_BYTES, PIXEL_GIF_CONTENT_TYPE,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::AdminState;
use crate::tenant::TenantId;

/// Query string holding the HMAC tag. Parsed via `axum::Query`.
#[derive(Debug, Deserialize)]
pub struct TagQuery {
    pub tag: String,
}

/// `GET /t/o/{tenant_id}/{msg_id}?tag=...`
pub async fn open_handler(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Path((tenant_id, msg_id)): Path<(String, String)>,
    Query(q): Query<TagQuery>,
) -> Response {
    let Some(tracking) = state.tracking.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let msg = MsgId::new(msg_id);
    let token = TrackingToken::from_string(q.tag);
    if tracking
        .signer
        .verify(&tenant_id, &msg, None, &token)
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Best-effort persistence — never blocks the pixel response.
    let event = OpenEvent {
        tenant_id: tenant_id.clone(),
        msg_id: msg,
        opened_at_ms: Utc::now().timestamp_millis(),
        ip_hash: peer_ip_hash(&headers),
        ua_hash: header_hash(&headers, header::USER_AGENT),
    };
    if let Err(e) = tracking.store.record_open(&event).await {
        tracing::warn!(
            target: "marketing.tracking",
            error = %e,
            tenant = %tenant_id,
            "open event persist failed (non-fatal)"
        );
    }
    pixel_response()
}

/// `GET /t/c/{tenant_id}/{msg_id}/{link_id}?tag=...`
pub async fn click_handler(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Path((tenant_id, msg_id, link_id)): Path<(String, String, String)>,
    Query(q): Query<TagQuery>,
) -> Response {
    let Some(tracking) = state.tracking.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let msg = MsgId::new(msg_id);
    let link = LinkId::new(link_id);
    let token = TrackingToken::from_string(q.tag);
    if tracking
        .signer
        .verify(&tenant_id, &msg, Some(&link), &token)
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let original_url = match tracking
        .store
        .lookup_link(&tenant_id, &msg, &link)
        .await
    {
        Ok(Some(url)) => url,
        Ok(None) => {
            // Forged path or expired record — same response as
            // a missing tenant so we don't leak which step
            // failed.
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(e) => {
            tracing::warn!(
                target: "marketing.tracking",
                error = %e,
                tenant = %tenant_id,
                "click lookup failed (non-fatal, redirecting blind)"
            );
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let event = ClickEvent {
        tenant_id: tenant_id.clone(),
        msg_id: msg,
        link_id: link,
        clicked_at_ms: Utc::now().timestamp_millis(),
        ip_hash: peer_ip_hash(&headers),
        ua_hash: header_hash(&headers, header::USER_AGENT),
    };
    if let Err(e) = tracking.store.record_click(&event).await {
        tracing::warn!(
            target: "marketing.tracking",
            error = %e,
            tenant = %tenant_id,
            "click event persist failed (non-fatal)"
        );
    }
    let mut response = Redirect::to(&original_url).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

/// `GET /tracking/msg/{msg_id}/engagement` — protected, bearer
/// auth + `X-Tenant-Id` header. Returns the opens count + the
/// per-link click breakdown for one outbound message. Powers
/// the operator UI's lead drawer badge ("📧 Opened 3× · clicked
/// pricing 2×, faq 1×").
///
/// Empty result when the message hasn't been sent yet — caller
/// renders an empty state ("Sin lecturas todavía").
pub async fn engagement_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(msg_id): Path<String>,
) -> Response {
    let Some(tracking) = state.tracking.as_ref() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": { "code": "tracking_disabled" }
            })),
        )
            .into_response();
    };
    let msg = MsgId::new(msg_id.clone());
    let opens = match tracking
        .store
        .count_opens(tenant_id.as_str(), &msg)
        .await
    {
        Ok(n) => n,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": { "code": "store", "detail": e.to_string() } })),
            )
                .into_response();
        }
    };
    let clicks_by_link = match tracking
        .store
        .count_clicks_by_link(tenant_id.as_str(), &msg)
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|(l, n)| LinkClickCount {
                link_id: l.as_str().to_string(),
                count: n,
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": { "code": "store", "detail": e.to_string() } })),
            )
                .into_response();
        }
    };
    Json(json!({
        "msg_id": msg_id,
        "tenant_id": tenant_id.as_str(),
        "opens": opens,
        "clicks_by_link": clicks_by_link,
    }))
    .into_response()
}

/// One row of the per-link click histogram. Sorted desc by
/// `count` (the SQL `ORDER BY 2 DESC` does the work).
#[derive(Debug, Clone, Serialize)]
pub struct LinkClickCount {
    /// Link identifier (`L0`, `L1`, …).
    pub link_id: String,
    /// Hit count for that link.
    pub count: u64,
}

fn pixel_response() -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(PIXEL_GIF_CONTENT_TYPE),
    );
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store, no-cache, must-revalidate"),
    );
    headers.insert(
        header::PRAGMA,
        header::HeaderValue::from_static("no-cache"),
    );
    (StatusCode::OK, headers, PIXEL_GIF_BYTES.to_vec()).into_response()
}

/// SHA-256(IP) truncated to 16 hex chars. Reads
/// `X-Forwarded-For` first (operator's reverse proxy stamps
/// it) then falls back to the bare value. `None` when neither
/// header is present.
fn peer_ip_hash(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(str::trim)
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
        })?;
    if raw.is_empty() {
        return None;
    }
    Some(short_hash(raw))
}

fn header_hash(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    let raw = headers.get(name)?.to_str().ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(short_hash(raw))
}

fn short_hash(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let bytes = hasher.finalize();
    hex_encode(&bytes[..8])
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracking::TrackingDeps;
    use axum::body::to_bytes;
    use axum::body::Body;
    use axum::http::Request;
    use nexo_microapp_sdk::tracking::{
        open_pool, SqliteTrackingStore, TrackingStore, TrackingTokenSigner,
    };
    use tower::ServiceExt;

    async fn fresh_state() -> Arc<AdminState> {
        let pool = open_pool(":memory:").await.unwrap();
        let store: Arc<dyn TrackingStore> =
            Arc::new(SqliteTrackingStore::new(pool));
        let signer = TrackingTokenSigner::new(vec![0u8; 32]).unwrap();
        let deps = Arc::new(TrackingDeps::new(
            signer,
            store,
            "https://t.example".into(),
        ));
        Arc::new(AdminState::new("token".into()).with_tracking(deps))
    }

    fn router(state: Arc<AdminState>) -> axum::Router {
        axum::Router::new()
            .route(
                "/t/o/:tenant/:msg",
                axum::routing::get(open_handler),
            )
            .route(
                "/t/c/:tenant/:msg/:link",
                axum::routing::get(click_handler),
            )
            .with_state(state)
    }

    #[tokio::test]
    async fn open_with_valid_tag_returns_pixel_and_records_event() {
        let state = fresh_state().await;
        let tracking = state.tracking.clone().unwrap();
        let msg = MsgId::new("m1");
        let token = tracking.signer.sign_open("acme", &msg);
        let req = Request::builder()
            .uri(format!("/t/o/acme/m1?tag={}", token.as_str()))
            .body(Body::empty())
            .unwrap();
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            PIXEL_GIF_CONTENT_TYPE,
        );
        let body = to_bytes(resp.into_body(), 200).await.unwrap();
        assert_eq!(body.as_ref(), PIXEL_GIF_BYTES);
        // Event recorded.
        let n = tracking.store.count_opens("acme", &msg).await.unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn open_with_forged_tag_is_unauthorised() {
        let state = fresh_state().await;
        let req = Request::builder()
            .uri("/t/o/acme/m1?tag=00000000000000000000AA")
            .body(Body::empty())
            .unwrap();
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // No event recorded.
        let tracking = state.tracking.clone().unwrap();
        let n = tracking
            .store
            .count_opens("acme", &MsgId::new("m1"))
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn open_with_cross_tenant_tag_is_unauthorised() {
        let state = fresh_state().await;
        let tracking = state.tracking.clone().unwrap();
        let token = tracking.signer.sign_open("globex", &MsgId::new("m1"));
        // Tag minted for globex, URL says acme.
        let req = Request::builder()
            .uri(format!("/t/o/acme/m1?tag={}", token.as_str()))
            .body(Body::empty())
            .unwrap();
        let resp = router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn click_with_valid_tag_records_and_redirects() {
        let state = fresh_state().await;
        let tracking = state.tracking.clone().unwrap();
        let msg = MsgId::new("m1");
        let link = LinkId::new("L0");
        // Pre-register the link so lookup_link succeeds.
        tracking
            .store
            .register_link("acme", &msg, &link, "https://acme.com/x", 1)
            .await
            .unwrap();
        let token = tracking.signer.sign_click("acme", &msg, &link);
        let req = Request::builder()
            .uri(format!(
                "/t/c/acme/m1/L0?tag={}",
                token.as_str()
            ))
            .body(Body::empty())
            .unwrap();
        let resp = router(state.clone()).oneshot(req).await.unwrap();
        assert!(
            resp.status().is_redirection(),
            "expected 3xx, got {}",
            resp.status(),
        );
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "https://acme.com/x",
        );
        // Click recorded.
        let by_link = tracking
            .store
            .count_clicks_by_link("acme", &msg)
            .await
            .unwrap();
        assert_eq!(by_link, vec![(link, 1)]);
    }

    #[tokio::test]
    async fn click_with_unregistered_link_is_not_found() {
        let state = fresh_state().await;
        let tracking = state.tracking.clone().unwrap();
        let msg = MsgId::new("m1");
        let link = LinkId::new("L0");
        // Mint a tag against tenant + msg + link that exists,
        // but skip the register_link step.
        let token = tracking.signer.sign_click("acme", &msg, &link);
        let req = Request::builder()
            .uri(format!(
                "/t/c/acme/m1/L0?tag={}",
                token.as_str()
            ))
            .body(Body::empty())
            .unwrap();
        let resp = router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn click_with_forged_tag_is_unauthorised() {
        let state = fresh_state().await;
        let req = Request::builder()
            .uri("/t/c/acme/m1/L0?tag=00000000000000000000AA")
            .body(Body::empty())
            .unwrap();
        let resp = router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ip_hash_reads_x_forwarded_for_first() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "10.0.0.1, 192.168.1.1".parse().unwrap());
        h.insert("x-real-ip", "172.16.0.1".parse().unwrap());
        // First X-Forwarded-For value used.
        let hash = peer_ip_hash(&h).unwrap();
        let direct = short_hash("10.0.0.1");
        assert_eq!(hash, direct);
    }

    #[tokio::test]
    async fn ip_hash_falls_back_to_x_real_ip() {
        let mut h = HeaderMap::new();
        h.insert("x-real-ip", "172.16.0.1".parse().unwrap());
        let hash = peer_ip_hash(&h).unwrap();
        assert_eq!(hash, short_hash("172.16.0.1"));
    }

    #[tokio::test]
    async fn ip_hash_returns_none_when_no_proxy_headers() {
        let h = HeaderMap::new();
        assert!(peer_ip_hash(&h).is_none());
    }

    #[tokio::test]
    async fn open_404s_when_tracking_disabled() {
        let state = Arc::new(AdminState::new("token".into()));
        let req = Request::builder()
            .uri("/t/o/acme/m1?tag=AAAAAAAAAAAAAAAAAAAAAA")
            .body(Body::empty())
            .unwrap();
        let resp = router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ─── Engagement endpoint ──────────────────────────────────

    fn engagement_router(state: Arc<AdminState>) -> axum::Router {
        axum::Router::new()
            .route(
                "/tracking/msg/:msg_id/engagement",
                axum::routing::get(engagement_handler),
            )
            .layer(axum::Extension(TenantId::new("acme").unwrap()))
            .with_state(state)
    }

    #[tokio::test]
    async fn engagement_returns_zeros_for_unseen_message() {
        let state = fresh_state().await;
        let req = Request::builder()
            .uri("/tracking/msg/never-sent/engagement")
            .body(Body::empty())
            .unwrap();
        let resp = engagement_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["opens"], 0);
        assert_eq!(v["clicks_by_link"].as_array().unwrap().len(), 0);
        assert_eq!(v["tenant_id"], "acme");
    }

    #[tokio::test]
    async fn engagement_aggregates_opens_and_clicks() {
        let state = fresh_state().await;
        let tracking = state.tracking.clone().unwrap();
        let msg = MsgId::new("m1");
        let l0 = LinkId::new("L0");
        let l1 = LinkId::new("L1");
        // Seed 2 opens + 3 clicks (L0×2, L1×1).
        for _ in 0..2 {
            tracking
                .store
                .record_open(&OpenEvent {
                    tenant_id: "acme".into(),
                    msg_id: msg.clone(),
                    opened_at_ms: 1,
                    ip_hash: None,
                    ua_hash: None,
                })
                .await
                .unwrap();
        }
        for (l, t) in [(&l0, 10), (&l1, 20), (&l0, 30)] {
            tracking
                .store
                .record_click(&ClickEvent {
                    tenant_id: "acme".into(),
                    msg_id: msg.clone(),
                    link_id: l.clone(),
                    clicked_at_ms: t,
                    ip_hash: None,
                    ua_hash: None,
                })
                .await
                .unwrap();
        }
        let req = Request::builder()
            .uri("/tracking/msg/m1/engagement")
            .body(Body::empty())
            .unwrap();
        let resp = engagement_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["opens"], 2);
        let by_link = v["clicks_by_link"].as_array().unwrap();
        // Sorted desc by count → L0 first.
        assert_eq!(by_link[0]["link_id"], "L0");
        assert_eq!(by_link[0]["count"], 2);
        assert_eq!(by_link[1]["link_id"], "L1");
        assert_eq!(by_link[1]["count"], 1);
    }

    #[tokio::test]
    async fn engagement_404s_when_tracking_disabled() {
        let state = Arc::new(AdminState::new("token".into()));
        let req = Request::builder()
            .uri("/tracking/msg/m1/engagement")
            .body(Body::empty())
            .unwrap();
        let resp = engagement_router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
