//! `/admin/spam-filter/*` — tenant-customizable spam / promo
//! filter administration.
//!
//! Read + write paths share the `RulesCache` injected on
//! `AdminState`; every write invalidates the cache so the next
//! inbound through the broker hop picks up the fresh state
//! immediately.
//!
//! Bearer auth + `X-Tenant-Id` already applied by the middleware.
//!
//! Routes:
//! - `GET    /admin/spam-filter`              — config + every rule
//! - `PUT    /admin/spam-filter/config`       — strictness + thresholds
//! - `POST   /admin/spam-filter/rules`        — add a rule
//! - `DELETE /admin/spam-filter/rules/:id`    — delete a rule
//! - `POST   /admin/spam-filter/test`         — dry-run a raw `.eml`
//!
//! Response envelope is the canonical `{ok, result}` /
//! `{ok: false, error}` shape every other marketing endpoint
//! uses; the frontend's `call()` helper unwraps `result`.

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
use crate::spam_filter::{self, RuleKind, Strictness, ThresholdSet};
use crate::tenant::TenantId;

// ── GET /admin/spam-filter ────────────────────────────────────────

pub async fn get_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
) -> Response {
    let Some(cache) = state.spam_filter.as_ref() else {
        return service_unavailable();
    };
    let store = cache.store();
    let now_ms = Utc::now().timestamp_millis();
    let cfg = match store.get_config(tenant_id.as_str(), now_ms).await {
        Ok(c) => c,
        Err(e) => return store_error(e),
    };
    let rules = match store.list_rules(tenant_id.as_str()).await {
        Ok(r) => r,
        Err(e) => return store_error(e),
    };
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "result": {
                "config": cfg,
                "rules": rules,
                "rule_count": rules.len(),
            },
        })),
    )
        .into_response()
}

// ── PUT /admin/spam-filter/config ─────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PutConfigBody {
    pub strictness: Strictness,
    /// When omitted on a non-Custom strictness, the stored
    /// custom thresholds keep their previous values (we still
    /// accept partial updates so the UI doesn't have to re-
    /// submit the full struct on every preset switch).
    #[serde(default)]
    pub thresholds: Option<ThresholdSet>,
}

pub async fn put_config_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<PutConfigBody>,
) -> Response {
    let Some(cache) = state.spam_filter.as_ref() else {
        return service_unavailable();
    };
    let store = cache.store();
    let now_ms = Utc::now().timestamp_millis();
    let mut cfg = match store.get_config(tenant_id.as_str(), now_ms).await {
        Ok(c) => c,
        Err(e) => return store_error(e),
    };
    cfg.strictness = body.strictness;
    if let Some(t) = body.thresholds {
        cfg.thresholds = t;
    }
    cfg.updated_at_ms = now_ms;
    if let Err(e) = store.put_config(&cfg).await {
        return store_error(e);
    }
    cache.invalidate(tenant_id.as_str()).await;
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "result": { "config": cfg },
        })),
    )
        .into_response()
}

// ── POST /admin/spam-filter/rules ─────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AddRuleBody {
    pub kind: RuleKind,
    pub value: String,
    #[serde(default)]
    pub note: Option<String>,
}

pub async fn add_rule_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<AddRuleBody>,
) -> Response {
    let Some(cache) = state.spam_filter.as_ref() else {
        return service_unavailable();
    };
    let store = cache.store();
    let now_ms = Utc::now().timestamp_millis();
    let rule = match store
        .add_rule(
            tenant_id.as_str(),
            body.kind,
            &body.value,
            body.note.as_deref(),
            now_ms,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return store_error(e),
    };
    cache.invalidate(tenant_id.as_str()).await;
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "result": { "rule": rule },
        })),
    )
        .into_response()
}

// ── DELETE /admin/spam-filter/rules/:id ───────────────────────────

