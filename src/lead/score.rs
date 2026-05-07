//! Heuristic lead scorer (M15.23.f v1).
//!
//! Composes signals into a 0..=100 score with an explanation
//! trace ("+15 corporate domain, +10 replied within 1h").
//! LLM-backed scoring lives in M18 (intelligence module);
//! this is the deterministic baseline operators get for free.

use nexo_microapp_sdk::enrichment::{classify, DomainKind};
use nexo_tool_meta::marketing::{IntentClass, SentimentBand};

/// Inputs the scorer reads. All fields optional so callers
/// fill what they have at the moment the lead transitions
/// (some signals only appear on the second message, etc.).
#[derive(Debug, Clone, Default)]
pub struct ScoreInputs<'a> {
    pub from_email: Option<&'a str>,
    pub intent: Option<IntentClass>,
    pub sentiment: Option<SentimentBand>,
    pub seniority: Option<&'a str>,
    /// Time between the latest inbound and the agent / vendedor
    /// reply. `None` when the thread doesn't have a reply yet.
    pub reply_latency_ms: Option<u64>,
    /// `true` when the resolver has merged this person across
    /// 2+ prior threads (recurring contact).
    pub recurring_contact: bool,
    /// `true` when the operator manually marked the person VIP.
    pub vip: bool,
}

/// Per-signal contribution carried back to the operator UI's
/// "why this lead?" panel + the lead audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreContribution {
    pub signal: String,
    pub delta: i16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreOutput {
    pub score: u8,
    pub trace: Vec<ScoreContribution>,
}

