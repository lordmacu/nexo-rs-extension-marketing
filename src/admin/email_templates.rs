//! `/admin/email-templates` — CRUD over per-tenant email
//! templates. Bearer auth + X-Tenant-Id middleware already
//! applied. Standard `{ok, result}` envelope.
//!
//! Endpoints:
//!   GET    /admin/email-templates          → list
//!   GET    /admin/email-templates/:id      → fetch one
//!   POST   /admin/email-templates          → create
//!   PUT    /admin/email-templates/:id      → update
//!   DELETE /admin/email-templates/:id      → remove
//!   POST   /admin/email-templates/:id/render → server-side
//!     render with caller-supplied vars (preview / debug).

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use super::AdminState;
use crate::email_template::{
    render_template, EmailBlock, EmailTemplateStore, TemplateStoreError,
};
use crate::tenant::TenantId;

pub async fn list_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
) -> Response {
    let Some(store) = state.email_template_store.as_ref() else {
        return service_unavailable();
    };
    match store.list(tenant_id.as_str()).await {
        Ok(rows) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "result": { "templates": rows, "count": rows.len() },
            })),
        )
            .into_response(),
        Err(e) => store_error_response(e),
    }
}

pub async fn get_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
) -> Response {
    let Some(store) = state.email_template_store.as_ref() else {
        return service_unavailable();
    };
    match store.get(tenant_id.as_str(), &id).await {
        Ok(Some(t)) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "result": { "template": t } })),
        )
            .into_response(),
        Ok(None) => not_found(&id),
        Err(e) => store_error_response(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    pub name: String,
    #[serde(default)]
    pub blocks: Vec<EmailBlock>,
}

pub async fn create_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<CreateBody>,
) -> Response {
    let Some(store) = state.email_template_store.as_ref() else {
        return service_unavailable();
    };
    let now_ms = Utc::now().timestamp_millis();
    match store
        .create(tenant_id.as_str(), &body.name, &body.blocks, now_ms)
        .await
    {
        Ok(t) => {
            audit_change(
                &state,
                tenant_id.as_str(),
                "email_template_added",
                None,
                serde_json::to_value(&t).ok(),
                now_ms,
            )
            .await;
            (
                StatusCode::CREATED,
                Json(json!({ "ok": true, "result": { "template": t } })),
            )
                .into_response()
        }
        Err(e) => store_error_response(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct UpdateBody {
    pub name: String,
    #[serde(default)]
    pub blocks: Vec<EmailBlock>,
}

pub async fn update_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> Response {
    let Some(store) = state.email_template_store.as_ref() else {
        return service_unavailable();
    };
    let now_ms = Utc::now().timestamp_millis();
    let before = store.get(tenant_id.as_str(), &id).await.ok().flatten();
    match store
        .update(tenant_id.as_str(), &id, &body.name, &body.blocks, now_ms)
        .await
    {
        Ok(t) => {
            audit_change(
                &state,
                tenant_id.as_str(),
                "email_template_updated",
                before.and_then(|b| serde_json::to_value(b).ok()),
                serde_json::to_value(&t).ok(),
                now_ms,
            )
            .await;
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "result": { "template": t } })),
            )
                .into_response()
        }
        Err(e) => store_error_response(e),
    }
}

pub async fn delete_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
) -> Response {
    let Some(store) = state.email_template_store.as_ref() else {
        return service_unavailable();
    };
    match store.delete(tenant_id.as_str(), &id).await {
        Ok(true) => {
            let now_ms = Utc::now().timestamp_millis();
            audit_change(
                &state,
                tenant_id.as_str(),
                "email_template_removed",
                Some(json!({ "id": id })),
                None,
                now_ms,
            )
            .await;
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "result": { "deleted": id } })),
            )
                .into_response()
        }
        Ok(false) => not_found(&id),
        Err(e) => store_error_response(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct RenderBody {
    /// Variable map for `{{name}}` style placeholders. Empty
    /// map → unsubstituted placeholders pass through to the
    /// rendered HTML.
    #[serde(default)]
    pub vars: HashMap<String, String>,
}

pub async fn render_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(id): Path<String>,
    Json(body): Json<RenderBody>,
) -> Response {
    let Some(store) = state.email_template_store.as_ref() else {
        return service_unavailable();
    };
    match store.get(tenant_id.as_str(), &id).await {
        Ok(Some(t)) => {
            let html = render_template(&t.blocks, &body.vars);
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "result": {
                        "id": t.id,
                        "html": html,
                        "vars_used": body.vars,
                    },
                })),
            )
                .into_response()
        }
        Ok(None) => not_found(&id),
        Err(e) => store_error_response(e),
    }
}

// ── Helpers ──────────────────────────────────────────────────────

async fn audit_change(
    state: &AdminState,
    tenant_id: &str,
    scope: &str,
    before_json: Option<serde_json::Value>,
    after_json: Option<serde_json::Value>,
    at_ms: i64,
) {
    if let Some(audit) = state.audit.as_ref() {
        audit
            .record_config_change(
                tenant_id,
                scope,
                before_json,
                after_json,
                at_ms as u64,
            )
            .await;
    }
}

fn service_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "ok": false,
            "error": {
                "code": "email_templates_disabled",
                "message": "email_template_store is not configured for this tenant",
            },
        })),
    )
        .into_response()
}

fn not_found(id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "ok": false,
            "error": {
                "code": "template_not_found",
                "message": format!("no template with id {id:?}"),
            },
        })),
    )
        .into_response()
}

fn store_error_response(e: TemplateStoreError) -> Response {
    let (status, code) = match &e {
        TemplateStoreError::NotFound(_) => (StatusCode::NOT_FOUND, "template_not_found"),
        TemplateStoreError::MissingName => (StatusCode::BAD_REQUEST, "missing_name"),
        TemplateStoreError::InvalidJson(_) => (StatusCode::BAD_REQUEST, "invalid_blocks"),
        TemplateStoreError::Sqlite(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "store_error")
        }
    };
    (
        status,
        Json(json!({
            "ok": false,
            "error": { "code": code, "message": e.to_string() },
        })),
    )
        .into_response()
}