pub async fn delete_rule_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(rule_id): Path<i64>,
) -> Response {
    let Some(cache) = state.spam_filter.as_ref() else {
        return service_unavailable();
    };
    let store = cache.store();
    let removed = match store.delete_rule(tenant_id.as_str(), rule_id).await {
        Ok(r) => r,
        Err(e) => return store_error(e),
    };
    if !removed {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "ok": false,
                "error": {
                    "code": "rule_not_found",
                    "message": format!("rule {rule_id} not found for tenant"),
                },
            })),
        )
            .into_response();
    }
    cache.invalidate(tenant_id.as_str()).await;
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "result": { "deleted": rule_id } })),
    )
        .into_response()
}

// ── POST /admin/spam-filter/test ──────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TestBody {
    /// Raw RFC 5322 message. Either pass `raw_eml` directly or
    /// `subject` + `from_email` + `body` for a synthesised
    /// minimal message (used by the operator UI's "test the
    /// classifier" panel).
    #[serde(default)]
    pub raw_eml: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub from_email: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub html: Option<String>,
}

pub async fn test_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<TestBody>,
) -> Response {
    let Some(cache) = state.spam_filter.as_ref() else {
        return service_unavailable();
    };
    let now_ms = Utc::now().timestamp_millis();
    let resolved = cache.get_or_load(tenant_id.as_str(), now_ms).await;

    let (raw, subject, from) = match build_raw(&body) {
        Ok(t) => t,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "error": { "code": "bad_input", "message": msg },
                })),
            )
                .into_response();
        }
    };
    let classification =
        spam_filter::classify_with_rules(raw.as_bytes(), &subject, &from, &resolved);
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "result": {
                "verdict": classification.verdict,
                "signals": classification.signals,
                "active_strictness": resolved.strictness,
            },
        })),
    )
        .into_response()
}

fn build_raw(body: &TestBody) -> Result<(String, String, String), String> {
    if let Some(raw) = &body.raw_eml {
        // Cheap parse to pull subject + from for the response so
        // the UI can label what got tested.
        let (subj, from) = peek_subject_from(raw);
        return Ok((raw.clone(), subj, from));
    }
    let from = body
        .from_email
        .clone()
        .ok_or_else(|| "from_email required when raw_eml omitted".to_string())?;
    let subject = body.subject.clone().unwrap_or_default();
    let mut s = String::new();
    s.push_str(&format!("From: {from}\r\n"));
    s.push_str("To: ops@example.com\r\n");
    s.push_str(&format!("Subject: {subject}\r\n"));
    s.push_str("MIME-Version: 1.0\r\n");
    if let Some(html) = &body.html {
        s.push_str("Content-Type: text/html; charset=utf-8\r\n");
        s.push_str("\r\n");
        s.push_str(html);
    } else {
        s.push_str("Content-Type: text/plain; charset=utf-8\r\n");
        s.push_str("\r\n");
        s.push_str(body.body.as_deref().unwrap_or(""));
    }
    Ok((s, subject, from))
}

fn peek_subject_from(raw: &str) -> (String, String) {
    let mut subject = String::new();
    let mut from = String::new();
    for line in raw.lines() {
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix("Subject:") {
            subject = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("From:") {
            from = rest.trim().to_string();
        }
    }
    (subject, from)
}

// ── Response helpers ─────────────────────────────────────────────

fn service_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "ok": false,
            "error": {
                "code": "spam_filter_disabled",
                "message": "spam filter is not configured for this tenant",
            },
        })),
    )
        .into_response()
}

fn store_error(e: spam_filter::StoreError) -> Response {
    let (status, code) = match &e {
        spam_filter::StoreError::EmptyValue
        | spam_filter::StoreError::ValueTooLong { .. } => {
            (StatusCode::BAD_REQUEST, "bad_input")
        }
        spam_filter::StoreError::InvalidKind(_)
        | spam_filter::StoreError::InvalidStrictness(_) => {
            (StatusCode::BAD_REQUEST, "bad_input")
        }
        spam_filter::StoreError::Sqlite(_) => {
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

