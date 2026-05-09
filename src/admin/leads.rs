//! `/leads` read + mutation endpoints.
//!
//! **F29 sweep:** marketing-specific by design. Every endpoint
//! reaches into CRM-shaped types (`LeadStore`, `LeadId`, drafts,
//! state transitions, operator notes, followup overrides). The
//! axum router glue is generic but the SDK's
//! `nexo-microapp-http` already covers that boilerplate;
//! these handlers are the lead-shaped surface that builds on
//! top.
//!
//! - `GET /leads` — list. Optional `?state=engaged&limit=50`.
//! - `GET /leads/:lead_id` — single fetch. 404 when missing.
//! - `POST /leads/:lead_id/transition` — manual state move.
//! - `PUT /leads/:lead_id/notes` — operator scratch pad
//!   (M15.21.notes).
//! - `POST /leads/:lead_id/followup/override` — skip /
//!   postpone the followup cadence (M15.21.followup-override).
//! - draft CRUD endpoints (`/drafts/...`).
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
    /// When set, returns `count_by_state` (telemetry headline);
    /// without it the handler returns the inbox view (`list_all`)
    /// or — under `?due=1` — the followup-sweep envelope.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// `true` swaps the default list to `list_due_for_followup`
    /// (next_check_at_ms <= now). The followup sweep cron uses
    /// this; the operator dashboard does not. Default `false`
    /// = inbox view.
    #[serde(default)]
    pub due: bool,
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

    // Default: inbox view — every lead in the tenant ordered by
    // `last_activity_ms DESC`. Operator dashboard's landing list.
    // The followup sweep cron passes `?due=1` to get the
    // `next_check_at_ms <= now` envelope it needs.
    let now = chrono::Utc::now().timestamp_millis();
    if q.due {
        return match store.list_due_for_followup(now, limit).await {
            Ok(leads) => ok(json!({
                "leads": leads,
                "now_ms": now,
                "limit": limit,
                "view": "due",
            })),
            Err(e) => marketing_error(e),
        };
    }
    match store.list_all(limit).await {
        Ok(leads) => ok(json!({
            "leads": leads,
            "now_ms": now,
            "limit": limit,
            "view": "inbox",
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
    /// inherited from the lead row at send time when no
    /// override is supplied here.
    pub body: String,
    /// Optional operator-facing label. Defaults to "AI" so
    /// the lead drawer reads consistently regardless of
    /// who authored the draft (LLM tool / operator manual /
    /// auto-template).
    #[serde(default)]
    pub from_label: Option<String>,
    /// Optional subject override. When `Some`, the approve
    /// handler honors this verbatim instead of inheriting
    /// the lead's subject + the `Re:` prefix.
    #[serde(default)]
    pub subject: Option<String>,
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
    let subject = payload
        .subject
        .clone()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let new_msg = crate::lead::NewThreadMessage {
        message_id: message_id.clone(),
        direction: crate::lead::MessageDirection::Draft,
        from_label,
        body: payload.body,
        at_ms: now_ms,
        draft_status: Some(crate::lead::DraftStatus::Pending),
        subject,
        signature: None,
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
    /// Draft body. Required + non-empty when present;
    /// caller still always sends body even if only changing
    /// subject (avoids two endpoints).
    pub body: String,
    /// Optional subject mutation. Three cases:
    /// - field absent ⇒ leave subject untouched.
    /// - empty / whitespace ⇒ clear the subject (revert to
    ///   inheriting the lead's at approve time).
    /// - non-empty ⇒ persist verbatim.
    #[serde(default)]
    pub subject: Option<String>,
}

/// `PUT /leads/:lead_id/drafts/:message_id` — body
/// `{ body, subject? }`. Only succeeds when the draft is
/// `pending`; approved or rejected drafts return 409.
/// Subject + body land in two separate UPDATE statements
/// (each lock-checks `pending` independently); a body
/// update success without a subject success is treated as
/// success — the subject column is best-effort.
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
        Ok(_) => {
            if let Some(subject) = payload.subject.as_ref() {
                let trimmed = subject.trim();
                let new_subject =
                    if trimmed.is_empty() { None } else { Some(trimmed) };
                if let Err(e) = store
                    .update_draft_subject(&id, &message_id, new_subject)
                    .await
                {
                    return marketing_error(e);
                }
            }
            ok(json!({ "draft_id": message_id }))
        }
        Err(e) => marketing_error(e),
    }
}

/// `POST /leads/:lead_id/drafts/:message_id/approve` —
/// fires the outbound. Atomic-ish:
///
/// 1. Resolve lead + seller + person on the active tenant.
/// 2. Validate the seller has an SMTP credential (M15.16).
/// 3. Compose the `OutboundEmail` (subject inherits from
///    the lead row + draft body + RFC 5322 threading
///    headers).
/// 4. Run `prepare_outbound_email` (M15.23.a) when tracking
///    is wired — injects open pixel + rewrites links +
///    persists link mappings.
/// 5. Call `OutboundPublisher::dispatch` — runs the compliance
///    gate + idempotency reservation.
/// 6. On `Publish { topic, command, idempotency_key }`,
///    serialise the command + publish via the cached
///    `BrokerSender`.
/// 7. Atomically: set draft status = approved + append
///    outbound `thread_messages` row carrying the same
///    `msg_id` so the lead drawer reflects the send.
/// 8. Record audit + firehose event.
///
/// Failures degrade to typed error codes the operator UI
/// surfaces in the modal banner — no partial state lands
/// when any step refuses (compliance block, missing creds,
/// broker not attached, etc.).
pub async fn approve_draft_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path((lead_id, message_id)): Path<(String, String)>,
) -> Response {
    use crate::broker::{DispatchInput, OutboundDispatchResult, OutboundEmail};
    use crate::lead::DraftStatus;

    // ── Validate wiring ──────────────────────────────────────
    let store = match state.lookup_store(&tenant_id) {
        Some(s) => s,
        None => return server_error("store_missing", "tenant store not mounted"),
    };
    let outbound = match state.outbound.as_ref() {
        Some(o) => o.clone(),
        None => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "outbound_disabled",
                "OutboundPublisher not wired; build with the marketing-extension binary that sets state.outbound at boot",
            );
        }
    };
    let broker_sender = match state.broker_sender.load_full() {
        Some(s) => s,
        None => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "broker_not_attached",
                "BrokerSender not yet captured; wait for the first inbound or trigger any plugin event before approving",
            );
        }
    };

    // ── Resolve lead + draft ────────────────────────────────
    let lead_obj_id = LeadId(lead_id.clone());
    let lead = match store.get(&lead_obj_id).await {
        Ok(Some(l)) => l,
        Ok(None) => {
            return error(
                StatusCode::NOT_FOUND,
                "lead_not_found",
                &format!("no lead with id {lead_id:?} for the active tenant"),
            );
        }
        Err(e) => return marketing_error(e),
    };
    let drafts = match store.list_drafts(&lead_obj_id, None).await {
        Ok(rows) => rows,
        Err(e) => return marketing_error(e),
    };
    let draft = match drafts.iter().find(|d| d.id == message_id) {
        Some(d) => d,
        None => {
            return error(
                StatusCode::NOT_FOUND,
                "draft_not_found",
                "no draft with that id on the active lead",
            );
        }
    };
    if draft.draft_status != Some(DraftStatus::Pending) {
        return error(
            StatusCode::CONFLICT,
            "draft_not_pending",
            "draft is already approved/rejected",
        );
    }

    // ── Resolve seller + SMTP credential ────────────────────
    let sellers_lookup = match state.seller_lookup.as_ref() {
        Some(l) => l.load_full(),
        None => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "sellers_not_loaded",
                "seller index not mounted on AdminState — operator hasn't saved sellers.yaml yet",
            );
        }
    };
    let seller = match sellers_lookup.get(&lead.seller_id) {
        Some(s) => s.clone(),
        None => {
            return error(
                StatusCode::NOT_FOUND,
                "seller_not_found",
                &format!("seller {:?} bound to lead but missing from sellers.yaml", lead.seller_id.0),
            );
        }
    };
    let smtp = match seller.smtp_credential.as_ref() {
        Some(c) => c.clone(),
        None => {
            return error(
                StatusCode::PRECONDITION_FAILED,
                "seller_missing_smtp",
                &format!(
                    "seller {:?} has no smtp_credential — register one via the seller form before approving drafts",
                    seller.id.0,
                ),
            );
        }
    };

    // ── Resolve recipient via PersonStore (when wired) ──────
    // Falls back to the lead's `person_id` interpreted as
    // an email — placeholder leads carry a synthesised id
    // shaped like `placeholder-<uuid>` which isn't a real
    // address, so the fallback bails clearly.
    let recipient_email = lead.person_id.0.clone();
    let recipient_email = if recipient_email.contains('@') {
        recipient_email
    } else {
        return error(
            StatusCode::PRECONDITION_FAILED,
            "recipient_unresolved",
            "lead's person_id is a placeholder — operator must confirm the contact's email before approving",
        );
    };

    // ── Compose the OutboundEmail body ──────────────────────
    let mut html_body = format!(
        "<html><body>{}<br/><br/>{}</body></html>",
        draft.body.replace('\n', "<br/>"),
        seller.signature_text.replace('\n', "<br/>"),
    );

    // Subject — honour the operator's per-draft override
    // when set, else inherit the lead's subject with a
    // `Re:` prefix when missing. Empty / whitespace-only
    // overrides fall back to the inherited path so the
    // outbound never goes out with a blank subject line.
    let subject = match draft
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(custom) => custom.to_string(),
        None => {
            if lead.subject.to_ascii_lowercase().starts_with("re:") {
                lead.subject.clone()
            } else {
                format!("Re: {}", lead.subject)
            }
        }
    };

    // ── Tracking pre-send (open pixel + link rewrite) ───────
    // When tracking deps are wired, mint a fresh msg_id +
    // mutate `html_body` in place. The returned msg_id
    // becomes the stable identifier for engagement
    // attribution AND the thread_messages row.
    let msg_id = if let Some(tracking) = state.tracking_for_outbound.as_ref() {
        match crate::tracking::prepare_outbound_email(
            tracking,
            &tenant_id,
            &mut html_body,
        )
        .await
        {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(
                    target: "extension.marketing.draft_approve",
                    error = %e,
                    "prepare_outbound_email failed; outbound will lack tracking signals (non-fatal)"
                );
                None
            }
        }
    } else {
        None
    };

    let outbound_msg_id = msg_id
        .as_ref()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| format!("outbound-{}", uuid::Uuid::new_v4()));

    // Pre-track the outbound rfc_message_id (compose-flow
    // parity) so the customer's reply on this specific message
    // resolves back to THIS lead via outbound_message_ids
    // even if the reply chain doesn't echo the original
    // thread_id. Format mirrors compose:
    // `{lead_id}.{outbound_msg_id}@{seller_domain}`.
    let seller_domain = seller
        .primary_email
        .split_once('@')
        .map(|(_, d)| d.to_string())
        .unwrap_or_else(|| "marketing.local".into());
    let approve_rfc_message_id = format!(
        "{}.{}@{}",
        lead.id.0, outbound_msg_id, seller_domain
    );
    if let Err(e) = store
        .record_outbound_message_id(
            &approve_rfc_message_id,
            &lead.id,
            &lead.thread_id,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
    {
        tracing::warn!(
            target: "extension.marketing.draft_approve",
            error = %e,
            "outbound_message_id persist failed; reply may not thread back to this lead",
        );
    }

    // Audit fix #99 — in_reply_to should chain against the
    // LATEST inbound on the thread, not the original root.
    // Keeps the recipient's mail client view tidy AND lets
    // multi-round threads accumulate the references chain
    // properly per RFC 5322 §3.6.4. Falls back to the lead's
    // thread_id (the legacy single-element references) when
    // no inbound row exists yet (cold compose path that hasn't
    // received a reply).
    let approve_thread = match store.list_thread(&lead_obj_id).await {
        Ok(rows) => rows,
        Err(_) => Vec::new(),
    };
    let latest_inbound_id: Option<String> = approve_thread
        .iter()
        .rev()
        .find(|m| m.direction == crate::lead::MessageDirection::Inbound)
        .map(|m| m.id.clone());
    let in_reply_to_target = latest_inbound_id
        .clone()
        .unwrap_or_else(|| lead.thread_id.clone());
    // References = thread_root + every subsequent inbound id.
    // Mail clients walk this list to render the conversation
    // hierarchy. Keeping it short (max ~10 entries) avoids
    // header bloat on long threads.
    let mut references: Vec<String> = vec![lead.thread_id.clone()];
    for m in approve_thread
        .iter()
        .filter(|m| m.direction == crate::lead::MessageDirection::Inbound)
        .rev()
        .take(8)
    {
        if !references.contains(&m.id) {
            references.push(m.id.clone());
        }
    }

    let email = OutboundEmail {
        to: vec![recipient_email],
        cc: vec![],
        bcc: vec![],
        subject,
        body: html_body,
        in_reply_to: Some(in_reply_to_target),
        references,
        // Pre-supplied so the email plugin honours it verbatim
        // (lets `outbound_message_ids` resolve replies on this
        // exact message later).
        message_id: Some(approve_rfc_message_id.clone()),
    };
    let dispatch_input = DispatchInput {
        thread_id: lead.thread_id.clone(),
        draft_id: message_id.clone(),
        seller_smtp_instance: smtp.instance.clone(),
        email,
    };

    // ── Compliance gate + idempotency reservation ───────────
    let outcome = outbound.dispatch(dispatch_input);
    let (topic, command) = match outcome {
        OutboundDispatchResult::Publish {
            topic, command, ..
        } => (topic, command),
        OutboundDispatchResult::Skipped { reason, .. } => {
            return error(
                StatusCode::CONFLICT,
                "outbound_skipped",
                &format!("publisher reservation says draft already dispatched: {reason}"),
            );
        }
        OutboundDispatchResult::Blocked { reason, .. } => {
            return error(
                StatusCode::PRECONDITION_FAILED,
                "outbound_blocked",
                &format!("compliance gate blocked send: {reason}"),
            );
        }
    };

    // ── Publish to the broker ───────────────────────────────
    let json_payload =
        serde_json::to_value(&command).unwrap_or_else(|_| serde_json::json!({}));
    let event = nexo_microapp_sdk::BrokerEvent::new(
        topic.clone(),
        "marketing.outbound",
        json_payload,
    );
    if let Err(e) = broker_sender.publish(&topic, event).await {
        return error(
            StatusCode::BAD_GATEWAY,
            "broker_publish_failed",
            &format!("publish to {topic}: {e}"),
        );
    }

    // ── Persist draft status + outbound thread row ──────────
    // Ordering: status flip happens first so a crash mid-
    // append doesn't fork the row into a "approved but no
    // outbound message" state.
    if let Err(e) = store
        .set_draft_status(&lead_obj_id, &message_id, DraftStatus::Approved)
        .await
    {
        return marketing_error(e);
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let outbound_msg = crate::lead::NewThreadMessage {
        message_id: outbound_msg_id.clone(),
        direction: crate::lead::MessageDirection::Outbound,
        from_label: seller.name.clone(),
        body: command.body.clone(),
        at_ms: now_ms,
        draft_status: None, subject: None,
        signature: None,
    };
    if let Err(e) = store.append_thread_message(&lead_obj_id, outbound_msg).await {
        tracing::warn!(
            target: "extension.marketing.draft_approve",
            error = %e,
            lead_id = %lead.id.0,
            "outbound thread_message persist failed (non-fatal — broker already published)"
        );
    }

    // ── Audit row ───────────────────────────────────────────
    if let Some(audit) = state.audit.as_ref() {
        let event = crate::audit::AuditEvent::NotificationPublished {
            tenant_id: tenant_id.as_str().to_string(),
            lead_id: lead.id.0.clone(),
            seller_id: lead.seller_id.0.clone(),
            notification_kind: "draft_approved".into(),
            channel: "smtp_outbound".into(),
            at_ms: now_ms.max(0) as u64,
        };
        if let Err(e) = audit.record(event).await {
            tracing::warn!(
                target: "extension.marketing.draft_approve",
                error = %e,
                "approve audit record failed (non-fatal)"
            );
        }
    }

    ok(json!({
        "draft_id": message_id,
        "status": "approved",
        "topic": topic,
        "outbound_message_id": outbound_msg_id,
        "rfc_message_id": approve_rfc_message_id,
        "tracking_msg_id": msg_id.map(|m| m.as_str().to_string()),
    }))
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

/// `POST /leads/:lead_id/transition` body
/// `{ to: "engaged"|"meeting_scheduled"|"qualified"|"lost",
///   reason?: "..." }`.
///
/// Operator-driven manual transition. Validates the target
/// state + the from→to legality at the store layer (the
/// store refuses cross-illegal moves like
/// `qualified → cold`). Records an audit row + publishes a
/// firehose event so SSE consumers (lead timeline) see it
/// in real time.
#[derive(Debug, Deserialize)]
pub struct TransitionBody {
    pub to: String,
    #[serde(default)]
    pub reason: Option<String>,
}

pub async fn transition_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(lead_id): Path<String>,
    Json(payload): Json<TransitionBody>,
) -> Response {
    use nexo_tool_meta::marketing::TenantIdRef;
    let store = match state.lookup_store(&tenant_id) {
        Some(s) => s,
        None => return server_error("store_missing", "tenant store not mounted"),
    };
    let target = match parse_state(&payload.to) {
        Some(s) => s,
        None => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_state",
                "to must be one of: cold, engaged, meeting_scheduled, qualified, lost",
            );
        }
    };
    let id = LeadId(lead_id.clone());
    // Capture the pre-transition state for the audit row +
    // firehose event. Refuses 404 here so the caller gets a
    // clean error before the store's validate_transition
    // would surface a less-specific message.
    let before = match store.get(&id).await {
        Ok(Some(l)) => l,
        Ok(None) => {
            return error(
                StatusCode::NOT_FOUND,
                "lead_not_found",
                &format!("no lead with id {lead_id:?} for the active tenant"),
            );
        }
        Err(e) => return marketing_error(e),
    };
    let after = match store.transition(&id, target).await {
        Ok(l) => l,
        Err(crate::error::MarketingError::InvalidTransition {
            lead_id: _,
            from,
            to,
        }) => {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "illegal_transition",
                &format!(
                    "lead state machine refuses {} → {}",
                    state_label(from),
                    state_label(to),
                ),
            );
        }
        Err(e) => return marketing_error(e),
    };
    let reason = payload
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("operator override")
        .to_string();
    let now_ms = chrono::Utc::now().timestamp_millis();

    // ── Audit ──────────────────────────────────────────────
    if let Some(audit) = state.audit.as_ref() {
        let event = crate::audit::AuditEvent::LeadTransitioned {
            tenant_id: tenant_id.as_str().to_string(),
            lead_id: lead_id.clone(),
            from: state_label(before.state).into(),
            to: state_label(target).into(),
            reason: reason.clone(),
            at_ms: now_ms.max(0) as u64,
        };
        if let Err(e) = audit.record(event).await {
            tracing::warn!(
                target: "extension.marketing.transition",
                error = %e,
                "transition audit record failed (non-fatal)"
            );
        }
    }

    // ── Firehose ───────────────────────────────────────────
    state.firehose.publish(
        crate::firehose::LeadFirehoseEvent::Transitioned {
            tenant_id: TenantIdRef(tenant_id.as_str().to_string()),
            lead_id: id.clone(),
            from: before.state,
            to: target,
            at_ms: now_ms,
            reason: reason.clone(),
        },
    );

    ok(json!({
        "lead": after,
        "from": state_label(before.state),
        "to": state_label(target),
        "reason": reason,
    }))
}

