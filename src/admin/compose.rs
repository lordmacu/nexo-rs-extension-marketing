//! `POST /admin/compose/send` — operator-initiated outbound
//! email (cold outreach).
//!
//! Today the outbound flow only fires on operator-approved
//! drafts threaded against an existing inbound. Compose lets
//! the operator START a conversation: pick a recipient + seller
//! + subject + body, the handler creates a fresh Lead + Person
//! row, applies the tracking signer (open pixel + click
//! rewrite) when configured, and dispatches via the same
//! OutboundPublisher path the approve flow uses — so cold
//! outreach gets the same compliance gate, the same
//! idempotency reservation, and the same engagement
//! attribution as a reply.
//!
//! Caveat: replies from the recipient may land as a separate
//! Lead because the email plugin generates its own Message-Id
//! that we don't currently track. Person-level dedup still
//! works (the customer's email maps to the same Person row),
//! so the operator can manually merge the two leads via the
//! duplicate-merge prompt. A future iteration can persist
//! outbound Message-Ids and resolve replies back to the
//! originating Lead.

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
use uuid::Uuid;

use nexo_microapp_sdk::identity::Person;
use nexo_tool_meta::marketing::{EnrichmentStatus, LeadId, PersonId, TenantIdRef};

use super::AdminState;
use crate::tenant::TenantId;

#[derive(Debug, Deserialize)]
pub struct ComposeBody {
    pub to_email: String,
    #[serde(default)]
    pub to_name: Option<String>,
    pub subject: String,
    pub body: String,
    pub seller_id: String,
    /// When unset, defaults to `true`. Operator-disable when a
    /// transactional outbound shouldn't carry attribution
    /// signals (e.g. legal templates).
    #[serde(default = "default_with_tracking")]
    pub with_tracking: bool,
}

fn default_with_tracking() -> bool {
    true
}

