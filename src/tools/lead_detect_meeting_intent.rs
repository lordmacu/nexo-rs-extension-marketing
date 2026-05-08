//! `marketing_lead_detect_meeting_intent` — dual-stage
//! classifier. Stage 1 regex catches obvious "yes Tuesday at
//! 3pm" / "sí, martes a las 15:00" / Calendly URL. Stage 2
//! LLM lands later (M18); this commit ships the regex stage
//! + a `Some(0.5)` confidence stub when text is ambiguous so
//! callers can defer to operator review.
//!
//! M15.41 — when the optional `lead_id` arg is present + the
//! classifier hits confidence ≥ 0.7 + the tool ctx provides
//! a `BrokerSender` + the seller opted in via
//! `on_meeting_intent`, publishes a `MeetingIntent`
//! notification. Fire-and-forget — never sinks the tool reply.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use nexo_microapp_sdk::plugin::BrokerSender;
use nexo_microapp_sdk::{ToolError, ToolReply};
use nexo_tool_meta::marketing::{LeadId, MeetingIntent, TenantIdRef};

use crate::lead::LeadStore;
use crate::notification::{
    maybe_notify_meeting_intent, NotificationOutcome, SellerLookup,
};
use crate::tenant::TenantId;

/// Confidence floor for firing a `MeetingIntent` notification.
/// 0.7 = the same threshold the SDK fallback chain uses for
/// "trust the source"; below this we treat the classifier
/// output as a hint that the operator should still confirm.
const NOTIFY_CONFIDENCE_FLOOR: f32 = 0.7;

#[derive(Debug, Deserialize)]
struct Args {
    tenant_id: TenantIdRef,
    body: String,
    /// Last 3-5 messages in the thread, oldest first. Used
    /// later for context-aware LLM stage.
    #[serde(default)]
    history: Vec<String>,
    /// Optional lead context. When present + confidence high,
    /// the tool publishes a `MeetingIntent` notification.
    /// Without it, the tool stays pure-classifier (back-compat
    /// with callers that just want the regex outcome).
    #[serde(default)]
    lead_id: Option<LeadId>,
}

pub async fn handle(
    expected_tenant: &TenantId,
    store: Arc<LeadStore>,
    sellers: Option<&SellerLookup>,
    templates: Option<&crate::notification::TemplateLookup>,
    broker: Option<&BrokerSender>,
    dedup: Option<&crate::notification_dedup::DedupCache>,
    args: Value,
) -> Result<ToolReply, ToolError> {
    let parsed: Args = serde_json::from_value(args).map_err(|e| {
        ToolError::InvalidArguments(format!("detect_meeting_intent args: {e}"))
    })?;
    if parsed.tenant_id.0 != expected_tenant.as_str() {
        return Ok(ToolReply::ok_json(serde_json::json!({
            "ok": false,
            "error": { "code": "tenant_unauthorised" }
        })));
    }

    let outcome = classify_regex(&parsed.body, &parsed.history);

    // Publish notification when ALL of:
    //   - outcome confidence ≥ floor
    //   - lead_id arg supplied + the lead exists for this tenant
    //   - tool ctx provides BrokerSender + seller lookup
    //   - seller opted in (gated inside the classifier)
    if outcome.confidence >= NOTIFY_CONFIDENCE_FLOOR {
        if let (Some(lead_id), Some(lookup), Some(sender)) =
            (&parsed.lead_id, sellers, broker)
        {
            // Look up the lead — needed for the payload's
            // seller_id resolution. Errors degrade silently
            // so a transient store hiccup never sinks the tool
            // reply.
            if let Ok(Some(lead)) = store.get(lead_id).await {
                match maybe_notify_meeting_intent(
                    expected_tenant,
                    lookup,
                    templates,
                    &lead,
                    outcome.confidence,
                    &outcome.evidence,
                ) {
                    NotificationOutcome::Publish { topic, payload } => {
                        // M15.53 / F9 — dedupe under TTL window so
                        // a tool retry inside the same minute
                        // doesn't ping the operator twice.
                        let dedup_key = crate::notification_dedup::DedupKey::new(
                            expected_tenant.as_str(),
                            &lead.id.0,
                            "meeting_intent",
                            payload.at_ms,
                        );
                        let is_dup = dedup
                            .map(|c| c.is_duplicate(&dedup_key))
                            .unwrap_or(false);
                        if is_dup {
                            tracing::debug!(
                                target: "tool.marketing.lead_detect_meeting_intent",
                                key = %dedup_key.as_str(),
                                "meeting_intent notification deduped"
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
                                    target: "tool.marketing.lead_detect_meeting_intent",
                                    error = %e,
                                    topic = %topic,
                                    "meeting_intent notification publish failed (non-fatal)"
                                );
                            }
                        }
                    }
                    other => {
                        tracing::trace!(
                            target: "tool.marketing.lead_detect_meeting_intent",
                            outcome = ?other,
                            "meeting_intent notification skipped"
                        );
                    }
                }
            }
        }
    }

    Ok(ToolReply::ok_json(serde_json::json!({
        "ok": true,
        "result": outcome,
    })))
}

