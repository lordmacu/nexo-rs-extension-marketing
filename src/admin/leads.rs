//! `/leads` read endpoints.
//!
//! - `GET /leads` — list. Optional `?state=engaged&limit=50`.
//! - `GET /leads/:lead_id` — single fetch. 404 when missing.
//!
//! Tenant id comes from the auth middleware via
//! `Extension<TenantId>` — the URL never carries it.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use super::AdminState;
use crate::error::MarketingError;
use crate::tenant::TenantId;
use nexo_tool_meta::marketing::{LeadId, LeadState};

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 500;

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// `cold | engaged | meeting_scheduled | qualified | lost`.
    /// Filter out when omitted; the store's count_by_state +
    /// list_due_for_followup cover the use cases that need a
    /// state filter today.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

pub async fn list_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Query(q): Query<ListQuery>,
) -> Response {
    let store = match state.lookup_store(&tenant_id) {
        Some(s) => s,
        None => return server_error("store_missing", "tenant store not mounted"),
    };
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);

    // For v1 the only "list" path is "leads due for followup
    // <= now"; the dashboard's filtered list lands when M15.24
    // wires SELECT helpers. We expose count_by_state below so
    // operators get headline numbers immediately.
    if let Some(state_filter) = &q.state {
        let parsed = parse_state(state_filter);
        match parsed {
            Some(s) => match store.count_by_state(s).await {
                Ok(n) => {
                    return ok(json!({
                        "filter": { "state": state_filter },
                        "count": n,
                    }));
                }
                Err(e) => return marketing_error(e),
            },
            None => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "invalid_state",
                    &format!("state {state_filter:?} not one of cold|engaged|meeting_scheduled|qualified|lost"),
                );
            }
        }
    }

    // No filter → return the followup-due list (most useful
    // single-call endpoint for the operator dashboard's
    // landing view).
    let now = chrono::Utc::now().timestamp_millis();
    match store.list_due_for_followup(now, limit).await {
        Ok(leads) => ok(json!({
            "leads": leads,
            "now_ms": now,
            "limit": limit,
        })),
        Err(e) => marketing_error(e),
    }
}

pub async fn get_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(lead_id): Path<String>,
) -> Response {
    let store = match state.lookup_store(&tenant_id) {
        Some(s) => s,
        None => return server_error("store_missing", "tenant store not mounted"),
    };
    match store.get(&LeadId(lead_id.clone())).await {
        Ok(Some(lead)) => ok(json!({ "lead": lead })),
        Ok(None) => error(
            StatusCode::NOT_FOUND,
            "lead_not_found",
            &format!("no lead with id {lead_id:?} for the active tenant"),
        ),
        Err(e) => marketing_error(e),
    }
}

/// `GET /leads/:lead_id/thread` — chronological message list
/// for the lead. 404 when the lead doesn't exist (so the UI can
/// distinguish "no thread yet" from "lead missing"). Empty
/// thread is a legitimate `200` with `messages: []` — leads
/// created before persistence landed have no rows.
pub async fn thread_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(lead_id): Path<String>,
) -> Response {
    let store = match state.lookup_store(&tenant_id) {
        Some(s) => s,
        None => return server_error("store_missing", "tenant store not mounted"),
    };
    let id = LeadId(lead_id.clone());
    // Verify the lead exists in this tenant first — defense-in-
    // depth + clearer 404 surface for the UI.
    match store.get(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return error(
                StatusCode::NOT_FOUND,
                "lead_not_found",
                &format!("no lead with id {lead_id:?} for the active tenant"),
            );
        }
        Err(e) => return marketing_error(e),
    }
    match store.list_thread(&id).await {
        Ok(messages) => ok(json!({
            "lead_id": lead_id,
            "messages": messages,
            "count": messages.len(),
        })),
        Err(e) => marketing_error(e),
    }
}

// ── M15.21 slice 1 — draft CRUD endpoints ──────────────────────

#[derive(Debug, Deserialize)]
pub struct DraftListQuery {
    /// `pending | approved | rejected`. Omitted ⇒ every status.
    #[serde(default)]
    pub status: Option<String>,
}

