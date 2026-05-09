//! `/admin/scoring/config` — tenant-tunable scoring weights +
//! thresholds + keyword lists. Operator overrides the bundled
//! defaults via PUT; broker hop reads via the cached config so
//! changes land on the very next inbound.
//!
//! Endpoints:
//! - `GET    /admin/scoring/config`
//! - `PUT    /admin/scoring/config`  (full ScoringConfig body)
//! - `DELETE /admin/scoring/config`  (reset to bundled defaults)

use std::sync::Arc;

use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde_json::json;

use super::AdminState;
use crate::scoring::ScoringConfig;
use crate::tenant::TenantId;

pub async fn get_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
) -> Response {
    let Some(cache) = state.scoring.as_ref() else {
        return service_unavailable();
    };
    let cfg = cache.get_or_load(tenant_id.as_str()).await;
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "result": { "config": &*cfg },
        })),
    )
        .into_response()
}

pub async fn put_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<ScoringConfig>,
) -> Response {
    let Some(cache) = state.scoring.as_ref() else {
        return service_unavailable();
    };
    let now_ms = Utc::now().timestamp_millis();
    if let Err(e) = cache.store().put(tenant_id.as_str(), &body, now_ms).await {
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
        target: "extension.marketing.scoring",
        tenant_id = %tenant_id.as_str(),
        "scoring config updated",
    );
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "result": { "config": body } })),
    )
        .into_response()
}

pub async fn delete_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
) -> Response {
    let Some(cache) = state.scoring.as_ref() else {
        return service_unavailable();
    };
    if let Err(e) = cache.store().reset(tenant_id.as_str()).await {
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
    let defaults = ScoringConfig::default();
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "result": { "config": defaults, "reset_to_defaults": true },
        })),
    )
        .into_response()
}

fn service_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "ok": false,
            "error": {
                "code": "scoring_disabled",
                "message": "scoring config cache is not configured for this tenant",
            },
        })),
    )
        .into_response()
}