// ── M15.21.followup-override — skip / postpone endpoint ───────

/// `POST /leads/:lead_id/followup/override` body. Tagged-union
/// shape so the daemon can validate per-action fields without
/// a permissive sentinel pattern. `action: skip` clears
/// `next_check_at_ms`; `action: postpone` requires `until_ms`.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum FollowupOverrideBody {
    /// Cancel the pending followup iteration. The lead stays
    /// active — a future inbound bumps `next_check_at_ms`
    /// back. `followup_attempts` is NOT incremented (skip is
    /// a manual decision, not an exhausted retry).
    Skip {
        #[serde(default)]
        reason: Option<String>,
    },
    /// Move `next_check_at_ms` forward to a specific operator-
    /// chosen wall-clock timestamp (ms). Validated to be > now
    /// so a postpone never accidentally re-schedules into the
    /// past.
    Postpone {
        until_ms: i64,
        #[serde(default)]
        reason: Option<String>,
    },
}

/// `POST /leads/:lead_id/followup/override` — operator bypass
/// of the followup cadence for one lead. Validates payload,
/// runs `set_next_check`, records audit, publishes firehose.
/// Typed errors:
///  - 400 invalid_body            — missing fields per action
///  - 400 postpone_in_past        — until_ms <= now
///  - 404 lead_not_found          — wrong tenant / id
pub async fn followup_override_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(lead_id): Path<String>,
    Json(payload): Json<FollowupOverrideBody>,
) -> Response {
    use nexo_tool_meta::marketing::TenantIdRef;
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
    let now_ms = chrono::Utc::now().timestamp_millis();
    let (next_check_at_ms, action_label, raw_reason) = match payload {
        FollowupOverrideBody::Skip { reason } => (None, "skip", reason),
        FollowupOverrideBody::Postpone { until_ms, reason } => {
            if until_ms <= now_ms {
                return error(
                    StatusCode::BAD_REQUEST,
                    "postpone_in_past",
                    &format!(
                        "until_ms ({until_ms}) must be strictly after now ({now_ms})"
                    ),
                );
            }
            (Some(until_ms), "postpone", reason)
        }
    };
    // `increment_attempts: false` — skip + postpone are
    // operator decisions, not exhausted retries. The followup
    // sweep increments attempts only when IT fires the bump
    // automatically.
    let lead = match store.set_next_check(&id, next_check_at_ms, false).await {
        Ok(l) => l,
        Err(MarketingError::Sqlite(sqlx::Error::RowNotFound)) => {
            return error(
                StatusCode::NOT_FOUND,
                "lead_not_found",
                &format!("no lead with id {lead_id:?} for the active tenant"),
            );
        }
        Err(e) => return marketing_error(e),
    };
    let reason = raw_reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("operator override")
        .to_string();

    // ── Audit ──────────────────────────────────────────────
    if let Some(audit) = state.audit.as_ref() {
        let event = crate::audit::AuditEvent::FollowupOverridden {
            tenant_id: tenant_id.as_str().to_string(),
            lead_id: lead_id.clone(),
            action: action_label.to_string(),
            postpone_until_ms: next_check_at_ms,
            reason: reason.clone(),
            at_ms: now_ms.max(0) as u64,
        };
        if let Err(e) = audit.record(event).await {
            tracing::warn!(
                target: "extension.marketing.followup_override",
                error = %e,
                "followup override audit record failed (non-fatal)"
            );
        }
    }

    // ── Firehose ───────────────────────────────────────────
    state.firehose.publish(
        crate::firehose::LeadFirehoseEvent::FollowupOverridden {
            tenant_id: TenantIdRef(tenant_id.as_str().to_string()),
            lead_id: id.clone(),
            action: action_label.to_string(),
            next_check_at_ms,
            reason: reason.clone(),
            at_ms: now_ms,
        },
    );

    ok(json!({
        "lead": lead,
        "action": action_label,
        "next_check_at_ms": next_check_at_ms,
        "reason": reason,
    }))
}

