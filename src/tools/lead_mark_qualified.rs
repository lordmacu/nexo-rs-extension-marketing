//! `marketing_lead_mark_qualified` — apply state transition
//! to `Qualified`. Validates current state via the state
//! machine; legal sources are `MeetingScheduled` only (one-
//! step forward). Call-to-`Lost` lives in a separate tool to
//! keep the audit trail clean.
//!
//! M15.41 — when a `BrokerSender` + `SellerLookup` are
//! available (Phase 81.17.c.ctx tool dispatch), publishes a
//! `LeadTransitioned` notification post-success. Fire-and-
//! forget — failure logs warn, never sinks the tool reply.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use nexo_microapp_sdk::plugin::BrokerSender;
use nexo_microapp_sdk::{ToolError, ToolReply};
use nexo_tool_meta::marketing::{LeadId, LeadState, TenantIdRef};

use crate::error::MarketingError;
use crate::lead::{state_str as state_label, LeadStore};
use crate::notification::{
    maybe_notify_lead_transitioned, NotificationOutcome, SellerLookup,
};
use crate::tenant::TenantId;

#[derive(Debug, Deserialize)]
struct Args {
    tenant_id: TenantIdRef,
    lead_id: LeadId,
    /// Operator-facing reason — gets stored on the
    /// transition event for audit.
    #[serde(default)]
    reason: String,
}