/// `GET /leads/:lead_id/drafts?status=pending`
pub async fn list_drafts_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(lead_id): Path<String>,
    Query(q): Query<DraftListQuery>,
) -> Response {
    let store = match state.lookup_store(&tenant_id) {
        Some(s) => s,
        None => return server_error("store_missing", "tenant store not mounted"),
    };
    let id = LeadId(lead_id.clone());
    if matches!(store.get(&id).await, Ok(None)) {
        return error(
            StatusCode::NOT_FOUND,
            "lead_not_found",
            &format!("no lead with id {lead_id:?} for the active tenant"),
        );
    }
    let status_filter = q.status.as_deref().and_then(parse_draft_status);
    if q.status.is_some() && status_filter.is_none() {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_status",
            "status must be one of: pending, approved, rejected",
        );
    }
    match store.list_drafts(&id, status_filter).await {
        Ok(drafts) => ok(json!({
            "lead_id": lead_id,
            "drafts": drafts,
            "count": drafts.len(),
        })),
        Err(e) => marketing_error(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateDraftBody {
    /// Required — body of the proposed reply. Subject is
    /// inherited from the lead row at send time (M15.21
    /// slice 2); operator can override later via PUT.
    pub body: String,
    /// Optional operator-facing label. Defaults to "AI" so
    /// the lead drawer reads consistently regardless of
    /// who authored the draft (LLM tool / operator manual /
    /// auto-template).
    #[serde(default)]
    pub from_label: Option<String>,
}

/// `POST /leads/:lead_id/drafts` — body `{ body, from_label? }`.
/// Generates a fresh `draft-<uuidv4>` message id, inserts as
/// `direction=draft / status=pending`. Returns the created
/// row so the caller has the id for subsequent PUT/DELETE.
pub async fn create_draft_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(lead_id): Path<String>,
    Json(payload): Json<CreateDraftBody>,
) -> Response {
    if payload.body.trim().is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "empty_body",
            "draft body must be non-empty",
        );
    }
    let store = match state.lookup_store(&tenant_id) {
        Some(s) => s,
        None => return server_error("store_missing", "tenant store not mounted"),
    };
    let id = LeadId(lead_id.clone());
    if matches!(store.get(&id).await, Ok(None)) {
        return error(
            StatusCode::NOT_FOUND,
            "lead_not_found",
            &format!("no lead with id {lead_id:?} for the active tenant"),
        );
    }
    let message_id = format!("draft-{}", uuid::Uuid::new_v4());
    let from_label = payload
        .from_label
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "AI".into());
    let now_ms = chrono::Utc::now().timestamp_millis();
    let new_msg = crate::lead::NewThreadMessage {
        message_id: message_id.clone(),
        direction: crate::lead::MessageDirection::Draft,
        from_label,
        body: payload.body,
        at_ms: now_ms,
        draft_status: Some(crate::lead::DraftStatus::Pending),
    };
    if let Err(e) = store.append_thread_message(&id, new_msg).await {
        return marketing_error(e);
    }
    // Read back so the response carries the canonical row.
    let drafts = match store
        .list_drafts(&id, Some(crate::lead::DraftStatus::Pending))
        .await
    {
        Ok(rows) => rows,
        Err(e) => return marketing_error(e),
    };
    let row = drafts.into_iter().find(|d| d.id == message_id);
    match row {
        Some(d) => (
            StatusCode::CREATED,
            Json(json!({ "ok": true, "result": { "draft": d } })),
        )
            .into_response(),
        None => server_error(
            "draft_persist_inconsistency",
            "draft inserted but read-back failed to find it",
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateDraftBody {
    pub body: String,
}

/// `PUT /leads/:lead_id/drafts/:message_id` — body `{ body }`.
/// Only succeeds when the draft is `pending`; approved or
/// rejected drafts return 409.
pub async fn update_draft_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path((lead_id, message_id)): Path<(String, String)>,
    Json(payload): Json<UpdateDraftBody>,
) -> Response {
    if payload.body.trim().is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "empty_body",
            "draft body must be non-empty",
        );
    }
    let store = match state.lookup_store(&tenant_id) {
        Some(s) => s,
        None => return server_error("store_missing", "tenant store not mounted"),
    };
    let id = LeadId(lead_id);
    match store
        .update_draft_body(&id, &message_id, &payload.body)
        .await
    {
        Ok(0) => error(
            StatusCode::CONFLICT,
            "draft_locked",
            "draft is approved/rejected or doesn't exist for this lead",
        ),
        Ok(_) => ok(json!({ "draft_id": message_id })),
        Err(e) => marketing_error(e),
    }
}

/// `POST /leads/:lead_id/drafts/:message_id/reject` — sets
/// `draft_status = rejected`. Idempotent on already-rejected
/// rows (returns 409 once locked). The approve handler lands
/// in M15.21 slice 2 alongside the publisher wiring.
pub async fn reject_draft_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path((lead_id, message_id)): Path<(String, String)>,
) -> Response {
    let store = match state.lookup_store(&tenant_id) {
        Some(s) => s,
        None => return server_error("store_missing", "tenant store not mounted"),
    };
    let id = LeadId(lead_id);
    match store
        .set_draft_status(&id, &message_id, crate::lead::DraftStatus::Rejected)
        .await
    {
        Ok(0) => error(
            StatusCode::CONFLICT,
            "draft_not_pending",
            "draft is already approved/rejected or doesn't exist",
        ),
        Ok(_) => ok(json!({ "draft_id": message_id, "status": "rejected" })),
        Err(e) => marketing_error(e),
    }
}

