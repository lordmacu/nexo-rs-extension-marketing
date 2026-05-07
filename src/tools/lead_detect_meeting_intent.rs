//! `marketing_lead_detect_meeting_intent` — dual-stage
//! classifier. Stage 1 regex catches obvious "yes Tuesday at
//! 3pm" / "sí, martes a las 15:00" / Calendly URL. Stage 2
//! LLM lands later (M18); this commit ships the regex stage
//! + a `Some(0.5)` confidence stub when text is ambiguous so
//! callers can defer to operator review.

use serde::Deserialize;
use serde_json::Value;

use nexo_microapp_sdk::{ToolError, ToolReply};
use nexo_tool_meta::marketing::{MeetingIntent, TenantIdRef};

use crate::tenant::TenantId;

#[derive(Debug, Deserialize)]
struct Args {
    tenant_id: TenantIdRef,
    body: String,
    /// Last 3-5 messages in the thread, oldest first. Used
    /// later for context-aware LLM stage.
    #[serde(default)]
    history: Vec<String>,
}

pub async fn handle(
    expected_tenant: &TenantId,
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

    fn args(t: &str, body: &str) -> Value {
        serde_json::json!({
            "tenant_id": t,
            "body": body,
            "history": [],
        })
    }

    #[tokio::test]
    async fn english_affirmation_with_day_hits() {
        let r = handle(
            &TenantId::new("acme").unwrap(),
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
        let r = handle(
            &TenantId::new("acme").unwrap(),
            args("acme", "Perfecto, el martes a las 15:00 me sirve."),
        )
        .await
        .unwrap();
        let v = r.as_value();
        assert_eq!(v["result"]["accepted"], true);
    }

    #[tokio::test]
    async fn calendly_url_high_confidence() {
        let r = handle(
            &TenantId::new("acme").unwrap(),
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
        let r = handle(
            &TenantId::new("acme").unwrap(),
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
        let r = handle(&TenantId::new("acme").unwrap(), args("globex", "Yes Tuesday"))
            .await
            .unwrap();
        assert_eq!(r.as_value()["ok"], false);
    }
}