// ── M15.21.notes — operator notes endpoint ─────────────────────

/// `PUT /leads/:lead_id/notes` body. `notes: null` clears the
/// column to SQL NULL; `notes: ""` persists an empty string
/// (legal terminal state — round-trips faithfully). The shape
/// is wire-symmetric to the field on the `Lead` struct so
/// callers can serialise the response straight back into the
/// editor without re-shaping.
#[derive(Debug, Deserialize)]
pub struct UpdateNotesBody {
    /// `Some` writes verbatim (empty string allowed); `None`
    /// (or omitted from JSON) clears the field to SQL NULL.
    #[serde(default)]
    pub notes: Option<String>,
}

/// `PUT /leads/:lead_id/notes` — replace the lead's free-form
/// operator notes (markdown). Idempotent: re-sending the same
/// body is a no-op. Caller MUST hold a tenant scope (the auth
/// middleware enforces it). 404 when the lead doesn't exist
/// for this tenant.
pub async fn update_notes_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(lead_id): Path<String>,
    Json(payload): Json<UpdateNotesBody>,
) -> Response {
    let store = match state.lookup_store(&tenant_id) {
        Some(s) => s,
        None => return server_error("store_missing", "tenant store not mounted"),
    };
    let id = LeadId(lead_id.clone());
    // Defense-in-depth: surface a clean 404 before reaching
    // the UPDATE so the caller doesn't get the less-specific
    // RowNotFound that bubbles up from `update_operator_notes`
    // when the row is missing.
    if matches!(store.get(&id).await, Ok(None)) {
        return error(
            StatusCode::NOT_FOUND,
            "lead_not_found",
            &format!("no lead with id {lead_id:?} for the active tenant"),
        );
    }
    match store.update_operator_notes(&id, payload.notes).await {
        Ok(lead) => ok(json!({ "lead": lead })),
        Err(MarketingError::Sqlite(sqlx::Error::RowNotFound)) => error(
            StatusCode::NOT_FOUND,
            "lead_not_found",
            &format!("no lead with id {lead_id:?} for the active tenant"),
        ),
        Err(e) => marketing_error(e),
    }
}

