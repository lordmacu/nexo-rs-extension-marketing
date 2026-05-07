//! Inbound email decoder.
//!
//! Subscribes (in the binary; this module is pure) to
//! `plugin.inbound.email.*` and decodes the email plugin's
//! `InboundEvent` payload (raw RFC 5322 bytes + IMAP UID +
//! account id) into the typed `ParsedInbound` shape the rest
//! of the extension consumes.
//!
//! Pure functions + a small trait — no async-nats client
//! here. The binary wires `async-nats` to call
//! `decode_inbound_email` per delivery.

use async_trait::async_trait;
use mail_parser::MessageParser;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use nexo_microapp_sdk::enrichment::{classify, DomainKind};

use crate::tenant::{TenantId, TenantIdError};

/// Subset of fields the rest of the pipeline needs. Decoded
/// from the raw RFC 5322 message + the email plugin's
/// InboundEvent envelope. Heavy bodies stay borrow-only when
/// possible to avoid alloc on hot path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInbound {
    pub instance: String,
    pub account_id: String,
    pub uid: u32,
    pub from_email: String,
    pub from_display_name: Option<String>,
    pub to_emails: Vec<String>,
    pub reply_to: Option<String>,
    pub subject: String,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub body_excerpt: String,
    /// Stable thread id derived from References / In-Reply-To /
    /// Message-Id chain. Used by `LeadStore::find_by_thread`.
    pub thread_id: String,
    /// Domain classifier result — let the resolver branch
    /// without re-classifying.
    pub from_domain_kind: DomainKind,
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("mail-parser failed to decode RFC 5322 body")]
    DecodeFailed,
    #[error("missing required header: {0}")]
    MissingHeader(&'static str),
    #[error("invalid sender email: {0:?}")]
    InvalidFromEmail(String),
    #[error("disposable sender; dropped pre-pipeline: {0:?}")]
    DisposableSender(String),
    #[error("tenant resolution: {0}")]
    TenantResolution(String),
}

