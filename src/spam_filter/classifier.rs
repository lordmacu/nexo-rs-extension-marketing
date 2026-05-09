//! Rule-aware promo / spam classifier.
//!
//! Pure-fn over `(raw_bytes, subject, from_email, &ResolvedRules)`.
//! Caller (broker hop) supplies the already-decoded raw bytes
//! plus the cached `ResolvedRules` for the tenant; the classifier
//! does its own MIME walk for the body parts (image counts +
//! visible text) but does not touch SQLite.
//!
//! Decision order (every step short-circuits the next):
//! 1. Allow rules — sender / domain / keyword whitelist
//! 2. Block rules — sender / domain blacklist
//! 3. Threshold-based signal classifier:
//!    - image-only body
//!    - image-heavy / low-text
//!    - role sender + ≥1 promo keyword
//!    - N+ independent weak signals
//!
//! All bias here points the same way: never silently drop a
//! human message. Operators add allow rules to rescue false
//! positives; the defaults are tuned conservatively.

use mail_parser::{MessageParser, PartType};
use serde::{Deserialize, Serialize};

use super::rules::ResolvedRules;

/// Outcome the broker hop branches on. `Human` ⇒ continue with
/// lead pipeline; `Promo` ⇒ drop + audit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromoClassification {
    pub signals: PromoSignals,
    pub verdict: PromoVerdict,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "reason", rename_all = "snake_case")]
pub enum PromoVerdict {
    Human,
    Promo(BlockReason),
}

/// Why the classifier dropped a message.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BlockReason {
    /// Tenant-supplied `DomainBlock` rule matched the sender's
    /// domain.
    DomainBlocklist,
    /// Tenant-supplied `SenderBlock` rule matched the full
    /// sender address.
    SenderBlocklist,
    /// `>= 1` image and zero visible text.
    ImageOnlyBody,
    /// HTML body has many images and very little visible text
    /// (newsletter / catalog shape).
    ImageHeavyLowText,
    /// Sender's local-part is a no-reply / marketing role
    /// address AND at least one promo keyword hit.
    NoReplySenderWithKeyword,
    /// Two or more weak signals fired together.
    MultipleWeakSignals,
}