/// Pure function. Sums signal contributions, clamps to 0..=100,
/// returns the score + trace.
pub fn score_lead(input: &ScoreInputs<'_>) -> ScoreOutput {
    let mut trace: Vec<ScoreContribution> = Vec::new();
    let mut sum: i32 = 30; // baseline so a corporate cold lead lands at ~45-55

    // Domain kind.
    if let Some(email) = input.from_email {
        match classify(email) {
            DomainKind::Corporate => trace.push(ScoreContribution {
                signal: "corporate domain".into(),
                delta: 15,
            }),
            DomainKind::Personal => trace.push(ScoreContribution {
                signal: "personal domain".into(),
                delta: 0,
            }),
            DomainKind::Disposable => trace.push(ScoreContribution {
                signal: "disposable domain".into(),
                delta: -25,
            }),
        }
    }

    // Intent.
    if let Some(intent) = input.intent {
        let (label, delta) = match intent {
            IntentClass::ReadyToBuy => ("intent ready_to_buy", 25),
            IntentClass::Comparing => ("intent comparing", 12),
            IntentClass::Browsing => ("intent browsing", 5),
            IntentClass::SupportRequest => ("intent support_request", 0),
            IntentClass::Objecting => ("intent objecting", -5),
            IntentClass::OutOfScope => ("intent out_of_scope", -20),
        };
        trace.push(ScoreContribution {
            signal: label.into(),
            delta,
        });
    }

    // Sentiment.
    if let Some(s) = input.sentiment {
        let (label, delta) = match s {
            SentimentBand::VeryPositive => ("sentiment very_positive", 8),
            SentimentBand::Positive => ("sentiment positive", 4),
            SentimentBand::Neutral => ("sentiment neutral", 0),
            SentimentBand::Negative => ("sentiment negative", -4),
            SentimentBand::VeryNegative => ("sentiment very_negative", -8),
        };
        trace.push(ScoreContribution {
            signal: label.into(),
            delta,
        });
    }

    // Seniority — buyer authority signal.
    if let Some(level) = input.seniority {
        let delta = match level.to_ascii_lowercase().as_str() {
            "c-level" => 15,
            "vp" => 10,
            "director" => 7,
            "manager" => 3,
            _ => 0,
        };
        if delta != 0 {
            trace.push(ScoreContribution {
                signal: format!("seniority {level}"),
                delta,
            });
        }
    }

    // Reply latency — fast replies = warm lead.
    if let Some(ms) = input.reply_latency_ms {
        let (label, delta) = if ms < 60 * 60 * 1000 {
            ("replied within 1h", 10)
        } else if ms < 4 * 60 * 60 * 1000 {
            ("replied within 4h", 5)
        } else if ms > 48 * 60 * 60 * 1000 {
            ("replied after 48h", -5)
        } else {
            ("normal reply latency", 0)
        };
        if delta != 0 {
            trace.push(ScoreContribution {
                signal: label.into(),
                delta,
            });
        }
    }

    if input.recurring_contact {
        trace.push(ScoreContribution {
            signal: "recurring contact (2+ threads)".into(),
            delta: 8,
        });
    }
    if input.vip {
        trace.push(ScoreContribution {
            signal: "tagged VIP".into(),
            delta: 20,
        });
    }

    for c in &trace {
        sum += c.delta as i32;
    }
    let clamped = sum.clamp(0, 100) as u8;

    ScoreOutput {
        score: clamped,
        trace,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent_set<'a>(email: &'a str, intent: IntentClass) -> ScoreInputs<'a> {
        ScoreInputs {
            from_email: Some(email),
            intent: Some(intent),
            ..Default::default()
        }
    }

    #[test]
    fn baseline_corporate_unknown_intent_around_mid() {
        let s = score_lead(&ScoreInputs {
            from_email: Some("juan@acme.com"),
            ..Default::default()
        });
        assert!(s.score >= 40 && s.score <= 55, "got {}", s.score);
    }

    #[test]
    fn ready_to_buy_corporate_high_score() {
        let s = score_lead(&intent_set("juan@acme.com", IntentClass::ReadyToBuy));
        // 30 baseline + 15 corp + 25 intent = 70.
        assert_eq!(s.score, 70);
    }

    #[test]
    fn disposable_caps_low() {
        let s = score_lead(&intent_set(
            "spam@mailinator.com",
            IntentClass::ReadyToBuy,
        ));
        // 30 baseline + (-25 disposable) + 25 intent = 30.
        assert_eq!(s.score, 30);
    }

    #[test]
    fn vip_tag_pushes_above_baseline() {
        let mut input = ScoreInputs::default();
        input.from_email = Some("anna@gmail.com");
        input.vip = true;
        let s = score_lead(&input);
        // 30 baseline + 0 personal + 20 VIP = 50.
        assert_eq!(s.score, 50);
    }

    #[test]
    fn fast_reply_adds_signal() {
        let s = score_lead(&ScoreInputs {
            from_email: Some("juan@acme.com"),
            reply_latency_ms: Some(30 * 60 * 1000),
            ..Default::default()
        });
        // 30 + 15 corp + 10 replied-within-1h = 55.
        assert_eq!(s.score, 55);
    }

    #[test]
    fn slow_reply_penalised() {
        let s = score_lead(&ScoreInputs {
            from_email: Some("juan@acme.com"),
            reply_latency_ms: Some(72 * 60 * 60 * 1000),
            ..Default::default()
        });
        // 30 + 15 corp - 5 slow = 40.
        assert_eq!(s.score, 40);
    }

    #[test]
    fn c_level_seniority_boosts() {
        let s = score_lead(&ScoreInputs {
            from_email: Some("ceo@acme.com"),
            seniority: Some("C-level"),
            ..Default::default()
        });
        // 30 + 15 corp + 15 c-level = 60.
        assert_eq!(s.score, 60);
    }

    #[test]
    fn maxed_signals_cap_at_100() {
        let s = score_lead(&ScoreInputs {
            from_email: Some("ceo@acme.com"),
            intent: Some(IntentClass::ReadyToBuy),
            sentiment: Some(SentimentBand::VeryPositive),
            seniority: Some("C-level"),
            reply_latency_ms: Some(10 * 60 * 1000),
            recurring_contact: true,
            vip: true,
        });
        // Sum = 30 + 15 + 25 + 8 + 15 + 10 + 8 + 20 = 131,
        // clamped to 100.
        assert_eq!(s.score, 100);
    }

    #[test]
    fn negative_signals_floor_at_zero() {
        let s = score_lead(&ScoreInputs {
            from_email: Some("spam@mailinator.com"),
            intent: Some(IntentClass::OutOfScope),
            sentiment: Some(SentimentBand::VeryNegative),
            ..Default::default()
        });
        // 30 - 25 - 20 - 8 = -23 → clamped to 0.
        assert_eq!(s.score, 0);
    }

    #[test]
    fn trace_explains_each_signal() {
        let s = score_lead(&ScoreInputs {
            from_email: Some("juan@acme.com"),
            intent: Some(IntentClass::ReadyToBuy),
            ..Default::default()
        });
        let labels: Vec<&str> = s.trace.iter().map(|c| c.signal.as_str()).collect();
        assert!(labels.contains(&"corporate domain"));
        assert!(labels.contains(&"intent ready_to_buy"));
    }
}
