//! Marketing-specific scorer composition (M15.23.f).
//!
//! **F29 sweep:** marketing-specific by design. Generic
//! `Scorer` / `HeuristicScorer` / `Score` types live in
//! `nexo_microapp_sdk::scoring`; this module composes the
//! 5 marketing-default rules + the `LeadCtx` projection.
//!
//! Wraps the SDK's generic `HeuristicScorer` with the lead-
//! shaped rules the marketing extension cares about today.
//! Pure logic + no async — the broker hop runs the scorer
//! synchronously inside its lead-create branch.
//!
//! ## Inputs
//!
//! [`LeadCtx`] is the minimal projection of the data the
//! broker hop has at create time: domain kind, body length,
//! signature role hint (when the resolver inferred one), and
//! the operator-supplied display name. Reply latency +
//! intent label aren't available yet on first inbound (no
//! prior outbound to compare against, no LLM intent stage
//! wired) — those rules light up later when the M22 draft
//! pipeline + M18 intent classifier land.
//!
//! ## Why heuristic v1
//!
//! Five rules cover the obvious operator intuition:
//!
//! - Corporate domain ⇒ +20 (most leads worth auto-routing)
//! - Personal domain ⇒ -5 (lower priority, but not blocked)
//! - Substantive body (≥ 30 words) ⇒ +10
//! - Brief body (< 5 words) ⇒ -10 (likely auto-reply)
//! - Sender display name carries a senior role hint ⇒ +15

use nexo_microapp_sdk::enrichment::DomainKind;
use nexo_microapp_sdk::scoring::{HeuristicRule, HeuristicScorer, Score, Scorer};

use crate::broker::inbound::ParsedInbound;

/// Lead-shaped scorer input. Cheap to build off a
/// [`ParsedInbound`] (see [`LeadCtx::from_parsed`]).
#[derive(Debug, Clone)]
pub struct LeadCtx {
    /// Domain classifier verdict (`Corporate` / `Personal` /
    /// `Disposable` — `Disposable` short-circuits before
    /// scoring lands so it never gets here).
    pub domain_kind: DomainKind,
    /// Word count over the body excerpt (whitespace split).
    pub body_words: u32,
    /// Lower-cased sender display name when present. Used by
    /// the senior-signature rule.
    pub display_name_lower: Option<String>,
}

impl LeadCtx {
    /// Build from the parsed inbound. Pure projection — no
    /// I/O.
    pub fn from_parsed(parsed: &ParsedInbound) -> Self {
        let body_words = parsed.body_excerpt.split_whitespace().count() as u32;
        let display_name_lower = parsed
            .from_display_name
            .as_ref()
            .map(|s| s.to_lowercase());
        Self {
            domain_kind: parsed.from_domain_kind,
            body_words,
            display_name_lower,
        }
    }
}

/// Build the marketing-default heuristic scorer. Five rules,
/// composable via [`Scorer`] dyn dispatch with future scorers
/// (sentiment, intent, BANT) when those land.
pub fn build_marketing_scorer() -> HeuristicScorer<LeadCtx> {
    let mut s = HeuristicScorer::new();

    s.push(HeuristicRule::with_detail(
        "corporate_domain",
        20,
        "remitente desde dominio corporativo",
        |c: &LeadCtx| c.domain_kind == DomainKind::Corporate,
    ));

    s.push(HeuristicRule::with_detail(
        "personal_domain",
        -5,
        "remitente desde dominio personal (gmail, hotmail, …)",
        |c: &LeadCtx| c.domain_kind == DomainKind::Personal,
    ));

    s.push(HeuristicRule::with_detail(
        "substantive_body",
        10,
        "cuerpo del mensaje ≥ 30 palabras",
        |c: &LeadCtx| c.body_words >= 30,
    ));

    s.push(HeuristicRule::with_detail(
        "brief_body",
        -10,
        "cuerpo del mensaje < 5 palabras (posible auto-reply)",
        |c: &LeadCtx| c.body_words < 5,
    ));

    s.push(HeuristicRule::with_detail(
        "senior_signature",
        15,
        "el display name menciona un rol senior",
        |c: &LeadCtx| {
            const SENIOR_TOKENS: &[&str] = &[
                "ceo",
                "cto",
                "cfo",
                "coo",
                "vp ",
                "vice president",
                "director",
                "head of",
                "founder",
                "co-founder",
                "fundador",
                "fundadora",
                "gerente general",
                "presidente",
            ];
            let Some(name) = c.display_name_lower.as_deref() else {
                return false;
            };
            SENIOR_TOKENS.iter().any(|tok| name.contains(tok))
        },
    ));

    s
}

