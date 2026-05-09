//! `marketing_lead_followup_sweep` — cron-driven iterator
//! over leads with `next_check_at_ms <= now`. Returns the
//! due-list payload so the agent generates draft replies +
//! the binary's outbound layer publishes them.
//!
//! **F29 sweep:** marketing-specific by design. Reads
//! `LeadStore::list_due_for_followup` directly. Generic
//! "due-row sweeper" pattern would only earn its keep with
//! a second consumer (campaign engine, scheduled-task UI).

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
    /// Operates as a soft limit — the tool returns up to this
    /// many leads + a `more_due` flag the agent runtime uses
    /// to schedule the next tick early.
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    25
}

/// Suggested gap between this tick and the next when there are
/// still leads queued. Lets the agent runtime spread a burst
/// across multiple ticks so the LLM provider doesn't see N
/// concurrent calls fired at the same instant — combines with
/// the per-lead jitter on `next_check_at_ms` to keep load smooth.
const NEXT_TICK_HINT_MS_WHEN_BACKLOG: i64 = 30_000;
/// Default cadence hint when nothing is due — lets the agent
/// runtime back off to a slower schedule.
const NEXT_TICK_HINT_MS_WHEN_IDLE: i64 = 300_000;

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
    // Pull `limit + 1` so we know whether there's more queued
    // beyond the cap. We trim the extra row before returning;
    // its presence flips `more_due` to surface "schedule next
    // tick sooner" to the agent runtime.
    let pull_limit = parsed.limit.saturating_add(1);
    let mut due = store
        .list_due_for_followup(now, pull_limit)
        .await
        .map_err(|e| ToolError::Internal(e.to_string()))?;
    let more_due = due.len() as u32 > parsed.limit;
    if more_due {
        due.truncate(parsed.limit as usize);
    }
    let next_tick_hint_ms = if more_due {
        NEXT_TICK_HINT_MS_WHEN_BACKLOG
    } else {
        NEXT_TICK_HINT_MS_WHEN_IDLE
    };
    Ok(ToolReply::ok_json(serde_json::json!({
        "ok": true,
        "result": {
            "due": due,
            "now_ms": now,
            // Spreading hints — let the agent runtime decide
            // when to fire next sweep without hardcoding a
            // schedule on the agent side.
            "more_due": more_due,
            "next_tick_hint_ms": next_tick_hint_ms,
            "limit_applied": parsed.limit,
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
        let v = r.as_value();
        assert_eq!(v["result"]["due"].as_array().unwrap().len(), 2);
        // Backlog signal: 3 more queued beyond the limit.
        assert_eq!(v["result"]["more_due"], true);
        assert_eq!(v["result"]["next_tick_hint_ms"], NEXT_TICK_HINT_MS_WHEN_BACKLOG);
        assert_eq!(v["result"]["limit_applied"], 2);
    }

    #[tokio::test]
    async fn signals_idle_hint_when_no_backlog() {
        let s = store_with_due_lead(100).await;
        let r = handle(
            &TenantId::new("acme").unwrap(),
            s,
            serde_json::json!({ "tenant_id": "acme", "now_ms": 500, "limit": 25 }),
        )
        .await
        .unwrap();
        let v = r.as_value();
        assert_eq!(v["result"]["due"].as_array().unwrap().len(), 1);
        assert_eq!(v["result"]["more_due"], false);
        assert_eq!(v["result"]["next_tick_hint_ms"], NEXT_TICK_HINT_MS_WHEN_IDLE);
    }
}
