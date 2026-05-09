//! M15.24 — `/telemetry` aggregate snapshot.
//!
//! **F29 sweep:** marketing-specific by design. Aggregates
//! CRM counts (leads-by-state, drafts-pending, scraper hits).
//! Generic Prometheus-counter primitive is the right lift
//! candidate but lives upstream in `nexo-core`.
//!
//! Powers the operator's `/marketing/health` dashboard.
//! Single GET returns headline counts the operator
//! glances at:
//!
//! - Leads by state (cold / engaged / meeting_scheduled
//!   / qualified / lost).
//! - Drafts pending operator approval (M15.21 slice 1).
//! - Inbound + outbound thread messages over the last
//!   `window_hours` (default 24).
//!
//! Tenant-scoped via the auth-middleware-stamped
//! `Extension<TenantId>`. Cross-tenant counts return zero
//! at the store layer.

use std::sync::Arc;

use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use nexo_tool_meta::marketing::LeadState;

use super::AdminState;
use crate::lead::MessageDirection;
use crate::tenant::TenantId;

const DEFAULT_WINDOW_HOURS: u32 = 24;
const MAX_WINDOW_HOURS: u32 = 24 * 30; // 30 days hard cap

#[derive(Debug, Deserialize)]
pub struct TelemetryQuery {
    /// Lookback window for the inbound / outbound message
    /// counters. Defaults to 24h; clamped to 30d.
    #[serde(default)]
    pub window_hours: Option<u32>,
}

/// `GET /telemetry?window_hours=` — aggregate snapshot
/// keyed off the active tenant. Empty tenant → zeros
/// everywhere (legitimate "no leads yet" state).
pub async fn handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Query(q): Query<TelemetryQuery>,
) -> Response {
    let store = match state.lookup_store(&tenant_id) {
        Some(s) => s,
        None => return server_error("store_missing", "tenant store not mounted"),
    };
    let window_hours = q
        .window_hours
        .unwrap_or(DEFAULT_WINDOW_HOURS)
        .clamp(1, MAX_WINDOW_HOURS);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let since_ms = now_ms - (window_hours as i64) * 3_600_000;

    // ── Leads by state ──────────────────────────────────────
    let mut leads_by_state = serde_json::Map::new();
    for state_kind in [
        LeadState::Cold,
        LeadState::Engaged,
        LeadState::MeetingScheduled,
        LeadState::Qualified,
        LeadState::Lost,
    ] {
        match store.count_by_state(state_kind).await {
            Ok(n) => {
                leads_by_state
                    .insert(state_label(state_kind).into(), json!(n));
            }
            Err(e) => return server_error("count_by_state", &e.to_string()),
        }
    }

    // ── Drafts pending ──────────────────────────────────────
    let drafts_pending = match store.count_drafts_pending().await {
        Ok(n) => n,
        Err(e) => return server_error("count_drafts_pending", &e.to_string()),
    };

    // ── Inbound / outbound window ───────────────────────────
    let inbound = match store
        .count_messages_by_direction_since(MessageDirection::Inbound, since_ms)
        .await
    {
        Ok(n) => n,
        Err(e) => return server_error("count_inbound", &e.to_string()),
    };
    let outbound = match store
        .count_messages_by_direction_since(
            MessageDirection::Outbound,
            since_ms,
        )
        .await
    {
        Ok(n) => n,
        Err(e) => return server_error("count_outbound", &e.to_string()),
    };

    ok(json!({
        "tenant_id": tenant_id.as_str(),
        "now_ms": now_ms,
        "window_hours": window_hours,
        "since_ms": since_ms,
        "leads_by_state": leads_by_state,
        "drafts_pending": drafts_pending,
        "inbound_messages": inbound,
        "outbound_messages": outbound,
    }))
}

fn state_label(s: LeadState) -> &'static str {
    match s {
        LeadState::Cold => "cold",
        LeadState::Engaged => "engaged",
        LeadState::MeetingScheduled => "meeting_scheduled",
        LeadState::Qualified => "qualified",
        LeadState::Lost => "lost",
    }
}

fn ok(result: serde_json::Value) -> Response {
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "result": result })),
    )
        .into_response()
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({ "ok": false, "error": { "code": code, "message": message } })),
    )
        .into_response()
}