/// Inverse of `parse_state` — used by the transition
/// handler's audit + firehose payload.
fn state_label(s: LeadState) -> &'static str {
    match s {
        LeadState::Cold => "cold",
        LeadState::Engaged => "engaged",
        LeadState::MeetingScheduled => "meeting_scheduled",
        LeadState::Qualified => "qualified",
        LeadState::Lost => "lost",
    }
}

/// `GET /drafts?limit=N` — tenant-wide pending drafts
/// queue. Returns rows joined with the lead context so the
/// operator's drafts inbox renders subject + seller +
/// state without a follow-up fetch per row. Newest first;
/// `limit` defaults to 50, clamped to [1, 200].
#[derive(Debug, Deserialize)]
pub struct DraftsInboxQuery {
    #[serde(default)]
    pub limit: Option<u32>,
}

const DEFAULT_DRAFTS_LIMIT: u32 = 50;
const MAX_DRAFTS_LIMIT: u32 = 200;

pub async fn drafts_inbox_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Query(q): Query<DraftsInboxQuery>,
) -> Response {
    let store = match state.lookup_store(&tenant_id) {
        Some(s) => s,
        None => return server_error("store_missing", "tenant store not mounted"),
    };
    let limit = q
        .limit
        .unwrap_or(DEFAULT_DRAFTS_LIMIT)
        .clamp(1, MAX_DRAFTS_LIMIT);
    match store.list_pending_drafts_tenant_wide(limit).await {
        Ok(rows) => ok(json!({
            "drafts": rows,
            "count": rows.len(),
            "limit": limit,
        })),
        Err(e) => marketing_error(e),
    }
}

