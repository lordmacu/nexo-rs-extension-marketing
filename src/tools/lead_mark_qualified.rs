//! `marketing_lead_mark_qualified` — apply state transition
//! to `Qualified`. Validates current state via the state
//! machine; legal sources are `MeetingScheduled` only (one-
//! step forward). Call-to-`Lost` lives in a separate tool to
//! keep the audit trail clean.
//!
//! M15.41 — when a `BrokerSender` + `VendedorLookup` are
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
use crate::lead::LeadStore;
use crate::notification::{
    maybe_notify_lead_transitioned, NotificationOutcome, VendedorLookup,
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
    vendedores: Option<&VendedorLookup>,
    broker: Option<&BrokerSender>,
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
            // Fire-and-forget notification publish post-transition.
            // Only fires when ALL of: tool ctx provides broker,
            // vendedor lookup is wired, vendedor opted in via
            // `on_lead_transitioned`, agent_id is bound, channel
            // is non-Disabled. Misses (NotConfigured / EventDisabled
            // / NoAgentBound / ChannelDisabled) log at trace.
            if let (Some(lookup), Some(sender), Some(from)) =
                (vendedores, broker, from_state)
            {
                match maybe_notify_lead_transitioned(
                    expected_tenant,
                    lookup,
                    &updated,
                    from,
                    LeadState::Qualified,
                    &parsed.reason,
                ) {
                    NotificationOutcome::Publish { topic, payload } => {
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
    use nexo_tool_meta::marketing::{PersonId, VendedorId};

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
            vendedor_id: VendedorId("v".into()),
            last_activity_ms: 1,
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
        let r = handle(&TenantId::new("acme").unwrap(), s, None, None, args())
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
        let r = handle(&TenantId::new("acme").unwrap(), s, None, None, args())
            .await
            .unwrap();
        let v = r.as_value();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "invalid_transition");
    }
}
