//! `marketing_lead_mark_qualified` — apply state transition
//! to `Qualified`. Validates current state via the state
//! machine; legal sources are `MeetingScheduled` only (one-
//! step forward). Call-to-`Lost` lives in a separate tool to
//! keep the audit trail clean.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use nexo_microapp_sdk::{ToolError, ToolReply};
use nexo_tool_meta::marketing::{LeadId, LeadState, TenantIdRef};

use crate::error::MarketingError;
use crate::lead::LeadStore;
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
    match store.transition(&parsed.lead_id, LeadState::Qualified).await {
        Ok(updated) => Ok(ToolReply::ok_json(serde_json::json!({
            "ok": true,
            "result": updated,
            "reason": parsed.reason,
        }))),
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
        let r = handle(&TenantId::new("acme").unwrap(), s, args()).await.unwrap();
        let v = r.as_value();
        assert_eq!(v["ok"], true);
        assert_eq!(v["result"]["state"], "qualified");
        assert_eq!(v["reason"], "demo agendada confirmada");
    }

    #[tokio::test]
    async fn cold_to_qualified_invalid_transition() {
        let s = store_with_lead_in(LeadState::Cold).await;
        let r = handle(&TenantId::new("acme").unwrap(), s, args()).await.unwrap();
        let v = r.as_value();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"]["code"], "invalid_transition");
    }
}