fn server_error(code: &str, message: &str) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::router;
    use crate::lead::{LeadStore, NewLead, NewThreadMessage};
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request};
    use nexo_tool_meta::marketing::{LeadId, PersonId, SellerId};
    use std::path::PathBuf;
    use tower::util::ServiceExt;

    async fn build_state_with_seed() -> Arc<AdminState> {
        let store = LeadStore::open(
            PathBuf::from(":memory:"),
            TenantId::new("acme").unwrap(),
        )
        .await
        .unwrap();
        // Seed: 2 cold, 1 engaged.
        for (id, st_idx) in [("c1", 0), ("c2", 0), ("e1", 1)] {
            let lead = NewLead {
                id: LeadId(id.into()),
                thread_id: format!("th-{id}"),
                subject: "s".into(),
                person_id: PersonId("p".into()),
                seller_id: SellerId("v".into()),
                last_activity_ms: 1,
                score: 0,
                topic_tags: vec![],
                why_routed: vec![],
            };
            let _ = store.create(lead).await.unwrap();
            if st_idx == 1 {
                let _ = store
                    .transition(&LeadId(id.into()), LeadState::Engaged)
                    .await;
            }
        }
        // Two pending drafts on c1, one rejected.
        for (msg_id, status) in [
            ("d-1", crate::lead::DraftStatus::Pending),
            ("d-2", crate::lead::DraftStatus::Pending),
            ("d-3", crate::lead::DraftStatus::Rejected),
        ] {
            store
                .append_thread_message(
                    &LeadId("c1".into()),
                    NewThreadMessage {
                        message_id: msg_id.into(),
                        direction: MessageDirection::Draft,
                        from_label: "AI".into(),
                        body: "b".into(),
                        at_ms: 1,
                        draft_status: Some(status), subject: None,
                        signature: None,
                    },
                )
                .await
                .unwrap();
        }
        // Window-relevant inbound + outbound: at_ms must
        // fall within the default 24h window so the test
        // doesn't drift across midnights.
        let now = chrono::Utc::now().timestamp_millis();
        store
            .append_thread_message(
                &LeadId("c1".into()),
                NewThreadMessage {
                    message_id: "in-recent".into(),
                    direction: MessageDirection::Inbound,
                    from_label: "Cliente".into(),
                    body: "hi".into(),
                    at_ms: now - 1000,
                    draft_status: None, subject: None,
                    signature: None,
                },
            )
            .await
            .unwrap();
        store
            .append_thread_message(
                &LeadId("c1".into()),
                NewThreadMessage {
                    message_id: "out-recent".into(),
                    direction: MessageDirection::Outbound,
                    from_label: "Pedro".into(),
                    body: "ok".into(),
                    at_ms: now - 500,
                    draft_status: None, subject: None,
                    signature: None,
                },
            )
            .await
            .unwrap();
        // Old inbound — outside the default 24h window but
        // still inside the 30-day window (max clamp is 720h
        // = 30d; seed at 25d so the 720h test picks it up
        // even with seed-vs-handler `now` drift).
        store
            .append_thread_message(
                &LeadId("c1".into()),
                NewThreadMessage {
                    message_id: "in-old".into(),
                    direction: MessageDirection::Inbound,
                    from_label: "Cliente".into(),
                    body: "old".into(),
                    at_ms: now - 25 * 24 * 3_600_000,
                    draft_status: None, subject: None,
                    signature: None,
                },
            )
            .await
            .unwrap();
        Arc::new(AdminState::new("secret".into()).with_store(Arc::new(store)))
    }

    fn req(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer secret")
            .header("X-Tenant-Id", "acme")
            .body(Body::empty())
            .unwrap()
    }

    async fn body_to_json(resp: Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn telemetry_returns_aggregate_snapshot() {
        let state = build_state_with_seed().await;
        let app = router(state);
        let resp = app.oneshot(req("/telemetry")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["ok"], true);
        let r = &v["result"];
        // Seed: c1 + c2 stayed cold; only e1 was transitioned.
        assert_eq!(r["leads_by_state"]["cold"], 2);
        assert_eq!(r["leads_by_state"]["engaged"], 1);
        assert_eq!(r["leads_by_state"]["meeting_scheduled"], 0);
        assert_eq!(r["leads_by_state"]["qualified"], 0);
        assert_eq!(r["leads_by_state"]["lost"], 0);
        assert_eq!(r["drafts_pending"], 2);
        // Default window is 24h — old inbound excluded.
        assert_eq!(r["inbound_messages"], 1);
        assert_eq!(r["outbound_messages"], 1);
        assert_eq!(r["window_hours"], 24);
    }

    #[tokio::test]
    async fn telemetry_honours_window_query() {
        let state = build_state_with_seed().await;
        let app = router(state);
        let resp = app
            .oneshot(req("/telemetry?window_hours=720"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        let r = &v["result"];
        // 30-day window picks up the old inbound row.
        assert_eq!(r["window_hours"], 720);
        assert!(r["inbound_messages"].as_i64().unwrap() >= 2);
    }

    #[tokio::test]
    async fn telemetry_clamps_window_above_30d() {
        let state = build_state_with_seed().await;
        let app = router(state);
        let resp = app
            .oneshot(req("/telemetry?window_hours=99999"))
            .await
            .unwrap();
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["window_hours"], 720);
    }

    #[tokio::test]
    async fn telemetry_clamps_window_to_minimum_one() {
        let state = build_state_with_seed().await;
        let app = router(state);
        let resp = app
            .oneshot(req("/telemetry?window_hours=0"))
            .await
            .unwrap();
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["window_hours"], 1);
    }
}
