//! Operator notification publisher (M15.38).
//!
//! Marketing publishes typed [`EmailNotification`] frames to
//! `agent.email.notification.<agent_id>` whenever a lead-
//! lifecycle event matches the bound vendedor's per-event
//! toggles. The agent's runtime / sidecar subscribes and
//! forwards via the configured channel (WhatsApp / email /
//! disabled).
//!
//! The marketing extension itself never reaches WhatsApp /
//! SMTP directly — that's the agent's responsibility through
//! its existing inbound bindings. Decoupling keeps marketing
//! agnostic to channel transports.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use nexo_tool_meta::marketing::{
    EmailNotification, EmailNotificationKind, Lead, NotificationChannel, TenantIdRef,
    Vendedor, VendedorId,
};

use crate::broker::inbound::ParsedInbound;
use crate::tenant::TenantId;

/// Lock-free atomic vendedor index. Keyed by vendedor id so
/// the broker hop can resolve `lead.vendedor_id` to the full
/// `Vendedor` record (including `agent_id` +
/// `notification_settings`) in O(1) per inbound. PUT
/// `/config/vendedores` rebuilds + swaps the inner map under
/// the broker hop's nose — same `arc_swap` pattern as the
/// router live-reload (M15.33).
pub type VendedorLookup = Arc<arc_swap::ArcSwap<HashMap<VendedorId, Vendedor>>>;

/// Build a `VendedorLookup` from a vendedores list — typically
/// loaded via `crate::config::load_vendedores` at boot or on
/// PUT.
pub fn vendedor_lookup_from_list(rows: Vec<Vendedor>) -> VendedorLookup {
    let map = rows.into_iter().map(|v| (v.id.clone(), v)).collect();
    Arc::new(arc_swap::ArcSwap::from_pointee(map))
}

/// Decision returned by [`maybe_notify_lead_created`]. Pure
/// classification — the publish itself happens in the broker
/// hop with the live `BrokerSender`. Splitting the decision
/// from the IO keeps the unit tests pure (no broker mock
/// needed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationOutcome {
    /// Vendedor not in the lookup (stale lead.vendedor_id, or
    /// vendedor deleted since lead create). Skip silently.
    VendedorMissing,
    /// Vendedor has `notification_settings = None` — operator
    /// hasn't opted in. Skip.
    NotConfigured,
    /// Vendedor has settings but `on_lead_created = false`. Skip.
    EventDisabled,
    /// Vendedor has settings + event enabled but `agent_id`
    /// is None — no agent to address the topic to. Skip.
    NoAgentBound,
    /// Channel is `Disabled` — operator opted in to the event
    /// but explicitly disabled forwarding. Useful for "log
    /// only" flows. Skip.
    ChannelDisabled,
    /// All checks pass — caller should `BrokerSender::publish`
    /// to `topic` with the `payload` as the `Event.payload`.
    Publish {
        topic: String,
        payload: EmailNotification,
    },
}

/// Inspect the vendedor's notification settings + the lead +
/// the parsed inbound; either return `Publish` with the
/// pre-built topic + payload, or one of the skip variants.
pub fn maybe_notify_lead_created(
    tenant_id: &TenantId,
    vendedores: &VendedorLookup,
    lead: &Lead,
    parsed: &ParsedInbound,
) -> NotificationOutcome {
    let map = vendedores.load();
    let v = match map.get(&lead.vendedor_id) {
        Some(v) => v,
        None => return NotificationOutcome::VendedorMissing,
    };
    let settings = match &v.notification_settings {
        Some(s) => s,
        None => return NotificationOutcome::NotConfigured,
    };
    if !settings.on_lead_created {
        return NotificationOutcome::EventDisabled;
    }
    let agent_id = match &v.agent_id {
        Some(a) => a.clone(),
        None => return NotificationOutcome::NoAgentBound,
    };
    if matches!(settings.channel, NotificationChannel::Disabled) {
        return NotificationOutcome::ChannelDisabled;
    }

    let summary = render_summary(EmailNotificationKind::LeadCreated, parsed, v);
    let payload = EmailNotification {
        kind: EmailNotificationKind::LeadCreated,
        tenant_id: TenantIdRef(tenant_id.as_str().into()),
        agent_id: agent_id.clone(),
        lead_id: lead.id.clone(),
        vendedor_id: v.id.clone(),
        vendedor_email: v.primary_email.clone(),
        from_email: parsed.from_email.clone(),
        subject: parsed.subject.clone(),
        at_ms: Utc::now().timestamp_millis(),
        summary,
        channel: settings.channel.clone(),
    };
    NotificationOutcome::Publish {
        topic: format!("agent.email.notification.{agent_id}"),
        payload,
    }
}

