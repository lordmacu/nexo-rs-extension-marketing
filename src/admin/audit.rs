//! `GET /audit` — protected query endpoint over the AI
//! decision audit log (M15.23.c).
//!
//! Bearer auth + `X-Tenant-Id` already applied by the
//! middleware. The handler reads four optional query params:
//!
//! - `lead_id` — match exactly. Pulls one lead's full
//!   timeline.
//! - `kind` — filter by audit-event kind label
//!   (`routing_decided`, `lead_transitioned`,
//!   `notification_published`).
//! - `since_ms` — wall-clock lower bound (inclusive).
//! - `limit` — row cap. Defaults to 100; clamped at 1000 by
//!   the SDK store.
//!
//! Returns `{ events: [...], count: N }` so the operator UI
//! pages without re-counting client-side.

use std::sync::Arc;

use axum::{
    extract::{Extension, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use nexo_microapp_sdk::events::ListFilter;
use serde::Deserialize;
use serde_json::json;

use super::AdminState;
use crate::tenant::TenantId;

/// Query string the handler accepts. Every field is
/// optional; missing fields turn into "don't constrain".
#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    /// Filter by lead id.
    pub lead_id: Option<String>,
    /// Filter by audit-event kind.
    pub kind: Option<String>,
    /// Lower bound on `at_ms`.
    pub since_ms: Option<u64>,
    /// Row cap (default 100, max 1000).
    pub limit: Option<u32>,
}

/// `GET /audit?lead_id=&kind=&since_ms=&limit=`
pub async fn handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Query(q): Query<AuditQuery>,
) -> Response {
    let Some(audit) = state.audit.as_ref() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": { "code": "audit_disabled" } })),
        )
            .into_response();
    };

    let limit = q.limit.unwrap_or(100).min(1000) as usize;
    let filter = ListFilter {
        agent_id: q.lead_id.clone(),
        kind: q.kind.clone(),
        tenant_id: Some(tenant_id.as_str().to_string()),
        since_ms: q.since_ms,
        limit,
    };

    match audit.list(&filter).await {
        Ok(rows) => Json(json!({
            "events": rows,
            "count": rows.len(),
            "filter": {
                "lead_id": q.lead_id,
                "kind": q.kind,
                "since_ms": q.since_ms,
                "limit": limit,
            },
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(
                json!({ "error": { "code": "audit_store", "detail": e.to_string() } }),
            ),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{AuditEvent, AuditLog, AUDIT_TABLE};
    use axum::{
        body::{to_bytes, Body},
        http::Request,
        routing::get as axum_get,
        Router,
    };
    use nexo_microapp_sdk::events::EventStore;
    use tower::ServiceExt;

    async fn fresh_state_with_audit() -> Arc<AdminState> {
        let store = EventStore::<AuditEvent>::open_memory(AUDIT_TABLE)
            .await
            .unwrap();
        let log = AuditLog::new(Arc::new(store));
        // Seed 3 mixed rows.
        log.record(AuditEvent::RoutingDecided {
            tenant_id: "acme".into(),
            lead_id: Some("l1".into()),
            from_email: "x@y.com".into(),
            chosen_seller_id: Some("pedro".into()),
            rule_id: Some("warm-corp".into()),
            why: vec![],
            score: 65,
            score_reasons: vec![],
            at_ms: 10,
        })
        .await
        .unwrap();
        log.record(AuditEvent::LeadTransitioned {
            tenant_id: "acme".into(),
            lead_id: "l1".into(),
            from: "engaged".into(),
            to: "meeting_scheduled".into(),
            reason: "demo".into(),
            at_ms: 20,
        })
        .await
        .unwrap();
        log.record(AuditEvent::NotificationPublished {
            tenant_id: "acme".into(),
            lead_id: "l1".into(),
            seller_id: "pedro".into(),
            notification_kind: "lead_created".into(),
            channel: "whatsapp".into(),
            at_ms: 30,
        })
        .await
        .unwrap();
        // Globex row that must NOT leak across tenants.
        log.record(AuditEvent::RoutingDecided {
            tenant_id: "globex".into(),
            lead_id: Some("g1".into()),
            from_email: "z@y.com".into(),
            chosen_seller_id: Some("ana".into()),
            rule_id: None,
            why: vec![],
            score: 0,
            score_reasons: vec![],
            at_ms: 40,
        })
        .await
        .unwrap();
        Arc::new(
            AdminState::new("token".into()).with_audit(Arc::new(log)),
        )
    }

    fn router(state: Arc<AdminState>) -> Router {
        Router::new()
            .route("/audit", axum_get(handler))
            .layer(axum::Extension(TenantId::new("acme").unwrap()))
            .with_state(state)
    }

    #[tokio::test]
    async fn returns_all_rows_for_tenant_when_no_filters() {
        let state = fresh_state_with_audit().await;
        let req = Request::builder()
            .uri("/audit")
            .body(Body::empty())
            .unwrap();
        let resp = router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Tenant scoping kicks in — globex row never leaks.
        assert_eq!(v["count"], 3);
        let kinds: Vec<&str> = v["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"routing_decided"));
        assert!(kinds.contains(&"lead_transitioned"));
        assert!(kinds.contains(&"notification_published"));
    }

    #[tokio::test]
    async fn filters_by_kind() {
        let state = fresh_state_with_audit().await;
        let req = Request::builder()
            .uri("/audit?kind=lead_transitioned")
            .body(Body::empty())
            .unwrap();
        let resp = router(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["events"][0]["kind"], "lead_transitioned");
    }

    #[tokio::test]
    async fn filters_by_lead_id() {
        let state = fresh_state_with_audit().await;
        let req = Request::builder()
            .uri("/audit?lead_id=l1")
            .body(Body::empty())
            .unwrap();
        let resp = router(state).oneshot(req).await.unwrap();
        let body = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["count"], 3);
    }

    #[tokio::test]
    async fn filters_by_since_ms() {
        let state = fresh_state_with_audit().await;
        let req = Request::builder()
            .uri("/audit?since_ms=20")
            .body(Body::empty())
            .unwrap();
        let resp = router(state).oneshot(req).await.unwrap();
        let body = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        // Two rows ≥ 20 (transitioned + notification) — globex
        // at_ms=40 doesn't leak via tenant scoping.
        assert_eq!(v["count"], 2);
    }

    #[tokio::test]
    async fn returns_404_when_audit_disabled() {
        let state = Arc::new(AdminState::new("token".into()));
        let r = Router::new()
            .route("/audit", axum_get(handler))
            .layer(axum::Extension(TenantId::new("acme").unwrap()))
            .with_state(state);
        let req = Request::builder()
            .uri("/audit")
            .body(Body::empty())
            .unwrap();
        let resp = r.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
