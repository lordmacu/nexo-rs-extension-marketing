//! `marketing_lead_schedule_followup` — set / clear next
//! check-in for a lead's followup loop.
//!
//! **F29 sweep:** marketing-specific by design. Wraps
//! `LeadStore::set_next_check`.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use nexo_microapp_sdk::{ToolError, ToolReply};
use nexo_tool_meta::marketing::{LeadId, TenantIdRef};

use crate::lead::LeadStore;
use crate::tenant::TenantId;

/// Default jitter window applied to scheduled followups so a
/// burst of identical-cadence followups doesn't all become due
/// at the same millisecond and overwhelm the next cron tick.
/// 60 seconds is wide enough to spread out a typical 50-lead
/// burst across a single 5-min sweep window without delaying
/// any individual followup meaningfully.
const DEFAULT_JITTER_WINDOW_MS: u32 = 60_000;

#[derive(Debug, Deserialize)]
struct Args {
    tenant_id: TenantIdRef,
    lead_id: LeadId,
    /// `Some(ms)` to schedule, `None` to cancel.
    #[serde(default)]
    next_check_at_ms: Option<i64>,
    /// `true` when this call corresponds to "we just sent a
    /// followup" — bumps the attempts counter. `false` for
    /// "client replied, cancel pending followup".
    #[serde(default)]
    increment_attempts: bool,
    /// Optional override for the jitter window the store
    /// applies before persist. Default = 60 s. Pass `0` to
    /// disable jitter entirely (useful when the agent picks an
    /// exact time intentionally — a meeting reminder, e.g.).
    #[serde(default)]
    jitter_window_ms: Option<u32>,
}

pub async fn handle(
    expected_tenant: &TenantId,
    store: Arc<LeadStore>,
    args: Value,
) -> Result<ToolReply, ToolError> {
    let parsed: Args = serde_json::from_value(args).map_err(|e| {
        ToolError::InvalidArguments(format!("schedule_followup args: {e}"))
    })?;
    if parsed.tenant_id.0 != expected_tenant.as_str() {
        return Ok(ToolReply::ok_json(serde_json::json!({
            "ok": false,
            "error": { "code": "tenant_unauthorised" }
        })));
    }
    let jitter = parsed
        .jitter_window_ms
        .unwrap_or(DEFAULT_JITTER_WINDOW_MS);
    let updated = store
        .set_next_check_with_jitter(
            &parsed.lead_id,
            parsed.next_check_at_ms,
            parsed.increment_attempts,
            jitter,
        )
        .await
        .map_err(|e| ToolError::Internal(e.to_string()))?;
    Ok(ToolReply::ok_json(serde_json::json!({
        "ok": true,
        "result": updated,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lead::{LeadStore, NewLead};
    use nexo_tool_meta::marketing::{PersonId, SellerId};

    async fn fresh_store(t: &str) -> Arc<LeadStore> {
        let s = LeadStore::open(
            std::path::PathBuf::from(":memory:"),
            TenantId::new(t).unwrap(),
        )
        .await
        .unwrap();
        s.create(NewLead {
            id: LeadId("l1".into()),
            thread_id: "th-l1".into(),
            subject: "Re".into(),
            person_id: PersonId("p".into()),
            seller_id: SellerId("v".into()),
            last_activity_ms: 1,
            score: 0,
            topic_tags: vec![],
            why_routed: vec![],
        })
        .await
        .unwrap();
        Arc::new(s)
    }

    #[tokio::test]
    async fn sets_next_check_and_increments_attempts() {
        let s = fresh_store("acme").await;
        let r = handle(
            &TenantId::new("acme").unwrap(),
            s.clone(),
            serde_json::json!({
                "tenant_id": "acme",
                "lead_id": "l1",
                "next_check_at_ms": 12345,
                "increment_attempts": true,
                // Disable jitter so the assertion can compare
                // exactly. Production callers leave this unset
                // and pick up the 60 s default.
                "jitter_window_ms": 0,
            }),
        )
        .await
        .unwrap();
        let v = r.as_value();
        assert_eq!(v["ok"], true);
        assert_eq!(v["result"]["next_check_at_ms"], 12345);
        assert_eq!(v["result"]["followup_attempts"], 1);
    }

    #[tokio::test]
    async fn applies_jitter_within_window() {
        let s = fresh_store("acme").await;
        let r = handle(
            &TenantId::new("acme").unwrap(),
            s.clone(),
            serde_json::json!({
                "tenant_id": "acme",
                "lead_id": "l1",
                "next_check_at_ms": 1_700_000_000_000_i64,
                "increment_attempts": false,
                "jitter_window_ms": 60_000,
            }),
        )
        .await
        .unwrap();
        let v = r.as_value();
        let got = v["result"]["next_check_at_ms"].as_i64().unwrap();
        // Within ±30 s of base.
        assert!(
            (1_699_999_970_000..=1_700_000_030_000).contains(&got),
            "expected jitter inside ±30s, got {got}"
        );
    }

    #[tokio::test]
    async fn clears_next_check_keeps_attempts() {
        let s = fresh_store("acme").await;
        // Pre-arm an attempt + deadline.
        s.set_next_check(&LeadId("l1".into()), Some(1), true).await.unwrap();
        let r = handle(
            &TenantId::new("acme").unwrap(),
            s.clone(),
            serde_json::json!({
                "tenant_id": "acme",
                "lead_id": "l1",
                "next_check_at_ms": null,
                "increment_attempts": false,
            }),
        )
        .await
        .unwrap();
        let v = r.as_value();
        assert!(v["result"]["next_check_at_ms"].is_null());
        assert_eq!(v["result"]["followup_attempts"], 1);
    }

    #[tokio::test]
    async fn tenant_mismatch_unauthorised() {
        let s = fresh_store("acme").await;
        let r = handle(
            &TenantId::new("acme").unwrap(),
            s,
            serde_json::json!({
                "tenant_id": "globex",
                "lead_id": "l1",
                "next_check_at_ms": 1,
                "increment_attempts": false,
            }),
        )
        .await
        .unwrap();
        assert_eq!(r.as_value()["ok"], false);
    }
}