impl BlockReason {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::DomainBlocklist => "domain_blocklist",
            Self::SenderBlocklist => "sender_blocklist",
            Self::ImageOnlyBody => "image_only",
            Self::ImageHeavyLowText => "image_heavy_low_text",
            Self::NoReplySenderWithKeyword => "noreply_with_keyword",
            Self::MultipleWeakSignals => "multi_weak_signals",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromoSignals {
    pub image_count: u32,
    pub visible_text_chars: u32,
    pub html_bytes: u32,
    pub keyword_hits: u32,
    /// Subset of `keywords` that actually fired — surfaced for
    /// the dry-run / test endpoint so the operator can see why.
    pub matched_keywords: Vec<String>,
    pub sender_role: bool,
    /// Tenant-allow rule short-circuited the entire pipeline.
    /// `Some(reason)` ⇒ verdict is forced to `Human` regardless
    /// of every other signal.
    pub allow_match: Option<AllowMatch>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AllowMatch {
    Domain,
    Sender,
}

/// Run the full classifier against the raw bytes + decoded
/// `subject` / `from_email` + the resolved per-tenant rules.
pub fn classify_with_rules(
    raw_bytes: &[u8],
    subject: &str,
    from_email: &str,
    rules: &ResolvedRules,
) -> PromoClassification {
    let from_lower = from_email.to_ascii_lowercase();
    let domain = from_lower.split_once('@').map(|(_, d)| d.to_string());

    // ── Allow rules — short-circuit to Human ──────────────────
    if let Some(d) = &domain {
        if rules.allow_domains.contains(d) {
            return PromoClassification {
                signals: PromoSignals {
                    allow_match: Some(AllowMatch::Domain),
                    ..Default::default()
                },
                verdict: PromoVerdict::Human,
            };
        }
    }
    if rules.allow_senders.contains(&from_lower) {
        return PromoClassification {
            signals: PromoSignals {
                allow_match: Some(AllowMatch::Sender),
                ..Default::default()
            },
            verdict: PromoVerdict::Human,
        };
    }

    // ── Block rules — short-circuit to Promo ──────────────────
    if rules.block_senders.contains(&from_lower) {
        return PromoClassification {
            signals: PromoSignals::default(),
            verdict: PromoVerdict::Promo(BlockReason::SenderBlocklist),
        };
    }
    if let Some(d) = &domain {
        if rules.block_domains.contains(d) {
            return PromoClassification {
                signals: PromoSignals::default(),
                verdict: PromoVerdict::Promo(BlockReason::DomainBlocklist),
            };
        }
    }

    // ── Signal extraction + threshold-based decision ──────────
    let signals = extract_signals(raw_bytes, subject, &from_lower, rules);
    let verdict = decide(&signals, rules);
    PromoClassification { signals, verdict }
}

fn extract_signals(
    raw_bytes: &[u8],
    subject: &str,
    from_lower: &str,
    rules: &ResolvedRules,
) -> PromoSignals {
    let parsed = MessageParser::default().parse(raw_bytes);
    let mut html_body: Option<String> = None;
    let mut text_body: Option<String> = None;
    if let Some(msg) = &parsed {
        for part in msg.parts.iter() {
            if let PartType::Html(h) = &part.body {
                if html_body.is_none() {
                    html_body = Some(h.to_string());
                }
            } else if let PartType::Text(t) = &part.body {
                if text_body.is_none() {
                    text_body = Some(t.to_string());
                }
            }
        }
    }

    let html = html_body.unwrap_or_default();
    let html_bytes = html.len() as u32;
    let image_count = count_img_tags(&html);
    let visible_source = if let Some(t) = text_body.as_ref() {
        t.clone()
    } else {
        strip_html_lite(&html)
    };
    let visible_collapsed = collapse_whitespace(&visible_source);
    let visible_text_chars = visible_collapsed.chars().count() as u32;

    let mut haystack = String::with_capacity(subject.len() + visible_collapsed.len() + 1);
    haystack.push_str(&subject.to_lowercase());
    haystack.push(' ');
    haystack.push_str(&visible_collapsed.to_lowercase());

    let mut matched_keywords = Vec::new();
    let mut keyword_hits = 0u32;
    for kw in &rules.keywords {
        if haystack.contains(kw.as_str()) {
            keyword_hits += 1;
            if matched_keywords.len() < 8 {
                matched_keywords.push(kw.clone());
            }
        }
    }

    let sender_role = sender_local_is_role(from_lower, &rules.role_local_parts);

    PromoSignals {
        image_count,
        visible_text_chars,
        html_bytes,
        keyword_hits,
        matched_keywords,
        sender_role,
        allow_match: None,
    }
}

fn decide(s: &PromoSignals, rules: &ResolvedRules) -> PromoVerdict {
    let t = &rules.thresholds;

    if t.image_only_drop && s.image_count >= 1 && s.visible_text_chars == 0 {
        return PromoVerdict::Promo(BlockReason::ImageOnlyBody);
    }

    if t.image_heavy_drop
        && s.image_count >= t.image_heavy_min_count
        && s.visible_text_chars < t.image_heavy_max_text_chars
    {
        return PromoVerdict::Promo(BlockReason::ImageHeavyLowText);
    }

    if t.role_keyword_drop && s.sender_role && s.keyword_hits >= 1 {
        return PromoVerdict::Promo(BlockReason::NoReplySenderWithKeyword);
    }

    if t.multi_weak_drop {
        let mut weak = 0u32;
        if s.keyword_hits >= 2 {
            weak += 1;
        }
        if s.image_count >= 2 && s.visible_text_chars < 500 {
            weak += 1;
        }
        if s.sender_role {
            weak += 1;
        }
        if weak >= t.multi_weak_threshold {
            return PromoVerdict::Promo(BlockReason::MultipleWeakSignals);
        }
    }

    PromoVerdict::Human
}

// ── Pure helpers ─────────────────────────────────────────────────

pub(crate) fn count_img_tags(html: &str) -> u32 {
    let lower = html.to_ascii_lowercase();
    let mut count = 0u32;
    for needle in ["<img ", "<img\t", "<img\n", "<img>", "<img/"] {
        count += lower.matches(needle).count() as u32;
    }
    count
}

pub(crate) fn strip_html_lite(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let cleaned = drop_block(&lower, "<style", "</style>");
    let cleaned = drop_block(&cleaned, "<script", "</script>");
    let cleaned = drop_block(&cleaned, "<head", "</head>");
    let mut out = String::with_capacity(cleaned.len());
    let mut in_tag = false;
    for ch in cleaned.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn drop_block(input: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        if let Some(rel_end) = rest[start..].find(close) {
            let after = start + rel_end + close.len();
            rest = &rest[after..];
        } else {
            return out;
        }
    }
    out.push_str(rest);
    out
}

pub(crate) fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = true;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

fn sender_local_is_role(from_lower: &str, role_parts: &[String]) -> bool {
    let local = match from_lower.split_once('@') {
        Some((l, _)) => l,
        None => return false,
    };
    role_parts.iter().any(|role| {
        local == role.as_str()
            || local.starts_with(&format!("{role}-"))
            || local.starts_with(&format!("{role}."))
            || local.starts_with(&format!("{role}+"))
            || local.starts_with(&format!("{role}_"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spam_filter::defaults::Strictness;
    use crate::spam_filter::store::{RuleKind, SpamFilterRule};

    fn raw_with_html(html: &str) -> Vec<u8> {
        let mut s = String::new();
        s.push_str("From: alice@example.com\r\n");
        s.push_str("To: ops@x.com\r\n");
        s.push_str("Subject: hi\r\n");
        s.push_str("MIME-Version: 1.0\r\n");
        s.push_str("Content-Type: text/html; charset=utf-8\r\n");
        s.push_str("\r\n");
        s.push_str(html);
        s.into_bytes()
    }

    fn raw_text(text: &str) -> Vec<u8> {
        let mut s = String::new();
        s.push_str("From: alice@example.com\r\n");
        s.push_str("To: ops@x.com\r\n");
        s.push_str("Subject: hi\r\n");
        s.push_str("\r\n");
        s.push_str(text);
        s.into_bytes()
    }

    fn rule(kind: RuleKind, value: &str) -> SpamFilterRule {
        SpamFilterRule {
            id: 1,
            tenant_id: "t".into(),
            kind,
            value: value.into(),
            note: None,
            enabled: true,
            created_at_ms: 0,
        }
    }

    #[test]
    fn human_short_text_passes() {
        let raw = raw_text("Hola, quiero comprar tu producto.");
        let r = ResolvedRules::defaults_only();
        let c = classify_with_rules(&raw, "Quiero comprar", "alice@example.com", &r);
        assert_eq!(c.verdict, PromoVerdict::Human);
    }

    #[test]
    fn image_only_drops_default() {
        let raw = raw_with_html(r#"<img src="banner.jpg"/>"#);
        let r = ResolvedRules::defaults_only();
        let c = classify_with_rules(&raw, "Promo", "alice@example.com", &r);
        assert_eq!(c.verdict, PromoVerdict::Promo(BlockReason::ImageOnlyBody));
    }

    #[test]
    fn image_only_passes_when_threshold_off() {
        let raw = raw_with_html(r#"<img src="banner.jpg"/>"#);
        let mut r = ResolvedRules::defaults_only();
        r.thresholds.image_only_drop = false;
        // Other signals still off → Human.
        let c = classify_with_rules(&raw, "Subj", "alice@example.com", &r);
        assert_eq!(c.verdict, PromoVerdict::Human);
    }

    #[test]
    fn domain_block_short_circuits() {
        let raw = raw_text("anything");
        let rules = vec![rule(RuleKind::DomainBlock, "example.com")];
        let r = ResolvedRules::from_persisted(
            Strictness::Balanced,
            crate::spam_filter::defaults::BALANCED_THRESHOLDS.clone(),
            &rules,
        );
        let c = classify_with_rules(&raw, "Hi", "alice@example.com", &r);
        assert_eq!(c.verdict, PromoVerdict::Promo(BlockReason::DomainBlocklist));
    }

    #[test]
    fn sender_allow_overrides_image_only() {
        // VIP sender who happens to send a banner-only confirmation:
        // allow rule must keep them through.
        let raw = raw_with_html(r#"<img src="logo.png"/>"#);
        let rules = vec![rule(RuleKind::SenderAllow, "alice@example.com")];
        let r = ResolvedRules::from_persisted(
            Strictness::Strict,
            crate::spam_filter::defaults::STRICT_THRESHOLDS.clone(),
            &rules,
        );
        let c = classify_with_rules(&raw, "Reciept", "Alice@Example.com", &r);
        assert_eq!(c.verdict, PromoVerdict::Human);
        assert_eq!(c.signals.allow_match, Some(AllowMatch::Sender));
    }

    #[test]
    fn keyword_allow_suppresses_default_hit() {
        // Default catalog includes "click here". Without an
        // allow rule, role sender + this keyword would drop.
        let raw = raw_with_html(
            "<p>Su pedido fue enviado. Haga click here para verlo.</p>",
        );
        let rules = vec![rule(RuleKind::KeywordAllow, "click here")];
        let r = ResolvedRules::from_persisted(
            Strictness::Balanced,
            crate::spam_filter::defaults::BALANCED_THRESHOLDS.clone(),
            &rules,
        );
        let c = classify_with_rules(&raw, "Confirmación", "notifications@shop.com", &r);
        // Role sender, but keyword was suppressed → only weak
        // signal is sender_role (1 weak) → not enough to drop.
        assert_eq!(c.verdict, PromoVerdict::Human);
    }

    #[test]
    fn tenant_keyword_block_extends() {
        let raw = raw_text("Hola, tenemos un super bono casino para usted.");
        let rules = vec![rule(RuleKind::KeywordBlock, "bono casino")];
        let r = ResolvedRules::from_persisted(
            Strictness::Balanced,
            crate::spam_filter::defaults::BALANCED_THRESHOLDS.clone(),
            &rules,
        );
        let c = classify_with_rules(&raw, "Promo", "alice@example.com", &r);
        // 1 keyword + non-role sender → 1 weak signal ⇒ Human.
        assert_eq!(c.verdict, PromoVerdict::Human);
        assert!(c.signals.matched_keywords.iter().any(|k| k == "bono casino"));
    }

    #[test]
    fn strict_preset_drops_image_heavy_2() {
        // 2 images + ~140 chars of plain copy that has zero
        // promo keywords — Balanced (image_heavy_min=3) keeps
        // it because no other signal fires; Strict
        // (image_heavy_min=2, max_text=500) drops as
        // ImageHeavyLowText.
        let raw = raw_with_html(
            r#"<img src="a"/><img src="b"/><p>Hola, te comparto el documento adjunto que solicitaste. Quedo atento a tus comentarios cuando lo revises.</p>"#,
        );
        let r_balanced = ResolvedRules::defaults_only();
        let c_b = classify_with_rules(&raw, "Documento", "alice@example.com", &r_balanced);
        assert_eq!(c_b.verdict, PromoVerdict::Human);

        let r_strict = ResolvedRules::from_persisted(
            Strictness::Strict,
            crate::spam_filter::defaults::STRICT_THRESHOLDS.clone(),
            &[],
        );
        let c_s = classify_with_rules(&raw, "Documento", "alice@example.com", &r_strict);
        // image_heavy threshold ≥2 imgs <500 chars under strict.
        assert_eq!(
            c_s.verdict,
            PromoVerdict::Promo(BlockReason::ImageHeavyLowText)
        );
    }

    #[test]
    fn allow_domain_overrides_block_sender() {
        // Both rules present — allow has higher priority.
        let raw = raw_text("Hi");
        let rules = vec![
            rule(RuleKind::SenderBlock, "alice@example.com"),
            rule(RuleKind::DomainAllow, "example.com"),
        ];
        let r = ResolvedRules::from_persisted(
            Strictness::Balanced,
            crate::spam_filter::defaults::BALANCED_THRESHOLDS.clone(),
            &rules,
        );
        let c = classify_with_rules(&raw, "Hi", "alice@example.com", &r);
        assert_eq!(c.verdict, PromoVerdict::Human);
    }

    #[test]
    fn role_sender_alone_does_not_drop() {
        let raw = raw_text("Su pedido ha sido enviado. Gracias.");
        let r = ResolvedRules::defaults_only();
        let c = classify_with_rules(&raw, "Confirmación", "notifications@shop.com", &r);
        assert!(c.signals.sender_role);
        assert_eq!(c.verdict, PromoVerdict::Human);
    }

    #[test]
    fn info_local_part_is_not_role() {
        let raw = raw_text("Hello");
        let r = ResolvedRules::defaults_only();
        let c = classify_with_rules(&raw, "x", "info@miempresa.com", &r);
        assert!(!c.signals.sender_role);
    }

    #[test]
    fn img_tag_self_closing_counts() {
        let raw = raw_with_html(r#"<img src="a"/><img src="b"/>"#);
        let r = ResolvedRules::defaults_only();
        let c = classify_with_rules(&raw, "x", "alice@example.com", &r);
        assert_eq!(c.signals.image_count, 2);
    }

    #[test]
    fn style_block_does_not_inflate_visible_text() {
        let raw = raw_with_html(
            r#"<html><head><style>.x { color: red; padding: 100px; font-family: sans-serif; }</style></head><body><img src="a"/></body></html>"#,
        );
        let r = ResolvedRules::defaults_only();
        let c = classify_with_rules(&raw, "x", "alice@example.com", &r);
        assert_eq!(c.verdict, PromoVerdict::Promo(BlockReason::ImageOnlyBody));
    }
}
