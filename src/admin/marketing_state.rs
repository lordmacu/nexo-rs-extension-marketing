//! `/admin/marketing/state` — operator-facing on/off toggle.
//!
//! Two endpoints:
//! - `GET /admin/marketing/state` → current `{enabled,
//!   paused_reason, updated_at_ms}`.
//! - `PUT /admin/marketing/state` `{enabled, paused_reason?}`
//!   — flips the toggle + invalidates the cache so the next
//!   broker hop / tool dispatch / draft generate sees the new
//!   value.
//!
//! Bearer auth + `X-Tenant-Id` already applied by the
//! middleware. Response uses the canonical `{ok, result}`
//! envelope.

use std::sync::Arc;

use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use super::AdminState;
use crate::marketing_state::MarketingState;
use crate::tenant::TenantId;

pub async fn get_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
) -> Response {
    let Some(cache) = state.marketing_state.as_ref() else {
        return service_unavailable();
    };
    let now_ms = Utc::now().timestamp_millis();
    let s = cache.get(tenant_id.as_str(), now_ms).await;
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "result": { "state": s } })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct PutBody {
    pub enabled: bool,
    #[serde(default)]
    pub paused_reason: Option<String>,
}

pub async fn put_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<PutBody>,
) -> Response {
    let Some(cache) = state.marketing_state.as_ref() else {
        return service_unavailable();
    };
    let now_ms = Utc::now().timestamp_millis();
    let new_state = MarketingState {
        tenant_id: tenant_id.as_str().to_string(),
        enabled: body.enabled,
        // Empty string treated as None so the operator can
        // clear the reason from the UI without the column
        // round-tripping a sentinel value.
        paused_reason: body
            .paused_reason
            .filter(|s| !s.trim().is_empty()),
        updated_at_ms: now_ms,
    };
    if let Err(e) = cache.store().put(&new_state).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "ok": false,
                "error": { "code": "store_error", "message": e.to_string() },
            })),
        )
            .into_response();
    }
    cache.invalidate(tenant_id.as_str()).await;
    tracing::info!(
        target: "extension.marketing.state",
        tenant_id = %tenant_id.as_str(),
        enabled = new_state.enabled,
        reason = ?new_state.paused_reason,
        "marketing_state toggled",
    );
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "result": { "state": new_state } })),
    )
        .into_response()
}

fn service_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "ok": false,
            "error": {
                "code": "marketing_state_disabled",
                "message": "marketing_state cache is not configured for this tenant",
            },
        })),
    )
        .into_response()
}