pub async fn handle(
    expected_tenant: &TenantId,
    store: Arc<LeadStore>,
    sellers: Option<&SellerLookup>,
    templates: Option<&crate::notification::TemplateLookup>,
    broker: Option<&BrokerSender>,
    dedup: Option<&crate::notification_dedup::DedupCache>,
    audit: Option<&crate::audit::AuditLog>,
    args: Value,
) -> Result<ToolReply, ToolError> {
    let parsed: Args = serde_json::from_value(args)
        .map_err(|e| ToolError::InvalidArguments(format!("mark_qualified args: {e}")))?;
    if parsed.tenant_id.0 != expected_tenant.as_str() {
        return Ok(ToolReply::ok_json(serde_json::json!({
            "ok": false,
            "error": { "code": "tenant_unauthorised" }
        })));
    }
    // Snapshot the current state BEFORE the transition so the
    // notification carries `from → to` accurately.
    let before = store
        .get(&parsed.lead_id)
        .await
        .map_err(|e| ToolError::Internal(e.to_string()))?;
    let from_state = before.as_ref().map(|l| l.state);

    match store.transition(&parsed.lead_id, LeadState::Qualified).await {
        Ok(updated) => {
            // M15.23.c — audit row for the transition.
            // Fire-and-forget; failure logs warn but never
            // sinks the tool reply.
            if let (Some(log), Some(from)) = (audit, from_state) {
                let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
                let event = crate::audit::AuditEvent::LeadTransitioned {
                    tenant_id: expected_tenant.as_str().to_string(),
                    lead_id: updated.id.0.clone(),
                    from: state_label(from).into(),
                    to: state_label(LeadState::Qualified).into(),
                    reason: parsed.reason.clone(),
                    at_ms: now_ms,
                };
                if let Err(e) = log.record(event).await {
                    tracing::warn!(
                        target: "tool.marketing.lead_mark_qualified",
                        error = %e,
                        lead_id = %updated.id.0,
                        "audit record failed (non-fatal)"
                    );
                }
            }
            // Fire-and-forget notification publish post-transition.
            // Only fires when ALL of: tool ctx provides broker,
            // seller lookup is wired, seller opted in via
            // `on_lead_transitioned`, agent_id is bound, channel
            // is non-Disabled. Misses (NotConfigured / EventDisabled
            // / NoAgentBound / ChannelDisabled) log at trace.
            if let (Some(lookup), Some(sender), Some(from)) =
                (sellers, broker, from_state)
            {
                match maybe_notify_lead_transitioned(
                    expected_tenant,
                    lookup,
                    templates,
                    &updated,
                    from,
                    LeadState::Qualified,
                    &parsed.reason,
                ) {
                    NotificationOutcome::Publish { topic, payload } => {
                        // M15.53 / F9 — dedupe under TTL window so
                        // a tool retry inside the same minute
                        // doesn't ping the operator twice.
                        let dedup_key = crate::notification_dedup::DedupKey::new(
                            expected_tenant.as_str(),
                            &updated.id.0,
                            "lead_transitioned",
                            payload.at_ms,
                        );
                        let is_dup = dedup
                            .map(|c| c.is_duplicate(&dedup_key))
                            .unwrap_or(false);
                        let channel_str = if is_dup {
                            crate::audit::CHANNEL_DEDUPED
                        } else {
                            crate::audit::channel_label(&payload.channel)
                        };
                        if is_dup {
                            tracing::debug!(
                                target: "tool.marketing.lead_mark_qualified",
                                key = %dedup_key.as_str(),
                                "lead_transitioned notification deduped"
                            );
                        } else {
                            let json_payload = serde_json::to_value(&payload)
                                .unwrap_or_else(|_| serde_json::json!({}));
                            let event = nexo_microapp_sdk::BrokerEvent::new(
                                topic.clone(),
                                "marketing.notification",
                                json_payload,
                            );
                            if let Err(e) = sender.publish(&topic, event).await {
                                tracing::warn!(
                                    target: "tool.marketing.lead_mark_qualified",
                                    error = %e,
                                    topic = %topic,
                                    "lead_transitioned notification publish failed (non-fatal)"
                                );
                            }
                        }
                        // M15.23.c — audit row regardless of dedupe
                        // so compliance sees every attempt.
                        if let Some(log) = audit {
                            let event = crate::audit::AuditEvent::NotificationPublished {
                                tenant_id: expected_tenant.as_str().to_string(),
                                lead_id: updated.id.0.clone(),
                                seller_id: updated.seller_id.0.clone(),
                                notification_kind: "lead_transitioned".into(),
                                channel: channel_str.to_string(),
                                at_ms: payload.at_ms.max(0) as u64,
                            };
                            if let Err(e) = log.record(event).await {
                                tracing::warn!(
                                    target: "tool.marketing.lead_mark_qualified",
                                    error = %e,
                                    "audit record failed (non-fatal)"
                                );
                            }
                        }
                    }
                    other => {
                        tracing::trace!(
                            target: "tool.marketing.lead_mark_qualified",
                            outcome = ?other,
                            "lead_transitioned notification skipped"
                        );
                    }
                }
            }
            Ok(ToolReply::ok_json(serde_json::json!({
                "ok": true,
                "result": updated,
                "reason": parsed.reason,
            })))
        }
        Err(MarketingError::InvalidTransition { from, to, .. }) => {
            Ok(ToolReply::ok_json(serde_json::json!({
                "ok": false,
                "error": {
                    "code": "invalid_transition",
                    "from": format!("{from:?}"),
                    "to": format!("{to:?}"),
                }
            })))
        }
        Err(e) => Err(ToolError::Internal(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lead::{LeadStore, NewLead};
    use nexo_tool_meta::marketing::{PersonId, SellerId};

    async fn store_with_lead_in(state: LeadState) -> Arc<LeadStore> {
        let s = LeadStore::open(
            std::path::PathBuf::from(":memory:"),
            TenantId::new("acme").unwrap(),
        )
        .await
        .unwrap();
        s.create(NewLead {
            id: LeadId("l1".into()),
            thread_id: "th".into(),
            subject: "x".into(),
            person_id: PersonId("p".into()),
            seller_id: SellerId("v".into()),
            last_activity_ms: 1,
            score: 0,
            topic_tags: vec![],
            why_routed: vec![],
        })
        .await
        .unwrap();
        // Walk forward using legal transitions when needed.
        if state != LeadState::Cold {
            s.transition(&LeadId("l1".into()), LeadState::Engaged).await.unwrap();
        }
        if matches!(state, LeadState::MeetingScheduled | LeadState::Qualified) {
            s.transition(&LeadId("l1".into()), LeadState::MeetingScheduled)
                .await
                .unwrap();
        }
        Arc::new(s)
    }

    fn args() -> Value {
        serde_json::json!({
            "tenant_id": "acme",
            "lead_id": "l1",
            "reason": "demo agendada confirmada"
        })
    }

    #[tokio::test]
    async fn meeting_to_qualified_persists() {
        let s = store_with_lead_in(LeadState::MeetingScheduled).await;
        let r = handle(&TenantId::new("acme").unwrap(), s, None, None, None, None, None, args())
            .await
            .unwrap();
        let v = r.as_value();
        assert_eq!(v["ok"], true);
        assert_eq!(v["result"]["state"], "qualified");
        assert_eq!(v["reason"], "demo agendada confirmada");
    }

    #[tokio::test]
    async fn cold_to_qualified_invalid_transition() {
        let s = store_with_lead_in(LeadState::Cold).await;
        let r = handle(&TenantId::new("acme").unwrap(), s, None, None, None, None, None, args())
            .await
            .unwrap();
        let v = r.as_value();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "invalid_transition");
    }
}