/// Convenience wrapper: build a scorer + apply it. Tests
/// invoke this directly without juggling the scorer instance.
pub fn score_lead(parsed: &ParsedInbound) -> Score {
    build_marketing_scorer().score(&LeadCtx::from_parsed(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_with(domain: DomainKind, body: &str, display: Option<&str>) -> ParsedInbound {
        ParsedInbound {
            instance: "default".into(),
            account_id: "acct".into(),
            uid: 1,
            from_email: "x@y.com".into(),
            from_display_name: display.map(String::from),
            to_emails: vec![],
            reply_to: None,
            subject: "Cotización".into(),
            message_id: Some("m@x".into()),
            in_reply_to: None,
            references: vec![],
            body_excerpt: body.into(),
            thread_id: "th".into(),
            from_domain_kind: domain,
        }
    }

    #[test]
    fn corporate_substantive_senior_lands_high() {
        let body = "Lorem ipsum ".repeat(20); // > 30 words.
        let p = parsed_with(
            DomainKind::Corporate,
            body.trim(),
            Some("Juan Pérez (CTO)"),
        );
        let s = score_lead(&p);
        // 20 (corporate) + 10 (substantive) + 15 (senior) = 45.
        assert_eq!(s.value(), 45);
        let labels: Vec<&str> = s.reasons().iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"corporate_domain"));
        assert!(labels.contains(&"substantive_body"));
        assert!(labels.contains(&"senior_signature"));
    }

    #[test]
    fn personal_brief_lands_zero_after_saturation() {
        let p = parsed_with(DomainKind::Personal, "ok", None);
        let s = score_lead(&p);
        // -5 (personal) + -10 (brief) = -15 → clamped to 0.
        assert_eq!(s.value(), 0);
        // Reasons still attached for the audit trail.
        assert_eq!(s.reasons().len(), 2);
    }

    #[test]
    fn corporate_brief_lands_at_corporate_minus_brief() {
        let p = parsed_with(DomainKind::Corporate, "ok", None);
        let s = score_lead(&p);
        // 20 - 10 = 10.
        assert_eq!(s.value(), 10);
    }

    #[test]
    fn senior_token_is_case_insensitive() {
        let body = "Lorem ipsum ".repeat(20);
        let p1 = parsed_with(DomainKind::Corporate, body.trim(), Some("CEO Juan"));
        let p2 = parsed_with(DomainKind::Corporate, body.trim(), Some("ceo juan"));
        let p3 = parsed_with(DomainKind::Corporate, body.trim(), Some("CEO Juan"));
        assert_eq!(score_lead(&p1).value(), score_lead(&p2).value());
        assert_eq!(score_lead(&p1).value(), score_lead(&p3).value());
    }

    #[test]
    fn no_display_name_disables_senior_rule() {
        let body = "Lorem ipsum ".repeat(20);
        let p = parsed_with(DomainKind::Corporate, body.trim(), None);
        let s = score_lead(&p);
        // 20 + 10 = 30 (no senior boost).
        assert_eq!(s.value(), 30);
    }

    #[test]
    fn body_words_uses_whitespace_split() {
        // Exactly 30 words → trips the substantive threshold.
        let body = "word ".repeat(30);
        let p = parsed_with(DomainKind::Corporate, body.trim(), None);
        let s = score_lead(&p);
        assert_eq!(s.value(), 30);
        let labels: Vec<&str> = s.reasons().iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"substantive_body"));
    }

    #[test]
    fn build_returns_five_rules() {
        let s = build_marketing_scorer();
        assert_eq!(s.rule_count(), 5);
    }

    #[test]
    fn score_carries_operator_facing_detail() {
        let body = "Lorem ipsum ".repeat(20);
        let p = parsed_with(
            DomainKind::Corporate,
            body.trim(),
            Some("CEO Juan"),
        );
        let s = score_lead(&p);
        let detailed = s
            .reasons()
            .iter()
            .filter_map(|r| r.detail.as_deref())
            .collect::<Vec<_>>();
        assert!(detailed.iter().any(|d| d.contains("corporativo")));
    }
}