/// `POST /leads/:lead_id/drafts/generate` — operator-pull
/// draft generation (M15.21 slice 4). Resolves the lead +
/// seller + last inbound message, hands the structured
/// context to the wired [`crate::draft::DraftGenerator`],
/// and persists the response as a `direction = draft /
/// status = pending` row. Returns the new draft so the
/// frontend can render it without a follow-up GET.
///
/// Optional request body:
/// ```json
/// { "operator_hint": "focus on pricing", "from_label": "AI" }
/// ```
///
/// Both fields are optional — `operator_hint` flows into
/// the generator's `DraftContext` so the template / LLM
/// can react; `from_label` defaults to `"AI"` so the lead
/// drawer reads consistently.
#[derive(Debug, Default, Deserialize)]
pub struct GenerateDraftBody {
    #[serde(default)]
    pub operator_hint: Option<String>,
    #[serde(default)]
    pub from_label: Option<String>,
}

pub async fn generate_draft_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Path(lead_id): Path<String>,
    payload: Option<Json<GenerateDraftBody>>,
) -> Response {
    use crate::lead::MessageDirection;

    // Marketing pause gate: refuse the LLM-spending automation
    // when the tenant is disabled. Operator can still EDIT,
    // APPROVE, SEND existing pending drafts (those endpoints
    // are intentionally separate so the operator stays in
    // control of inflight work during a pause).
    if let Some(state_cache) = state.marketing_state.as_ref() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        if !state_cache.is_enabled(tenant_id.as_str(), now_ms).await {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "marketing_paused",
                "marketing extension is paused for this tenant — re-enable to generate drafts",
            );
        }
    }

    // ── Validate wiring ──────────────────────────────────────
    let store = match state.lookup_store(&tenant_id) {
        Some(s) => s,
        None => return server_error("store_missing", "tenant store not mounted"),
    };
    let generator = match state.draft_generator.as_ref() {
        Some(g) => g.clone(),
        None => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "draft_generator_disabled",
                "no DraftGenerator wired in AdminState — operator boot config missing `with_draft_generator`",
            );
        }
    };
    let sellers_lookup = match state.seller_lookup.as_ref() {
        Some(l) => l.load_full(),
        None => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "sellers_not_loaded",
                "seller index not mounted on AdminState — operator hasn't saved sellers.yaml yet",
            );
        }
    };

    // ── Resolve lead ─────────────────────────────────────────
    let lead_obj_id = LeadId(lead_id.clone());
    let lead = match store.get(&lead_obj_id).await {
        Ok(Some(l)) => l,
        Ok(None) => {
            return error(
                StatusCode::NOT_FOUND,
                "lead_not_found",
                &format!("no lead with id {lead_id:?} for the active tenant"),
            );
        }
        Err(e) => return marketing_error(e),
    };
    let seller = match sellers_lookup.get(&lead.seller_id) {
        Some(s) => s.clone(),
        None => {
            return error(
                StatusCode::NOT_FOUND,
                "seller_not_found",
                &format!(
                    "seller {:?} bound to lead but missing from sellers.yaml",
                    lead.seller_id.0,
                ),
            );
        }
    };

    // ── Thread history + last inbound ───────────────────────
    // We pull the FULL chronological thread (oldest first) so
    // the LLM-driven generator can build a multi-turn prompt
    // and reflect the entire conversation in its reply, not
    // just the latest inbound. `last_inbound` is the convenient
    // single-message path the legacy Handlebars template still
    // references; both fields share the same source so they
    // never disagree.
    let thread_history = match store.list_thread(&lead_obj_id).await {
        Ok(messages) => messages,
        Err(e) => {
            tracing::warn!(
                target: "extension.marketing.draft_generate",
                error = %e,
                lead_id = %lead_id,
                "thread fetch failed during draft generation; proceeding without thread context"
            );
            Vec::new()
        }
    };
    let last_inbound = thread_history
        .iter()
        .rev()
        .find(|m| m.direction == MessageDirection::Inbound)
        .cloned();

    let body_in = payload.map(|Json(b)| b).unwrap_or_default();

    // ── Idempotent dedup gate ───────────────────────────────
    // Compute the input-identity signature BEFORE spending
    // tokens. If a pending draft already exists with this
    // exact signature we return it verbatim — same input ⇒
    // same output, no quality loss. Operator-hint bumps DO
    // NOT contribute to the signature on purpose: the hint
    // is meant to nudge a fresh take, so passing one always
    // re-generates regardless of cache state.
    //
    // Hint-empty path (the common one — cron + UI default
    // click) is the cache target.
    let last_inbound_msg_id =
        last_inbound.as_ref().map(|m| m.id.clone());
    let signature = crate::draft::compute_draft_signature(
        &lead,
        last_inbound_msg_id.as_deref(),
        &seller,
    );
    let hint_present = body_in
        .operator_hint
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    // Lock key includes tenant so two tenants with the same
    // signature space don't serialize against each other.
    let lock_key = format!("{}|{}", tenant_id.as_str(), signature);
    // Acquire ONLY on the cache-targetable path (no operator
    // hint). Hint-driven calls bypass lock + cache by design.
    let _draft_lock = if hint_present {
        None
    } else {
        Some(state.draft_locks.acquire(&lock_key).await)
    };
    if !hint_present {
        match store
            .find_pending_draft_by_signature(&lead_obj_id, &signature)
            .await
        {
            Ok(Some(existing)) => {
                let now_ms = chrono::Utc::now().timestamp_millis();
                if let Some(audit) = state.audit.as_ref() {
                    let ev = crate::audit::AuditEvent::DraftDeduped {
                        tenant_id: tenant_id.as_str().to_string(),
                        lead_id: lead_id.clone(),
                        draft_message_id: existing.id.clone(),
                        signature: signature.clone(),
                        source: "operator_pull".to_string(),
                        at_ms: now_ms as u64,
                    };
                    if let Err(e) = audit.record(ev).await {
                        tracing::warn!(
                            target: "extension.marketing.draft_generate",
                            error = %e,
                            "audit record draft_deduped failed",
                        );
                    }
                }
                tracing::info!(
                    target: "extension.marketing.draft_generate",
                    lead_id = %lead_id,
                    draft_message_id = %existing.id,
                    signature = %signature,
                    "draft dedup hit — returning existing pending draft",
                );
                return (
                    StatusCode::OK,
                    Json(json!({
                        "ok": true,
                        "result": { "draft": existing, "from_cache": true },
                    })),
                )
                    .into_response();
            }
            Ok(None) => {
                tracing::debug!(
                    target: "extension.marketing.draft_generate",
                    lead_id = %lead_id,
                    signature = %signature,
                    "draft dedup miss — generating fresh",
                );
            }
            Err(e) => {
                // Non-fatal: lookup failure should never block
                // a generate. Log + fall through to fresh
                // generation (we eat the duplicate cost rather
                // than refuse the operator).
                tracing::warn!(
                    target: "extension.marketing.draft_generate",
                    error = %e,
                    "draft dedup lookup failed; falling through to fresh generate",
                );
            }
        }
    }

    // ── Generate ────────────────────────────────────────────
    let ctx = crate::draft::DraftContext {
        lead,
        seller,
        last_inbound,
        thread_history,
        operator_hint: body_in
            .operator_hint
            .filter(|s| !s.trim().is_empty()),
    };
    let body = match generator.generate(&ctx).await {
        Ok(b) => b,
        Err(crate::draft::DraftGenError::TemplateParse(m)) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "template_invalid",
                &format!("draft template parse error: {m}"),
            );
        }
        Err(crate::draft::DraftGenError::TemplateRender(m)) => {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "template_render_error",
                &format!("draft template render error: {m}"),
            );
        }
        Err(crate::draft::DraftGenError::Backend(m)) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "generator_unavailable",
                &format!("draft generator backend refused: {m}"),
            );
        }
        Err(crate::draft::DraftGenError::Empty) => {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "generator_empty_body",
                "draft generator returned an empty body",
            );
        }
    };

    // ── Persist as pending draft ────────────────────────────
    let message_id = format!("draft-{}", uuid::Uuid::new_v4());
    let from_label = body_in
        .from_label
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "AI".into());
    let now_ms = chrono::Utc::now().timestamp_millis();
    // Persist the signature only on the cache-targetable path
    // (no operator hint). Hint-driven generations stay
    // signature-NULL so they don't poison the cache for the
    // next hint-empty regen — operator nudge ⇒ one-shot draft.
    let persisted_signature = if hint_present { None } else { Some(signature.clone()) };
    let new_msg = crate::lead::NewThreadMessage {
        message_id: message_id.clone(),
        direction: MessageDirection::Draft,
        from_label,
        body,
        at_ms: now_ms,
        draft_status: Some(crate::lead::DraftStatus::Pending),
        subject: None,
        signature: persisted_signature,
    };
    if let Err(e) = store.append_thread_message(&lead_obj_id, new_msg).await {
        return marketing_error(e);
    }

    // Read back the canonical row so the response shape
    // matches `create_draft_handler` — the frontend can
    // render the new draft without a follow-up GET.
    let drafts = match store
        .list_drafts(&lead_obj_id, Some(crate::lead::DraftStatus::Pending))
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
    async fn list_no_filter_returns_inbox_envelope() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app.oneshot(req("/leads")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert!(v["result"]["leads"].is_array());
        assert!(v["result"]["limit"].is_u64());
        assert_eq!(v["result"]["view"], "inbox");
        // The seeded lead has `next_check_at_ms = None` — the
        // pre-fix code would return [] here (only due-for-
        // followup); inbox view returns it.
        let leads = v["result"]["leads"].as_array().unwrap();
        assert_eq!(leads.len(), 1, "inbox view must return cold leads");
    }

    #[tokio::test]
    async fn list_due_query_returns_followup_envelope() {
        let state = build_state_with_lead().await;
        let app = router(state);
        // serde_urlencoded → bool requires the literal `true`/
        // `false`; query-string `due=1` doesn't deserialize.
        let resp = app.oneshot(req("/leads?due=true")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["view"], "due");
        // Seeded lead has no `next_check_at_ms` — the followup
        // sweep view excludes it.
        assert_eq!(v["result"]["leads"].as_array().unwrap().len(), 0);
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
                    draft_status: None, subject: None,
                    signature: None,
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
                    draft_status: None, subject: None,
                    signature: None,
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

    // ── M15.21 slice 4 — `POST /leads/:id/drafts/generate` ────

    /// Build an `AdminState` with the lead seeded (seller `v`)
    /// + a seller_lookup carrying `v` + a wired draft generator.
    /// Returns the inner store so tests can seed inbound rows.
    async fn build_state_with_generator() -> (Arc<AdminState>, Arc<LeadStore>) {
        use crate::notification::seller_lookup_from_list;
        use nexo_tool_meta::marketing::{Seller, TenantIdRef};

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
                subject: "Cot pricing".into(),
                person_id: PersonId("juan@acme.com".into()),
                seller_id: SellerId("v".into()),
                last_activity_ms: 1,
                score: 0,
                topic_tags: vec![],
                why_routed: vec!["fixture".into()],
            })
            .await
            .unwrap();
        let store = Arc::new(store);
        let sellers = seller_lookup_from_list(vec![Seller {
            id: SellerId("v".into()),
            tenant_id: TenantIdRef("acme".into()),
            name: "Pedro García".into(),
            primary_email: "pedro@acme.com".into(),
            alt_emails: vec![],
            signature_text: "—\nPedro · Acme".into(),
            working_hours: None,
            on_vacation: false,
            vacation_until: None,
            preferred_language: Some("es".into()),
            agent_id: None,
            model_override: None,
            notification_settings: None,
            smtp_credential: None,
            draft_template: None,
            system_prompt: None,
            model_provider: None,
            model_id: None,
        }]);
        let gen: Arc<dyn crate::draft::DraftGenerator + Send + Sync> = Arc::new(
            crate::draft::TemplateDraftGenerator::with_default_template(),
        );
        let state = Arc::new(
            AdminState::new("secret".into())
                .with_store(store.clone())
                .with_seller_lookup(sellers)
                .with_draft_generator(gen),
        );
        (state, store)
    }

    fn post(uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer secret")
            .header("X-Tenant-Id", "acme")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    #[tokio::test]
    async fn generate_creates_pending_draft_with_default_template() {
        let (state, _store) = build_state_with_generator().await;
        let app = router(state);
        let resp = app
            .oneshot(post("/leads/l-1/drafts/generate", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = body_to_json(resp).await;
        assert_eq!(v["ok"], true);
        let draft = &v["result"]["draft"];
        assert_eq!(draft["direction"], "draft");
        assert_eq!(draft["draft_status"], "pending");
        assert_eq!(draft["from_label"], "AI");
        let body = draft["body"].as_str().unwrap();
        assert!(body.contains("Cot pricing"));
        assert!(body.contains("Pedro García"));
    }

    #[tokio::test]
    async fn generate_uses_inbound_when_thread_has_one() {
        use crate::lead::{MessageDirection, NewThreadMessage};
        let (state, store) = build_state_with_generator().await;
        store
            .append_thread_message(
                &LeadId("l-1".into()),
                NewThreadMessage {
                    message_id: "m1".into(),
                    direction: MessageDirection::Inbound,
                    from_label: "Juan".into(),
                    body: "¿Cuánto cuesta?".into(),
                    at_ms: 100,
                    draft_status: None, subject: None,
                    signature: None,
                },
            )
            .await
            .unwrap();
        let app = router(state);
        let resp = app
            .oneshot(post("/leads/l-1/drafts/generate", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = body_to_json(resp).await;
        let body = v["result"]["draft"]["body"].as_str().unwrap();
        // Default template references the inbound author.
        assert!(body.contains("Hola Juan,"));
        assert!(body.contains("¿Cuánto cuesta?"));
    }

    #[tokio::test]
    async fn generate_passes_operator_hint_through_to_template() {
        let (state, _store) = build_state_with_generator().await;
        let app = router(state);
        let resp = app
            .oneshot(post(
                "/leads/l-1/drafts/generate",
                serde_json::json!({ "operator_hint": "Mencionar promo de mayo" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = body_to_json(resp).await;
        let body = v["result"]["draft"]["body"].as_str().unwrap();
        assert!(body.contains("Mencionar promo de mayo"));
    }

    #[tokio::test]
    async fn generate_respects_custom_from_label() {
        let (state, _store) = build_state_with_generator().await;
        let app = router(state);
        let resp = app
            .oneshot(post(
                "/leads/l-1/drafts/generate",
                serde_json::json!({ "from_label": "Pedro auto" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["draft"]["from_label"], "Pedro auto");
    }

    #[tokio::test]
    async fn generate_returns_503_when_generator_not_wired() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app
            .oneshot(post("/leads/l-1/drafts/generate", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let v = body_to_json(resp).await;
        assert_eq!(v["error"]["code"], "draft_generator_disabled");
    }

    #[tokio::test]
    async fn generate_returns_404_for_missing_lead() {
        let (state, _store) = build_state_with_generator().await;
        let app = router(state);
        let resp = app
            .oneshot(post("/leads/ghost/drafts/generate", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let v = body_to_json(resp).await;
        assert_eq!(v["error"]["code"], "lead_not_found");
    }

    #[tokio::test]
    async fn generate_returns_500_when_template_renders_empty() {
        // Wire a generator with a whitespace-only template
        // so the renderer trips the `Empty` guard.
        use crate::notification::seller_lookup_from_list;
        use nexo_tool_meta::marketing::{Seller, TenantIdRef};
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
                subject: "S".into(),
                person_id: PersonId("p".into()),
                seller_id: SellerId("v".into()),
                last_activity_ms: 1,
                score: 0,
                topic_tags: vec![],
                why_routed: vec![],
            })
            .await
            .unwrap();
        let sellers = seller_lookup_from_list(vec![Seller {
            id: SellerId("v".into()),
            tenant_id: TenantIdRef("acme".into()),
            name: "Pedro".into(),
            primary_email: "p@x.com".into(),
            alt_emails: vec![],
            signature_text: String::new(),
            working_hours: None,
            on_vacation: false,
            vacation_until: None,
            preferred_language: None,
            agent_id: None,
            model_override: None,
            notification_settings: None,
            smtp_credential: None,
            draft_template: None,
            system_prompt: None,
            model_provider: None,
            model_id: None,
        }]);
        let gen: Arc<dyn crate::draft::DraftGenerator + Send + Sync> =
            Arc::new(crate::draft::TemplateDraftGenerator::new(
                "   \n   ".into(),
            ));
        let state = Arc::new(
            AdminState::new("secret".into())
                .with_store(Arc::new(store))
                .with_seller_lookup(sellers)
                .with_draft_generator(gen),
        );
        let app = router(state);
        let resp = app
            .oneshot(post("/leads/l-1/drafts/generate", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let v = body_to_json(resp).await;
        assert_eq!(v["error"]["code"], "generator_empty_body");
    }

    // ── Drafts inbox queue (tenant-wide pending) ────────────

    #[tokio::test]
    async fn drafts_inbox_returns_tenant_wide_pending_rows() {
        use crate::lead::{DraftStatus, MessageDirection, NewThreadMessage};
        let state = build_state_with_lead().await;
        let store = state
            .lookup_store(&TenantId::new("acme").unwrap())
            .unwrap();
        store
            .append_thread_message(
                &LeadId("l-1".into()),
                NewThreadMessage {
                    message_id: "d-pending".into(),
                    direction: MessageDirection::Draft,
                    from_label: "AI".into(),
                    body: "pending body".into(),
                    at_ms: 100,
                    draft_status: Some(DraftStatus::Pending), subject: None,
                    signature: None,
                },
            )
            .await
            .unwrap();
        store
            .append_thread_message(
                &LeadId("l-1".into()),
                NewThreadMessage {
                    message_id: "d-approved".into(),
                    direction: MessageDirection::Draft,
                    from_label: "AI".into(),
                    body: "approved body".into(),
                    at_ms: 200,
                    draft_status: Some(DraftStatus::Approved), subject: None,
                    signature: None,
                },
            )
            .await
            .unwrap();
        let app = router(state);
        let resp = app.oneshot(req("/drafts")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["result"]["count"], 1);
        assert_eq!(v["result"]["limit"], 50);
        assert_eq!(v["result"]["drafts"][0]["message_id"], "d-pending");
        assert_eq!(v["result"]["drafts"][0]["lead_id"], "l-1");
        assert_eq!(v["result"]["drafts"][0]["lead_subject"], "Re: cot");
    }

    #[tokio::test]
    async fn drafts_inbox_clamps_limit_above_max() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app.oneshot(req("/drafts?limit=999")).await.unwrap();
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["limit"], 200);
    }

    // ── Manual state transitions ─────────────────────────────

    #[tokio::test]
    async fn transition_legal_move_returns_updated_lead() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app
            .oneshot(post(
                "/leads/l-1/transition",
                serde_json::json!({ "to": "engaged", "reason": "first reply" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_to_json(resp).await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["result"]["from"], "cold");
        assert_eq!(v["result"]["to"], "engaged");
        assert_eq!(v["result"]["reason"], "first reply");
        assert_eq!(v["result"]["lead"]["state"], "engaged");
    }

    #[tokio::test]
    async fn transition_uses_default_reason_when_blank() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app
            .oneshot(post(
                "/leads/l-1/transition",
                serde_json::json!({ "to": "engaged", "reason": "  " }),
            ))
            .await
            .unwrap();
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["reason"], "operator override");
    }

    #[tokio::test]
    async fn transition_invalid_state_returns_400() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app
            .oneshot(post(
                "/leads/l-1/transition",
                serde_json::json!({ "to": "foo" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_to_json(resp).await;
        assert_eq!(v["error"]["code"], "invalid_state");
    }

    #[tokio::test]
    async fn transition_missing_lead_returns_404() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app
            .oneshot(post(
                "/leads/ghost/transition",
                serde_json::json!({ "to": "engaged" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let v = body_to_json(resp).await;
        assert_eq!(v["error"]["code"], "lead_not_found");
    }

    #[tokio::test]
    async fn transition_illegal_returns_422() {
        let state = build_state_with_lead().await;
        // Push to qualified first (cold → engaged →
        // meeting_scheduled → qualified would normally take
        // multiple hops). Easier: jump straight to lost
        // (cold → lost is illegal in the state machine).
        let app = router(state);
        let resp = app
            .oneshot(post(
                "/leads/l-1/transition",
                serde_json::json!({ "to": "qualified" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let v = body_to_json(resp).await;
        assert_eq!(v["error"]["code"], "illegal_transition");
    }

    // ── Per-draft subject override ───────────────────────────

    #[tokio::test]
    async fn create_draft_persists_subject_override() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app
            .oneshot(post(
                "/leads/l-1/drafts",
                serde_json::json!({
                    "body": "Hola Juan",
                    "subject": "Promo de mayo",
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = body_to_json(resp).await;
        assert_eq!(v["result"]["draft"]["subject"], "Promo de mayo");
    }

    #[tokio::test]
    async fn create_draft_filters_blank_subject() {
        let state = build_state_with_lead().await;
        let app = router(state);
        let resp = app
            .oneshot(post(
                "/leads/l-1/drafts",
                serde_json::json!({ "body": "Hola", "subject": "   " }),
            ))
            .await
            .unwrap();
        let v = body_to_json(resp).await;
        // Whitespace-only ⇒ no subject persisted (field
        // omitted from response via `skip_serializing_if`).
        assert!(v["result"]["draft"]["subject"].is_null());
    }

    #[tokio::test]
    async fn update_draft_sets_subject_when_provided() {
        use crate::lead::{DraftStatus, MessageDirection, NewThreadMessage};
        let state = build_state_with_lead().await;
        let store = state
            .lookup_store(&TenantId::new("acme").unwrap())
            .unwrap();
        store
            .append_thread_message(
                &LeadId("l-1".into()),
                NewThreadMessage {
                    message_id: "d-1".into(),
                    direction: MessageDirection::Draft,
                    from_label: "AI".into(),
                    body: "old".into(),
                    at_ms: 1,
                    draft_status: Some(DraftStatus::Pending),
                    subject: None,
                    signature: None,
                },
            )
            .await
            .unwrap();
        // Re-fetch via PUT.
        let req = Request::builder()
            .method("PUT")
            .uri("/leads/l-1/drafts/d-1")
            .header(header::AUTHORIZATION, "Bearer secret")
            .header("X-Tenant-Id", "acme")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "body": "new",
                    "subject": "Promo mayo",
                }))
                .unwrap(),
            ))
            .unwrap();
        let app = router(state.clone());
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Verify directly via store.
        let drafts = store
            .list_drafts(&LeadId("l-1".into()), None)
            .await
            .unwrap();
        assert_eq!(drafts[0].subject.as_deref(), Some("Promo mayo"));
    }

    #[tokio::test]
    async fn update_draft_clears_subject_with_empty_string() {
        use crate::lead::{DraftStatus, MessageDirection, NewThreadMessage};
        let state = build_state_with_lead().await;
        let store = state
            .lookup_store(&TenantId::new("acme").unwrap())
            .unwrap();
        store
            .append_thread_message(
                &LeadId("l-1".into()),
                NewThreadMessage {
                    message_id: "d-1".into(),
                    direction: MessageDirection::Draft,
                    from_label: "AI".into(),
                    body: "x".into(),
                    at_ms: 1,
                    draft_status: Some(DraftStatus::Pending),
                    subject: Some("Old subject".into()),
                    signature: None,
                },
            )
            .await
            .unwrap();
        let req = Request::builder()
            .method("PUT")
            .uri("/leads/l-1/drafts/d-1")
            .header(header::AUTHORIZATION, "Bearer secret")
            .header("X-Tenant-Id", "acme")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "body": "x",
                    "subject": "",
                }))
                .unwrap(),
            ))
            .unwrap();
        let app = router(state.clone());
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let drafts = store
            .list_drafts(&LeadId("l-1".into()), None)
            .await
            .unwrap();
        assert!(drafts[0].subject.is_none());
    }

    #[tokio::test]
    async fn update_draft_leaves_subject_when_field_absent() {
        use crate::lead::{DraftStatus, MessageDirection, NewThreadMessage};
        let state = build_state_with_lead().await;
        let store = state
            .lookup_store(&TenantId::new("acme").unwrap())
            .unwrap();
        store
            .append_thread_message(
                &LeadId("l-1".into()),
                NewThreadMessage {
                    message_id: "d-1".into(),
                    direction: MessageDirection::Draft,
                    from_label: "AI".into(),
                    body: "x".into(),
                    at_ms: 1,
                    draft_status: Some(DraftStatus::Pending),
                    subject: Some("Keep me".into()),
                    signature: None,
                },
            )
            .await
            .unwrap();
        let req = Request::builder()
            .method("PUT")
            .uri("/leads/l-1/drafts/d-1")
            .header(header::AUTHORIZATION, "Bearer secret")
            .header("X-Tenant-Id", "acme")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({ "body": "edited" }))
                    .unwrap(),
            ))
            .unwrap();
        let app = router(state.clone());
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let drafts = store
            .list_drafts(&LeadId("l-1".into()), None)
            .await
            .unwrap();
        assert_eq!(drafts[0].subject.as_deref(), Some("Keep me"));
    }
}
