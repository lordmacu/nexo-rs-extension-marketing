//! `marketing_lead_followup_sweep` — cron-driven iterator
//! over leads with `next_check_at_ms <= now`. Returns the
//! due-list payload so the agent generates draft replies +
//! the binary's outbound layer publishes them.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use nexo_microapp_sdk::{ToolError, ToolReply};
use nexo_tool_meta::marketing::TenantIdRef;

use crate::lead::LeadStore;
use crate::tenant::TenantId;

#[derive(Debug, Deserialize)]
struct Args {
    tenant_id: TenantIdRef,
    /// Defaults to `now` UTC ms when caller omits.
    #[serde(default)]
    now_ms: Option<i64>,
    /// Cap so a swamped tenant doesn't blow the cron tick.
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    100
}

pub async fn handle(
    expected_tenant: &TenantId,
    store: Arc<LeadStore>,
    args: Value,
) -> Result<ToolReply, ToolError> {
    let parsed: Args = serde_json::from_value(args)
        .map_err(|e| ToolError::InvalidArguments(format!("followup_sweep args: {e}")))?;
    if parsed.tenant_id.0 != expected_tenant.as_str() {
        return Ok(ToolReply::ok_json(serde_json::json!({
            "ok": false,
            "error": { "code": "tenant_unauthorised" }
        })));
    }
    let now = parsed.now_ms.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    let due = store
        .list_due_for_followup(now, parsed.limit)
        .await
        .map_err(|e| ToolError::Internal(e.to_string()))?;
    Ok(ToolReply::ok_json(serde_json::json!({
        "ok": true,
        "result": {
            "due": due,
            "now_ms": now,
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lead::{LeadStore, NewLead};
    use nexo_tool_meta::marketing::{LeadId, PersonId, SellerId};

    async fn store_with_due_lead(due_at: i64) -> Arc<LeadStore> {
        let s = LeadStore::open(
            std::path::PathBuf::from(":memory:"),
            TenantId::new("acme").unwrap(),
        )
        .await
        .unwrap();
        s.create(NewLead {
            id: LeadId("l-due".into()),
            thread_id: "th".into(),
            subject: "x".into(),
            person_id: PersonId("p".into()),
            seller_id: SellerId("v".into()),
            last_activity_ms: 0,
            score: 0,
            topic_tags: vec![],
            why_routed: vec![],
        })
        .await
        .unwrap();
        s.set_next_check(&LeadId("l-due".into()), Some(due_at), false)
            .await
            .unwrap();
        Arc::new(s)
    }

    #[tokio::test]
    async fn returns_due_leads_under_now() {
        let s = store_with_due_lead(100).await;
        let r = handle(
            &TenantId::new("acme").unwrap(),
            s,
            serde_json::json!({
                "tenant_id": "acme",
                "now_ms": 500,
            }),
        )
        .await
        .unwrap();
        let v = r.as_value();
        assert_eq!(v["ok"], true);
        let due = v["result"]["due"].as_array().unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0]["id"], "l-due");
    }

    #[tokio::test]
    async fn empty_when_nothing_due() {
        let s = store_with_due_lead(10_000).await;
        let r = handle(
            &TenantId::new("acme").unwrap(),
            s,
            serde_json::json!({ "tenant_id": "acme", "now_ms": 100 }),
        )
        .await
        .unwrap();
        let v = r.as_value();
        assert_eq!(v["result"]["due"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn tenant_mismatch_unauthorised() {
        let s = store_with_due_lead(100).await;
        let r = handle(
            &TenantId::new("acme").unwrap(),
            s,
            serde_json::json!({ "tenant_id": "globex" }),
        )
        .await
        .unwrap();
        assert_eq!(r.as_value()["ok"], false);
    }

    #[tokio::test]
    async fn respects_limit() {
        let s = LeadStore::open(
            std::path::PathBuf::from(":memory:"),
            TenantId::new("acme").unwrap(),
        )
        .await
        .unwrap();
        // Create 5 due leads.
        for i in 0..5 {
            s.create(NewLead {
                id: LeadId(format!("l-{i}")),
                thread_id: format!("t-{i}"),
                subject: "x".into(),
                person_id: PersonId("p".into()),
                seller_id: SellerId("v".into()),
                last_activity_ms: 0,
                score: 0,
                topic_tags: vec![],
                why_routed: vec![],
            })
            .await
            .unwrap();
            s.set_next_check(&LeadId(format!("l-{i}")), Some(i as i64), false)
                .await
                .unwrap();
        }
        let r = handle(
            &TenantId::new("acme").unwrap(),
            Arc::new(s),
            serde_json::json!({ "tenant_id": "acme", "now_ms": 1000, "limit": 2 }),
        )
        .await
        .unwrap();
        assert_eq!(r.as_value()["result"]["due"].as_array().unwrap().len(), 2);
    }
}