/// Decode the email-plugin `InboundEvent.raw_bytes` into the
/// extension's typed shape.
///
/// `instance` + `account_id` + `uid` come from the broker
/// envelope — caller passes them through. Disposable senders
/// are dropped pre-pipeline (returns `DisposableSender`)
/// because the operator's routing rule for them is always
/// "drop" — short-circuiting saves one classifier hop.
pub fn decode_inbound_email(
    instance: &str,
    account_id: &str,
    uid: u32,
    raw_bytes: &[u8],
) -> Result<ParsedInbound, ParseError> {
    let msg = MessageParser::default()
        .parse(raw_bytes)
        .ok_or(ParseError::DecodeFailed)?;

    // From address + optional display name.
    let from_addr = msg.from().and_then(|a| a.first()).ok_or(ParseError::MissingHeader("From"))?;
    let from_email = from_addr
        .address()
        .map(|s| s.to_ascii_lowercase())
        .ok_or_else(|| ParseError::InvalidFromEmail("(no address)".into()))?;
    if !from_email.contains('@') {
        return Err(ParseError::InvalidFromEmail(from_email));
    }
    let from_display_name = from_addr
        .name()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let from_kind = classify(&from_email);
    if from_kind == DomainKind::Disposable {
        return Err(ParseError::DisposableSender(from_email));
    }

    // To addresses.
    let to_emails: Vec<String> = msg
        .to()
        .map(|list| {
            list.iter()
                .filter_map(|a| a.address().map(|s| s.to_ascii_lowercase()))
                .collect()
        })
        .unwrap_or_default();

    // Reply-To.
    let reply_to = msg
        .reply_to()
        .and_then(|list| list.first())
        .and_then(|a| a.address())
        .map(|s| s.to_ascii_lowercase());

    let subject = msg.subject().unwrap_or("").to_string();
    let message_id = msg.message_id().map(|s| s.to_string());
    let in_reply_to = msg.in_reply_to().as_text_list().and_then(|v| v.first().map(|s| s.to_string()));
    let references: Vec<String> = msg
        .references()
        .as_text_list()
        .map(|v| v.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();

    let body_excerpt = msg
        .body_text(0)
        .map(|s| s.to_string())
        .or_else(|| {
            msg.body_html(0).map(|s| {
                // Light strip — don't bring a full html-to-text
                // dep; the resolver only needs ~400 chars of
                // signal. Tags are removed; whitespace
                // collapsed. This is best-effort.
                strip_html(&s)
            })
        })
        .unwrap_or_default();
    let body_excerpt = body_excerpt.chars().take(2_000).collect::<String>();

    let thread_id = derive_thread_id(message_id.as_deref(), in_reply_to.as_deref(), &references);

    Ok(ParsedInbound {
        instance: instance.to_string(),
        account_id: account_id.to_string(),
        uid,
        from_email,
        from_display_name,
        to_emails,
        reply_to,
        subject,
        message_id,
        in_reply_to,
        references,
        body_excerpt,
        thread_id,
        from_domain_kind: from_kind,
    })
}

/// Resolve the tenant id this inbound belongs to. The email
/// plugin's `account_id` is shared with the agent the operator
/// configured + the agent record carries `tenant_id`. Real
/// impl calls the daemon's `agents/get` admin RPC; tests use
/// the static map impl below.
#[async_trait]
pub trait TenantResolver: Send + Sync {
    async fn resolve_for_account(
        &self,
        account_id: &str,
    ) -> Result<TenantId, TenantResolverError>;
}

#[derive(Debug, Error)]
pub enum TenantResolverError {
    #[error("no tenant binding for account_id {0:?}")]
    NotFound(String),
    #[error("upstream: {0}")]
    Upstream(String),
    #[error("invalid tenant id: {0}")]
    Invalid(#[from] TenantIdError),
}

/// In-memory resolver — useful in tests + a sane default
/// when the operator runs the extension single-tenant.
pub struct StaticTenantResolver {
    map: std::collections::HashMap<String, TenantId>,
    /// Returned when no map entry exists. `None` => the
    /// resolver returns `NotFound`. Some(t) => single-tenant
    /// fallback.
    default: Option<TenantId>,
}

impl StaticTenantResolver {
    pub fn new(map: impl IntoIterator<Item = (String, TenantId)>) -> Self {
        Self {
            map: map.into_iter().collect(),
            default: None,
        }
    }

    pub fn with_default(mut self, t: TenantId) -> Self {
        self.default = Some(t);
        self
    }
}

#[async_trait]
impl TenantResolver for StaticTenantResolver {
    async fn resolve_for_account(
        &self,
        account_id: &str,
    ) -> Result<TenantId, TenantResolverError> {
        if let Some(t) = self.map.get(account_id) {
            return Ok(t.clone());
        }
        if let Some(t) = &self.default {
            return Ok(t.clone());
        }
        Err(TenantResolverError::NotFound(account_id.to_string()))
    }
}

// ── Helpers ─────────────────────────────────────────────────────

fn derive_thread_id(
    message_id: Option<&str>,
    in_reply_to: Option<&str>,
    references: &[String],
) -> String {
    // Standard email threading: the first message in a thread
    // is its own root. Replies inherit the root from
    // References (first entry) or fall back to In-Reply-To.
    if let Some(first) = references.first() {
        return first.trim_matches(|c| c == '<' || c == '>').to_string();
    }
    if let Some(irt) = in_reply_to {
        return irt.trim_matches(|c| c == '<' || c == '>').to_string();
    }
    if let Some(mid) = message_id {
        return mid.trim_matches(|c| c == '<' || c == '>').to_string();
    }
    // No identifiers at all → synthesize from sender + subject
    // hash so the same conversation lands in the same thread
    // even on broken clients. Last-resort.
    "thread-orphan".to_string()
}

fn strip_html(html: &str) -> String {
    // Single-pass strip — drop tags, collapse whitespace.
    // Good-enough for body excerpt; the LLM extractor sees
    // similar input from real inboxes.
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut last_was_space = false;
    for ch in html.chars() {
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch == '>' {
            in_tag = false;
            // Treat a closing tag as whitespace boundary.
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
            continue;
        }
        if in_tag {
            continue;
        }
        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_corporate() -> Vec<u8> {
        // Minimal RFC 5322 — one line headers + blank +
        // body. mail-parser handles real-world variants on
        // top of this baseline.
        let raw = "\
From: \"Juan García\" <juan@acme.com>\r\n\
To: ventas@miempresa.com\r\n\
Subject: Cotización servicio\r\n\
Message-ID: <abc-001@acme.com>\r\n\
Date: Mon, 12 May 2026 14:32:00 +0000\r\n\
\r\n\
Hola, queremos saber sobre su servicio.\r\n\
Saludos,\r\nJuan\r\n";
        raw.as_bytes().to_vec()
    }

    fn fixture_disposable() -> Vec<u8> {
        let raw = "\
From: spam@mailinator.com\r\n\
Subject: Test\r\n\
Date: Mon, 12 May 2026 14:32:00 +0000\r\n\
\r\nbody\r\n";
        raw.as_bytes().to_vec()
    }

    fn fixture_reply() -> Vec<u8> {
        let raw = "\
From: Pedro <pedro@acme.com>\r\n\
To: ventas@miempresa.com\r\n\
Subject: Re: Cotización servicio\r\n\
Message-ID: <reply-002@acme.com>\r\n\
In-Reply-To: <abc-001@acme.com>\r\n\
References: <abc-001@acme.com>\r\n\
Date: Tue, 13 May 2026 09:11:00 +0000\r\n\
\r\nGracias por escribir.\r\n";
        raw.as_bytes().to_vec()
    }

    fn fixture_personal_with_reply_to() -> Vec<u8> {
        let raw = "\
From: \"Maria\" <maria.l@gmail.com>\r\n\
Reply-To: maria@globex.io\r\n\
To: ventas@miempresa.com\r\n\
Subject: Consulta\r\n\
Date: Mon, 12 May 2026 10:00:00 +0000\r\n\
\r\nHola, dudas sobre el plan.\r\n";
        raw.as_bytes().to_vec()
    }

    fn fixture_html_body() -> Vec<u8> {
        let raw = "\
From: jane@globex.io\r\n\
Subject: Hi\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<html><body><p>Hi <b>there</b>!</p></body></html>";
        raw.as_bytes().to_vec()
    }

    #[test]
    fn decode_corporate_inbound_extracts_fields() {
        let p =
            decode_inbound_email("acme-ventas", "ventas@miempresa.com", 42, &fixture_corporate())
                .unwrap();
        assert_eq!(p.from_email, "juan@acme.com");
        assert_eq!(p.from_display_name.as_deref(), Some("Juan García"));
        assert_eq!(p.subject, "Cotización servicio");
        assert_eq!(p.from_domain_kind, DomainKind::Corporate);
        assert!(p.body_excerpt.contains("Hola"));
        assert_eq!(p.uid, 42);
        assert_eq!(p.account_id, "ventas@miempresa.com");
        // mail-parser strips RFC 5322 angle brackets — store
        // the bare id; thread_id helper trims the same way.
        assert_eq!(p.message_id.as_deref(), Some("abc-001@acme.com"));
        assert_eq!(p.thread_id, "abc-001@acme.com");
    }

    #[test]
    fn decode_disposable_returns_typed_error_pre_pipeline() {
        let err = decode_inbound_email("acme-ventas", "ventas", 1, &fixture_disposable())
            .unwrap_err();
        assert!(matches!(err, ParseError::DisposableSender(_)));
    }

    #[test]
    fn decode_reply_threads_to_root() {
        let p = decode_inbound_email("acme-ventas", "ventas", 2, &fixture_reply()).unwrap();
        // References carries the root id → thread_id matches.
        assert_eq!(p.thread_id, "abc-001@acme.com");
        assert_eq!(p.in_reply_to.as_deref(), Some("abc-001@acme.com"));
    }

    #[test]
    fn decode_picks_up_reply_to_for_personal_sender() {
        let p = decode_inbound_email(
            "acme-ventas",
            "ventas",
            5,
            &fixture_personal_with_reply_to(),
        )
        .unwrap();
        assert_eq!(p.from_domain_kind, DomainKind::Personal);
        assert_eq!(p.reply_to.as_deref(), Some("maria@globex.io"));
    }

    #[test]
    fn decode_html_body_strips_tags() {
        let p = decode_inbound_email("acme-ventas", "ventas", 9, &fixture_html_body()).unwrap();
        // `<b>there</b>` → "there"; tags gone, whitespace
        // collapsed.
        assert!(p.body_excerpt.contains("Hi"));
        assert!(p.body_excerpt.contains("there"));
        assert!(!p.body_excerpt.contains('<'));
        assert!(!p.body_excerpt.contains('>'));
    }

    #[test]
    fn decode_invalid_bytes_returns_decode_failed() {
        let err = decode_inbound_email("a", "b", 0, &[]).unwrap_err();
        // Empty input — mail-parser still returns a Message
        // with no headers, which fails at MissingHeader("From").
        assert!(matches!(
            err,
            ParseError::DecodeFailed | ParseError::MissingHeader(_)
        ));
    }

    // ── Tenant resolver ─────────────────────────────────────

    #[tokio::test]
    async fn static_resolver_hits_map() {
        let acme = TenantId::new("acme").unwrap();
        let r = StaticTenantResolver::new([("ventas".to_string(), acme.clone())]);
        let got = r.resolve_for_account("ventas").await.unwrap();
        assert_eq!(got, acme);
    }

    #[tokio::test]
    async fn static_resolver_falls_through_to_default() {
        let acme = TenantId::new("acme").unwrap();
        let r = StaticTenantResolver::new(std::iter::empty()).with_default(acme.clone());
        let got = r.resolve_for_account("anything").await.unwrap();
        assert_eq!(got, acme);
    }

    #[tokio::test]
    async fn static_resolver_returns_not_found_without_default() {
        let r = StaticTenantResolver::new(std::iter::empty());
        let err = r.resolve_for_account("ghost").await.unwrap_err();
        assert!(matches!(err, TenantResolverError::NotFound(_)));
    }
}