pub async fn send_handler(
    State(state): State<Arc<AdminState>>,
    Extension(tenant_id): Extension<TenantId>,
    Json(body): Json<ComposeBody>,
) -> Response {
    use crate::broker::{DispatchInput, OutboundDispatchResult, OutboundEmail};

    // ── Marketing-paused gate ────────────────────────────────
    // Same gate the draft-generate handler uses — paused tenant
    // shouldn't fire automated outbound.
    if let Some(state_cache) = state.marketing_state.as_ref() {
        let now_ms = Utc::now().timestamp_millis();
        if !state_cache.is_enabled(tenant_id.as_str(), now_ms).await {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "marketing_paused",
                "marketing extension is paused for this tenant",
            );
        }
    }

    // ── Validate input ───────────────────────────────────────
    let to_email = body.to_email.trim().to_ascii_lowercase();
    if !to_email.contains('@') {
        return error(StatusCode::BAD_REQUEST, "bad_recipient", "to_email is malformed");
    }
    let subject = body.subject.trim().to_string();
    if subject.is_empty() {
        return error(StatusCode::BAD_REQUEST, "bad_subject", "subject required");
    }
    let body_text = body.body.trim().to_string();
    if body_text.is_empty() {
        return error(StatusCode::BAD_REQUEST, "bad_body", "body required");
    }

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
                "OutboundPublisher not wired",
            );
        }
    };
    let broker_sender = match state.broker_sender.load_full() {
        Some(s) => s,
        None => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "broker_not_attached",
                "BrokerSender not yet captured; trigger any plugin event first",
            );
        }
    };
    let sellers_lookup = match state.seller_lookup.as_ref() {
        Some(l) => l.load_full(),
        None => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "sellers_not_loaded",
                "seller index not mounted",
            );
        }
    };
    let seller = match sellers_lookup.get(&nexo_tool_meta::marketing::SellerId(
        body.seller_id.clone(),
    )) {
        Some(s) => s.clone(),
        None => {
            return error(
                StatusCode::NOT_FOUND,
                "seller_not_found",
                &format!("seller {:?} missing from sellers.yaml", body.seller_id),
            );
        }
    };
    let smtp = match seller.smtp_credential.as_ref() {
        Some(c) => c.clone(),
        None => {
            return error(
                StatusCode::PRECONDITION_FAILED,
                "seller_missing_smtp",
                &format!("seller {:?} has no smtp_credential", seller.id.0),
            );
        }
    };

    // ── Create / upsert Person ───────────────────────────────
    let person_id = deterministic_person_id(&to_email);
    let now_ms = Utc::now().timestamp_millis();
    if let Some(persons) = state.persons.as_ref() {
        let person = Person {
            id: person_id.clone(),
            tenant_id: TenantIdRef(tenant_id.as_str().into()),
            primary_name: body
                .to_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .unwrap_or_else(|| {
                    to_email.split_once('@').map(|(l, _)| l.to_string()).unwrap_or_else(|| to_email.clone())
                }),
            primary_email: to_email.clone(),
            alt_emails: Vec::new(),
            company_id: None,
            enrichment_status: EnrichmentStatus::None,
            enrichment_confidence: 1.0, // operator-typed = ground truth
            tags: Vec::new(),
            created_at_ms: now_ms,
            last_seen_at_ms: now_ms,
        };
        if let Err(e) = persons.upsert(tenant_id.as_str(), &person).await {
            tracing::warn!(
                target: "extension.marketing.compose",
                error = %e,
                "person upsert failed; proceeding with placeholder id (lead still creates)",
            );
        }
        // PersonEmail link: AdminState doesn't currently
        // surface that store. Best-effort skipped here — the
        // broker hop's resolver populates it on the first
        // inbound from this address.
    }

    // ── Create Lead ──────────────────────────────────────────
    let lead_id = LeadId(Uuid::new_v4().to_string());
    let thread_id = format!("compose:{}", lead_id.0);
    let new_lead = crate::lead::NewLead {
        id: lead_id.clone(),
        thread_id: thread_id.clone(),
        subject: subject.clone(),
        person_id: person_id.clone(),
        seller_id: seller.id.clone(),
        last_activity_ms: now_ms,
        score: 0,
        topic_tags: Vec::new(),
        why_routed: vec!["operator_compose".into()],
    };
    let lead = match store.create(new_lead).await {
        Ok(l) => l,
        Err(e) => return marketing_error(e),
    };

    // ── Compose HTML body + tracking ────────────────────────
    let mut html_body = format!(
        "<html><body>{}<br/><br/>{}</body></html>",
        body_text.replace('\n', "<br/>"),
        seller.signature_text.replace('\n', "<br/>"),
    );
    let tracking_msg_id = if body.with_tracking {
        if let Some(tracking) = state.tracking_for_outbound.as_ref() {
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
                        target: "extension.marketing.compose",
                        error = %e,
                        "prepare_outbound_email failed; outbound will lack tracking signals",
                    );
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };
    let outbound_msg_id = tracking_msg_id
        .as_ref()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| format!("outbound-{}", Uuid::new_v4()));

    // RFC 5322 Message-Id we pre-supply so the email plugin
    // uses it verbatim. The recipient's reply will echo this
    // id in `In-Reply-To` — the broker hop's reply path
    // resolves it via `find_lead_by_outbound_message_id` so
    // the reply threads back to THIS lead instead of creating
    // a sibling.
    //
    // Format: `<{lead_id}.{outbound_msg_id}@{seller_domain}>`
    // — angle brackets are added by the email plugin's
    // builder, we store the bare value here.
    let seller_domain = seller
        .primary_email
        .split_once('@')
        .map(|(_, d)| d.to_string())
        .unwrap_or_else(|| "marketing.local".into());
    let rfc_message_id = format!(
        "{}.{}@{}",
        lead_id.0, outbound_msg_id, seller_domain
    );
    if let Err(e) = store
        .record_outbound_message_id(
            &rfc_message_id,
            &lead_id,
            &thread_id,
            now_ms,
        )
        .await
    {
        tracing::warn!(
            target: "extension.marketing.compose",
            error = %e,
            "outbound_message_id persist failed; reply may not thread back to this lead",
        );
    }

    // ── Dispatch via OutboundPublisher ──────────────────────
    let email = OutboundEmail {
        to: vec![to_email.clone()],
        cc: vec![],
        bcc: vec![],
        subject: subject.clone(),
        body: html_body,
        // Cold outreach — no thread to reply against.
        in_reply_to: None,
        references: vec![],
        message_id: Some(rfc_message_id.clone()),
    };
    let dispatch_input = DispatchInput {
        thread_id: thread_id.clone(),
        // Synthetic draft id so the publisher's idempotency
        // reservation stays single-shot (a duplicate compose
        // with same outbound_msg_id collides correctly).
        draft_id: outbound_msg_id.clone(),
        seller_smtp_instance: smtp.instance.clone(),
        email,
    };
    let outcome = outbound.dispatch(dispatch_input);
    let (topic, command) = match outcome {
        OutboundDispatchResult::Publish { topic, command, .. } => (topic, command),
        OutboundDispatchResult::Skipped { reason, .. } => {
            return error(
                StatusCode::CONFLICT,
                "outbound_skipped",
                &format!("publisher reservation says compose already dispatched: {reason}"),
            );
        }
        OutboundDispatchResult::Blocked { reason, .. } => {
            return error(
                StatusCode::PRECONDITION_FAILED,
                "outbound_blocked",
                &format!("compliance gate blocked compose: {reason}"),
            );
        }
    };

    let payload = serde_json::to_value(&command).unwrap_or_else(|_| json!({}));
    let event = nexo_microapp_sdk::BrokerEvent::new(
        topic.clone(),
        "marketing.compose",
        payload,
    );
    if let Err(e) = broker_sender.publish(&topic, event).await {
        return error(
            StatusCode::BAD_GATEWAY,
            "broker_publish_failed",
            &format!("publish to {topic}: {e}"),
        );
    }

    // ── Persist outbound thread row ─────────────────────────
    let outbound_msg = crate::lead::NewThreadMessage {
        message_id: outbound_msg_id.clone(),
        direction: crate::lead::MessageDirection::Outbound,
        from_label: seller.name.clone(),
        body: command.body.clone(),
        at_ms: now_ms,
        draft_status: None,
        subject: Some(subject.clone()),
        signature: None,
    };
    if let Err(e) = store.append_thread_message(&lead_id, outbound_msg).await {
        tracing::warn!(
            target: "extension.marketing.compose",
            error = %e,
            lead_id = %lead.id.0,
            "compose outbound thread_message persist failed (non-fatal — broker already published)",
        );
    }

    // ── Audit ────────────────────────────────────────────────
    if let Some(audit) = state.audit.as_ref() {
        let event = crate::audit::AuditEvent::NotificationPublished {
            tenant_id: tenant_id.as_str().to_string(),
            lead_id: lead.id.0.clone(),
            seller_id: lead.seller_id.0.clone(),
            notification_kind: "operator_compose".into(),
            channel: "smtp_outbound".into(),
            at_ms: now_ms.max(0) as u64,
        };
        if let Err(e) = audit.record(event).await {
            tracing::warn!(
                target: "extension.marketing.compose",
                error = %e,
                "compose audit record failed (non-fatal)",
            );
        }
    }

    (
        StatusCode::CREATED,
        Json(json!({
            "ok": true,
            "result": {
                "lead_id": lead.id.0,
                "thread_id": thread_id,
                "outbound_message_id": outbound_msg_id,
                "rfc_message_id": rfc_message_id,
                "tracking_msg_id": tracking_msg_id.map(|m| m.as_str().to_string()),
                "topic": topic,
            },
        })),
    )
        .into_response()
}

fn deterministic_person_id(email: &str) -> PersonId {
    let ns = Uuid::NAMESPACE_DNS;
    let v5 = Uuid::new_v5(&ns, email.to_ascii_lowercase().as_bytes());
    PersonId(format!("placeholder-{v5}"))
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

fn marketing_error(e: crate::error::MarketingError) -> Response {
    server_error("marketing_error", &e.to_string())
}