/// `DELETE /leads/:lead_id/drafts/:message_id` — operator
/// dismisses the draft entirely. Hard delete; tenant-scoped;
/// only `direction=draft` rows match.
pub async fn delete_draft_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path((lead_id, message_id)): Path<(String, String)>,
) -> Response {
    let store = match state.lookup_store(&tenant_id) {
        Some(s) => s,
        None => return server_error("store_missing", "tenant store not mounted"),
    };
    let id = LeadId(lead_id);
    match store.delete_draft(&id, &message_id).await {
        Ok(0) => error(
            StatusCode::NOT_FOUND,
            "draft_not_found",
            "no draft with that id on the active lead",
        ),
        Ok(_) => ok(json!({ "draft_id": message_id, "deleted": true })),
        Err(e) => marketing_error(e),
    }
}

fn parse_draft_status(s: &str) -> Option<crate::lead::DraftStatus> {
    match s {
        "pending" => Some(crate::lead::DraftStatus::Pending),
        "approved" => Some(crate::lead::DraftStatus::Approved),
        "rejected" => Some(crate::lead::DraftStatus::Rejected),
        _ => None,
    }
}

fn parse_state(s: &str) -> Option<LeadState> {
    match s {
        "cold" => Some(LeadState::Cold),
        "engaged" => Some(LeadState::Engaged),
        "meeting_scheduled" => Some(LeadState::MeetingScheduled),
        "qualified" => Some(LeadState::Qualified),
        "lost" => Some(LeadState::Lost),
        _ => None,
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

fn marketing_error(e: MarketingError) -> Response {
    server_error("internal", &e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::router;
    use crate::lead::{LeadStore, NewLead};
    use axum::body::{to_bytes, Body};
    use axum::http::header;
    use axum::http::Request;
    use nexo_tool_meta::marketing::{PersonId, SellerId};
    use std::path::PathBuf;
    use tower::util::ServiceExt;

    async fn build_state_with_lead() -> Arc<AdminState> {
        let store = LeadStore::open(
            PathBuf::from(":memory:"),
            TenantId::new("acme").unwrap(),
        )
        .await
        .unwrap();
        store
            .create(NewLead {
                id: LeadId("l-1".into()),
                thread_id: "th-1".into(),
                subject: "Re: cot".into(),
                person_id: PersonId("p".into()),
                seller_id: SellerId("v".into()),
                last_activity_ms: 1,
                score: 0,
                topic_tags: vec![],
                why_routed: vec!["fixture".into()],
            })
            .await
            .unwrap();
        Arc::new(AdminState::new("secret".into()).with_store(Arc::new(store)))
    }

    async fn body_to_json(resp: Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn req(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer secret")
            .header("X-Tenant-Id", "acme")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn get_existing_lead_returns_200() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app.oneshot(req("/leads/l-1")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["result"]["lead"]["id"], "l-1");
    }

    #[tokio::test]
    async fn get_missing_lead_returns_404() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app.oneshot(req("/leads/ghost")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn list_state_filter_returns_count() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app.oneshot(req("/leads?state=cold")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["count"], 1);
    }

    #[tokio::test]
    async fn list_invalid_state_returns_400() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app.oneshot(req("/leads?state=foo")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn list_no_filter_returns_due_envelope() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app.oneshot(req("/leads")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert!(v["result"]["leads"].is_array());
        assert!(v["result"]["limit"].is_u64());
    }

    #[tokio::test]
    async fn thread_returns_chronological_messages() {
        use crate::lead::{MessageDirection, NewThreadMessage};
        let state = build_state_with_lead().await;
        // Reach in via the same Arc to seed messages.
        let store = state.lookup_store(&TenantId::new("acme").unwrap()).unwrap();
        store
            .append_thread_message(
                &LeadId("l-1".into()),
                NewThreadMessage {
                    message_id: "m1".into(),
                    direction: MessageDirection::Inbound,
                    from_label: "Cliente".into(),
                    body: "Hola".into(),
                    at_ms: 100,
                    draft_status: None,
                },
            )
            .await
            .unwrap();
        store
            .append_thread_message(
                &LeadId("l-1".into()),
                NewThreadMessage {
                    message_id: "m2".into(),
                    direction: MessageDirection::Outbound,
                    from_label: "Pedro".into(),
                    body: "Saludos".into(),
                    at_ms: 200,
                    draft_status: None,
                },
            )
            .await
            .unwrap();
        let app = router(state);
        let resp = app.oneshot(req("/leads/l-1/thread")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["result"]["count"], 2);
        let msgs = v["result"]["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["body"], "Hola");
        assert_eq!(msgs[1]["direction"], "outbound");
    }

    #[tokio::test]
    async fn thread_returns_empty_for_lead_with_no_messages() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app.oneshot(req("/leads/l-1/thread")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["count"], 0);
        assert_eq!(v["result"]["messages"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn thread_for_missing_lead_returns_404() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app.oneshot(req("/leads/ghost/thread")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