fn classify_regex(body: &str, _history: &[String]) -> MeetingIntent {
    let lower = body.to_ascii_lowercase();

    // Calendly / book.us / cal.com URL → high confidence.
    if lower.contains("calendly.com/")
        || lower.contains("cal.com/")
        || lower.contains("savvycal.com/")
    {
        return MeetingIntent {
            accepted: true,
            proposed_time_iso: None,
            confidence: 0.90,
            evidence: extract_first_url(body).unwrap_or_else(|| "calendar link".into()),
        };
    }

    // Spanish + English affirmations next to a day/hour.
    let positive = [
        "yes",
        "sounds good",
        "works for me",
        "perfect",
        "agendado",
        "agenda",
        "sí",
        "si,",
        "claro",
        "perfecto",
        "confirmado",
        "confirma",
    ];
    let day_hour_hint = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "lunes",
        "martes",
        "miércoles",
        "miercoles",
        "jueves",
        "viernes",
        "am",
        "pm",
        "h ",
        "h.",
        ":00",
        ":30",
    ];
    let pos_hit = positive.iter().any(|p| lower.contains(p));
    let dh_hit = day_hour_hint.iter().any(|h| lower.contains(h));
    if pos_hit && dh_hit {
        return MeetingIntent {
            accepted: true,
            proposed_time_iso: None,
            confidence: 0.78,
            evidence: short_excerpt(body),
        };
    }

    // Ambiguous — caller routes to operator confirmation.
    MeetingIntent {
        accepted: false,
        proposed_time_iso: None,
        confidence: 0.0,
        evidence: short_excerpt(body),
    }
}

fn short_excerpt(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= 120 {
        trimmed.to_string()
    } else {
        format!("{}…", &trimmed[..120])
    }
}

fn extract_first_url(s: &str) -> Option<String> {
    s.split_whitespace()
        .find(|t| t.starts_with("http://") || t.starts_with("https://"))
        .map(|t| t.trim_end_matches(['.', ',', ')', ']']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh_store() -> Arc<LeadStore> {
        Arc::new(
            LeadStore::open(
                std::path::PathBuf::from(":memory:"),
                TenantId::new("acme").unwrap(),
            )
            .await
            .unwrap(),
        )
    }

    fn args(t: &str, body: &str) -> Value {
        serde_json::json!({
            "tenant_id": t,
            "body": body,
            "history": [],
        })
    }

    #[tokio::test]
    async fn english_affirmation_with_day_hits() {
        let s = fresh_store().await;
        let r = handle(
            &TenantId::new("acme").unwrap(),
            s,
            None,
            None,
            None,
            None,
            args("acme", "Yes, Tuesday at 3pm works for me."),
        )
        .await
        .unwrap();
        let v = r.as_value();
        assert_eq!(v["ok"], true);
        assert_eq!(v["result"]["accepted"], true);
        assert!(v["result"]["confidence"].as_f64().unwrap() > 0.7);
    }

    #[tokio::test]
    async fn spanish_affirmation_with_hour_hits() {
        let s = fresh_store().await;
        let r = handle(
            &TenantId::new("acme").unwrap(),
            s,
            None,
            None,
            None,
            None,
            args("acme", "Perfecto, el martes a las 15:00 me sirve."),
        )
        .await
        .unwrap();
        let v = r.as_value();
        assert_eq!(v["result"]["accepted"], true);
    }

    #[tokio::test]
    async fn calendly_url_high_confidence() {
        let s = fresh_store().await;
        let r = handle(
            &TenantId::new("acme").unwrap(),
            s,
            None,
            None,
            None,
            None,
            args("acme", "Te paso link: https://calendly.com/luis/30min."),
        )
        .await
        .unwrap();
        let v = r.as_value();
        assert_eq!(v["result"]["accepted"], true);
        assert!(v["result"]["confidence"].as_f64().unwrap() >= 0.85);
        assert!(v["result"]["evidence"]
            .as_str()
            .unwrap()
            .contains("calendly.com"));
    }

    #[tokio::test]
    async fn ambiguous_text_returns_zero_confidence() {
        let s = fresh_store().await;
        let r = handle(
            &TenantId::new("acme").unwrap(),
            s,
            None,
            None,
            None,
            None,
            args("acme", "Gracias por la información, lo voy a revisar."),
        )
        .await
        .unwrap();
        let v = r.as_value();
        assert_eq!(v["result"]["accepted"], false);
        assert_eq!(v["result"]["confidence"], 0.0);
    }

    #[tokio::test]
    async fn tenant_mismatch_unauthorised() {
        let s = fresh_store().await;
        let r = handle(
            &TenantId::new("acme").unwrap(),
            s,
            None,
            None,
            None,
            None,
            args("globex", "Yes Tuesday"),
        )
        .await
        .unwrap();
        assert_eq!(r.as_value()["ok"], false);
    }
}