/// Compose the operator-facing summary the forwarder can use
/// verbatim as the WA / email body. Localised to the
/// vendedor's `preferred_language` when set; defaults to
/// Spanish (operator base case).
fn render_summary(
    kind: EmailNotificationKind,
    parsed: &ParsedInbound,
    v: &Vendedor,
) -> String {
    let lang = v.preferred_language.as_deref().unwrap_or("es");
    let from = parsed
        .from_display_name
        .as_deref()
        .unwrap_or(&parsed.from_email);
    match (kind, lang) {
        (EmailNotificationKind::LeadCreated, "en") => format!(
            "📧 New lead from {from}\nSubject: {subj}\nVendedor: {ve}",
            subj = parsed.subject,
            ve = v.primary_email,
        ),
        (EmailNotificationKind::LeadCreated, _) => format!(
            "📧 Nuevo lead de {from}\nAsunto: {subj}\nVendedor: {ve}",
            subj = parsed.subject,
            ve = v.primary_email,
        ),
        // Future kinds — fall through with a generic stub
        // that the WA forwarder can still display.
        (other, _) => format!(
            "📧 {other:?} · {from} · {subj}",
            subj = parsed.subject,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nexo_tool_meta::marketing::{
        DomainKind, LeadId, LeadState, NotificationChannel, PersonId, VendedorId,
        VendedorNotificationSettings,
    };

    fn vendedor_with(
        id: &str,
        agent_id: Option<&str>,
        settings: Option<VendedorNotificationSettings>,
    ) -> Vendedor {
        Vendedor {
            id: VendedorId(id.into()),
            tenant_id: TenantIdRef("acme".into()),
            name: "Pedro García".into(),
            primary_email: format!("{id}@acme.com"),
            alt_emails: Vec::new(),
            signature_text: String::new(),
            working_hours: None,
            on_vacation: false,
            vacation_until: None,
            preferred_language: Some("es".into()),
            agent_id: agent_id.map(|s| s.into()),
            model_override: None,
            notification_settings: settings,
        }
    }

    fn lead_with(vendedor: &str) -> Lead {
        Lead {
            id: LeadId("l-1".into()),
            tenant_id: TenantIdRef("acme".into()),
            thread_id: "th-1".into(),
            subject: "Cotización CRM".into(),
            person_id: PersonId("p-1".into()),
            vendedor_id: VendedorId(vendedor.into()),
            state: LeadState::Cold,
            score: 0,
            sentiment: nexo_tool_meta::marketing::SentimentBand::Neutral,
            intent: nexo_tool_meta::marketing::IntentClass::Browsing,
            topic_tags: Vec::new(),
            last_activity_ms: 0,
            next_check_at_ms: None,
            followup_attempts: 0,
            why_routed: Vec::new(),
        }
    }

    fn parsed_inbound() -> ParsedInbound {
        ParsedInbound {
            instance: "default".into(),
            account_id: "acct-1".into(),
            uid: 42,
            from_email: "cliente@empresa.com".into(),
            from_display_name: Some("Cliente".into()),
            to_emails: vec!["pedro@acme.com".into()],
            reply_to: None,
            subject: "Cotización CRM".into(),
            message_id: Some("abc".into()),
            in_reply_to: None,
            references: Vec::new(),
            body_excerpt: "Hola, queremos cotizar".into(),
            thread_id: "th-1".into(),
            from_domain_kind: DomainKind::Corporate,
        }
    }

    fn lookup(rows: Vec<Vendedor>) -> VendedorLookup {
        vendedor_lookup_from_list(rows)
    }

    #[test]
    fn missing_vendedor_short_circuits() {
        let lk = lookup(vec![]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            &lead_with("ghost"),
            &parsed_inbound(),
        );
        assert_eq!(out, NotificationOutcome::VendedorMissing);
    }

    #[test]
    fn vendedor_without_settings_short_circuits() {
        let v = vendedor_with("pedro", Some("pedro-agent"), None);
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        assert_eq!(out, NotificationOutcome::NotConfigured);
    }

    #[test]
    fn event_toggle_off_short_circuits() {
        let mut s = VendedorNotificationSettings::default();
        s.on_lead_created = false;
        let v = vendedor_with("pedro", Some("pedro-agent"), Some(s));
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        assert_eq!(out, NotificationOutcome::EventDisabled);
    }

    #[test]
    fn no_agent_bound_short_circuits() {
        let v = vendedor_with(
            "pedro",
            None,
            Some(VendedorNotificationSettings::default()),
        );
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        assert_eq!(out, NotificationOutcome::NoAgentBound);
    }

    #[test]
    fn channel_disabled_short_circuits() {
        let mut s = VendedorNotificationSettings::default();
        s.channel = NotificationChannel::Disabled;
        let v = vendedor_with("pedro", Some("pedro-agent"), Some(s));
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        assert_eq!(out, NotificationOutcome::ChannelDisabled);
    }

    #[test]
    fn happy_path_returns_publish_with_topic_and_payload() {
        let s = VendedorNotificationSettings::default();
        let v = vendedor_with("pedro", Some("pedro-agent"), Some(s));
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        match out {
            NotificationOutcome::Publish { topic, payload } => {
                assert_eq!(topic, "agent.email.notification.pedro-agent");
                assert_eq!(payload.kind, EmailNotificationKind::LeadCreated);
                assert_eq!(payload.agent_id, "pedro-agent");
                assert_eq!(payload.vendedor_id.0, "pedro");
                assert_eq!(payload.from_email, "cliente@empresa.com");
                match &payload.channel {
                    NotificationChannel::Whatsapp { instance } => {
                        // Default-built settings carry an empty
                        // instance — frontend resolves before save.
                        assert_eq!(instance, "");
                    }
                    other => panic!("expected Whatsapp, got {other:?}"),
                }
                assert!(
                    payload.summary.contains("Nuevo lead"),
                    "summary should be ES by default: {}",
                    payload.summary
                );
                assert!(payload.summary.contains("Cliente"));
            }
            other => panic!("expected Publish, got {other:?}"),
        }
    }

    #[test]
    fn english_locale_uses_english_summary() {
        let mut v = vendedor_with(
            "pedro",
            Some("pedro-agent"),
            Some(VendedorNotificationSettings::default()),
        );
        v.preferred_language = Some("en".into());
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        match out {
            NotificationOutcome::Publish { payload, .. } => {
                assert!(
                    payload.summary.starts_with("📧 New lead"),
                    "summary should be EN: {}",
                    payload.summary
                );
            }
            other => panic!("expected Publish, got {other:?}"),
        }
    }

    #[test]
    fn email_channel_passes_through_to_payload() {
        let mut s = VendedorNotificationSettings::default();
        s.channel = NotificationChannel::Email {
            from_instance: "ventas-acme".into(),
            to: "ops@acme.com".into(),
        };
        let v = vendedor_with("pedro", Some("pedro-agent"), Some(s));
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        match out {
            NotificationOutcome::Publish { payload, .. } => match payload.channel {
                NotificationChannel::Email { from_instance, to } => {
                    assert_eq!(from_instance, "ventas-acme");
                    assert_eq!(to, "ops@acme.com");
                }
                other => panic!("expected Email channel, got {other:?}"),
            },
            other => panic!("expected Publish, got {other:?}"),
        }
    }

    #[test]
    fn lookup_swap_picks_up_new_vendedor() {
        // Build empty lookup, then store a fresh map with a
        // vendedor — the next `load()` returns the new map.
        let lk = lookup(vec![]);
        let v = vendedor_with(
            "pedro",
            Some("pedro-agent"),
            Some(VendedorNotificationSettings::default()),
        );
        let new_map = std::iter::once((v.id.clone(), v.clone())).collect();
        lk.store(Arc::new(new_map));
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        assert!(matches!(out, NotificationOutcome::Publish { .. }));
    }
}
