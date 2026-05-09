//! Unsubscribe / opt-out detector (audit fix #5).
//!
//! Scans inbound subject + body for high-confidence opt-out
//! signals so a customer saying "please stop emailing me" is
//! handled by the system instead of looped through cron-driven
//! followups. Conservative keyword set on purpose — the
//! consequence of a false positive is silently dropping a
//! real lead, so every entry must imply unambiguous intent to
//! stop being contacted.
//!
//! When `detect_unsubscribe` matches, the broker hop (or any
//! other caller) MUST:
//!   1. Transition the lead to `LeadState::Lost` (auto-clears
//!      `next_check_at_ms` and rejects pending drafts via the
//!      terminal-transition handler).
//!   2. Add the sender's email to the per-tenant
//!      `spam_filter_rules` as a `SenderBlock` so future
//!      messages from the same address drop pre-pipeline.
//!   3. Record an `AuditEvent::UnsubscribeDetected` row.
//!
//! The detector is a pure fn so the broker hop, tests, and any
//! future operator dry-run UI share one code path.

/// High-confidence unsubscribe vocabulary (Spanish + English).
/// Lowercase substring match against the concatenated subject
/// + body haystack. Every entry must be a phrase that on its
/// own implies "stop contacting me" — bare "stop" / "no" /
/// "remove" are too generic and stay out.
const UNSUBSCRIBE_KEYWORDS: &[&str] = &[
    // ── English (very high confidence) ──────────────────────
    "unsubscribe",
    "opt out",
    "opt-out",
    "stop emailing me",
    "stop sending me",
    "stop contacting me",
    "remove me from your list",
    "remove me from this list",
    "take me off your list",
    "do not contact me",
    "do not email me",
    // ── Spanish (very high confidence) ──────────────────────
    "darse de baja",
    "dame de baja",
    "dame baja",
    "deseo darme de baja",
    "quiero darme de baja",
    "no me envíen más",
    "no me envien mas",
    "no me envíes más",
    "no me envies mas",
    "deja de enviarme",
    "dejen de enviarme",
    "no enviarme más",
    "no enviarme mas",
    "quitar de tu lista",
    "quitame de tu lista",
    "no quiero más correos",
    "no quiero mas correos",
    "no quiero recibir más",
    "no quiero recibir mas",
    "favor no me contacten",
    "no me contacten",
    "elimíname de tu lista",
    "elimíname de la lista",
    "eliminame de tu lista",
];

/// Returns `Some(matched_keyword)` when the detector finds an
/// opt-out signal; `None` otherwise. Caller logs the matched
/// keyword in the audit row + the tracing line so an operator
/// reviewing why a sender was auto-blocked sees exactly which
/// phrase fired.
pub fn detect_unsubscribe(subject: &str, body_excerpt: &str) -> Option<&'static str> {
    let mut haystack = String::with_capacity(subject.len() + body_excerpt.len() + 1);
    haystack.push_str(&subject.to_lowercase());
    haystack.push(' ');
    haystack.push_str(&body_excerpt.to_lowercase());
    UNSUBSCRIBE_KEYWORDS
        .iter()
        .find(|kw| haystack.contains(*kw))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_unsubscribe_subject_matches() {
        let m = detect_unsubscribe("UNSUBSCRIBE", "");
        assert_eq!(m, Some("unsubscribe"));
    }

    #[test]
    fn english_stop_emailing_body_matches() {
        let m = detect_unsubscribe(
            "Re: Your offer",
            "Thanks but please stop emailing me about this.",
        );
        assert_eq!(m, Some("stop emailing me"));
    }

    #[test]
    fn spanish_darse_de_baja_matches() {
        let m = detect_unsubscribe(
            "Re: Cotización",
            "Por favor, deseo darme de baja de su lista.",
        );
        assert!(m.is_some());
        assert!(["darse de baja", "darme de baja", "deseo darme de baja"]
            .contains(&m.unwrap()));
    }

    #[test]
    fn spanish_no_me_envien_mas_matches() {
        let m = detect_unsubscribe(
            "Re: Producto",
            "Por favor no me envíen más correos.",
        );
        assert!(m.is_some());
    }

    #[test]
    fn polite_thanks_does_not_match() {
        // Customer politely declines but does NOT request to be
        // removed — must stay open for a future re-engagement.
        let m = detect_unsubscribe(
            "Re: Cotización",
            "Gracias por la información. Por ahora vamos a evaluar otras opciones.",
        );
        assert_eq!(m, None);
    }

    #[test]
    fn bare_stop_word_does_not_match() {
        // Generic "stop" alone is too ambiguous — could be
        // "stop the demo here" or any sentence fragment.
        let m = detect_unsubscribe(
            "Re:",
            "Wait, stop. Let me think about this.",
        );
        assert_eq!(m, None);
    }

    #[test]
    fn case_insensitive() {
        let m = detect_unsubscribe(
            "RE: BUY NOW",
            "Please STOP EMAILING ME, I am not interested.",
        );
        assert_eq!(m, Some("stop emailing me"));
    }

    #[test]
    fn matches_in_subject_only() {
        let m = detect_unsubscribe("unsubscribe please", "neutral body");
        assert_eq!(m, Some("unsubscribe"));
    }

    #[test]
    fn empty_inputs_no_match() {
        let m = detect_unsubscribe("", "");
        assert_eq!(m, None);
    }

    #[test]
    fn ambiguous_no_match() {
        // "remove" alone — too generic ("remove the line item",
        // "remove me from the meeting", etc).
        let m = detect_unsubscribe("Re:", "Please remove the typo on page 2.");
        assert_eq!(m, None);
    }
}
