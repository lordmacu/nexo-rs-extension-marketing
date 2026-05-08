//! Operator notification publisher (M15.38).
//!
//! Marketing publishes typed [`EmailNotification`] frames to
//! `agent.email.notification.<agent_id>` whenever a lead-
//! lifecycle event matches the bound seller's per-event
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
    EmailNotification, EmailNotificationKind, Lead, LeadState, NotificationChannel,
    NotificationTemplates, Seller, SellerId, TenantIdRef,
};
use nexo_tool_meta::template::render_template;
use serde_json::json;

use crate::broker::inbound::ParsedInbound;
use crate::tenant::TenantId;

/// Lock-free atomic seller index. Keyed by seller id so
/// the broker hop can resolve `lead.seller_id` to the full
/// `Seller` record (including `agent_id` +
/// `notification_settings`) in O(1) per inbound. PUT
/// `/config/sellers` rebuilds + swaps the inner map under
/// the broker hop's nose — same `arc_swap` pattern as the
/// router live-reload (M15.33).
pub type SellerLookup = Arc<arc_swap::ArcSwap<HashMap<SellerId, Seller>>>;

/// Build a `SellerLookup` from a sellers list — typically
/// loaded via `crate::config::load_sellers` at boot or on
/// PUT.
pub fn seller_lookup_from_list(rows: Vec<Seller>) -> SellerLookup {
    let map = rows.into_iter().map(|v| (v.id.clone(), v)).collect();
    Arc::new(arc_swap::ArcSwap::from_pointee(map))
}

/// M15.44 — lock-free atomic notification template overrides.
/// `PUT /config/notification_templates` swaps a freshly loaded
/// `NotificationTemplates` into this Arc; the broker hop's
/// next render reads the updated strings without a process
/// restart. `None` (or empty templates) → renderers fall back
/// to the framework's hardcoded ES/EN strings.
pub type TemplateLookup = Arc<arc_swap::ArcSwap<NotificationTemplates>>;

/// Build a `TemplateLookup` from a typed templates record.
pub fn template_lookup_from(templates: NotificationTemplates) -> TemplateLookup {
    Arc::new(arc_swap::ArcSwap::from_pointee(templates))
}

/// Decision returned by [`maybe_notify_lead_created`]. Pure
/// classification — the publish itself happens in the broker
/// hop with the live `BrokerSender`. Splitting the decision
/// from the IO keeps the unit tests pure (no broker mock
/// needed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationOutcome {
    /// Seller not in the lookup (stale lead.seller_id, or
    /// seller deleted since lead create). Skip silently.
    SellerMissing,
    /// Seller has `notification_settings = None` — operator
    /// hasn't opted in. Skip.
    NotConfigured,
    /// Seller has settings but `on_lead_created = false`. Skip.
    EventDisabled,
    /// Seller has settings + event enabled but `agent_id`
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

/// Inspect the seller's notification settings + the lead +
/// the parsed inbound; either return `Publish` with the
/// pre-built topic + payload, or one of the skip variants.
pub fn maybe_notify_lead_created(
    tenant_id: &TenantId,
    sellers: &SellerLookup,
    templates: Option<&TemplateLookup>,
    lead: &Lead,
    parsed: &ParsedInbound,
) -> NotificationOutcome {
    classify(
        tenant_id,
        sellers,
        templates,
        lead,
        parsed,
        EmailNotificationKind::LeadCreated,
        |s| s.on_lead_created,
    )
}

/// M15.40 — same shape as `maybe_notify_lead_created` but for
/// the existing-thread reply event. The broker hop fires this
/// from the `find_by_thread → Some(lead)` branch.
pub fn maybe_notify_lead_replied(
    tenant_id: &TenantId,
    sellers: &SellerLookup,
    templates: Option<&TemplateLookup>,
    lead: &Lead,
    parsed: &ParsedInbound,
) -> NotificationOutcome {
    classify(
        tenant_id,
        sellers,
        templates,
        lead,
        parsed,
        EmailNotificationKind::LeadReplied,
        |s| s.on_lead_replied,
    )
}

/// M15.41 — `LeadTransitioned` notification fired after a
/// successful state transition (`lead_mark_qualified` tool
/// today; future tools mounting on the same store hook lead).
/// Doesn't take a `ParsedInbound` because transitions don't
/// originate from an inbound — the summary describes the
/// state delta + the operator-supplied reason.
pub fn maybe_notify_lead_transitioned(
    tenant_id: &TenantId,
    sellers: &SellerLookup,
    templates: Option<&TemplateLookup>,
    lead: &Lead,
    from: LeadState,
    to: LeadState,
    reason: &str,
) -> NotificationOutcome {
    let map = sellers.load();
    let v = match map.get(&lead.seller_id) {
        Some(v) => v,
        None => return NotificationOutcome::SellerMissing,
    };
    let settings = match &v.notification_settings {
        Some(s) => s,
        None => return NotificationOutcome::NotConfigured,
    };
    if !settings.on_lead_transitioned {
        return NotificationOutcome::EventDisabled;
    }
    let agent_id = match &v.agent_id {
        Some(a) => a.clone(),
        None => return NotificationOutcome::NoAgentBound,
    };
    if matches!(settings.channel, NotificationChannel::Disabled) {
        return NotificationOutcome::ChannelDisabled;
    }

    let templates_snap = templates.map(|t| t.load());
    let summary = render_transition_summary(
        from,
        to,
        reason,
        v,
        templates_snap.as_ref().map(|s| s.as_ref()),
        lead,
    );
    let payload = EmailNotification {
        kind: EmailNotificationKind::LeadTransitioned,
        tenant_id: TenantIdRef(tenant_id.as_str().into()),
        agent_id: agent_id.clone(),
        lead_id: lead.id.clone(),
        seller_id: v.id.clone(),
        seller_email: v.primary_email.clone(),
        from_email: format!("transition:{from:?}->{to:?}"),
        subject: lead.subject.clone(),
        at_ms: Utc::now().timestamp_millis(),
        summary,
        channel: settings.channel.clone(),
    };
    NotificationOutcome::Publish {
        topic: format!("agent.email.notification.{agent_id}"),
        payload,
    }
}

/// M15.41 — `MeetingIntent` notification fired by the
/// `lead_detect_meeting_intent` tool when confidence ≥ 0.7.
/// Operator gets pinged so they can confirm + book the
/// meeting before the AI auto-responds.
pub fn maybe_notify_meeting_intent(
    tenant_id: &TenantId,
    sellers: &SellerLookup,
    templates: Option<&TemplateLookup>,
    lead: &Lead,
    confidence: f32,
    evidence_span: &str,
) -> NotificationOutcome {
    let map = sellers.load();
    let v = match map.get(&lead.seller_id) {
        Some(v) => v,
        None => return NotificationOutcome::SellerMissing,
    };
    let settings = match &v.notification_settings {
        Some(s) => s,
        None => return NotificationOutcome::NotConfigured,
    };
    if !settings.on_meeting_intent {
        return NotificationOutcome::EventDisabled;
    }
    let agent_id = match &v.agent_id {
        Some(a) => a.clone(),
        None => return NotificationOutcome::NoAgentBound,
    };
    if matches!(settings.channel, NotificationChannel::Disabled) {
        return NotificationOutcome::ChannelDisabled;
    }

    let templates_snap = templates.map(|t| t.load());
    let summary = render_intent_summary(
        confidence,
        evidence_span,
        v,
        templates_snap.as_ref().map(|s| s.as_ref()),
        lead,
    );
    let payload = EmailNotification {
        kind: EmailNotificationKind::MeetingIntent,
        tenant_id: TenantIdRef(tenant_id.as_str().into()),
        agent_id: agent_id.clone(),
        lead_id: lead.id.clone(),
        seller_id: v.id.clone(),
        seller_email: v.primary_email.clone(),
        from_email: "intent:meeting".into(),
        subject: lead.subject.clone(),
        at_ms: Utc::now().timestamp_millis(),
        summary,
        channel: settings.channel.clone(),
    };
    NotificationOutcome::Publish {
        topic: format!("agent.email.notification.{agent_id}"),
        payload,
    }
}

fn render_transition_summary(
    from: LeadState,
    to: LeadState,
    reason: &str,
    v: &Seller,
    templates: Option<&NotificationTemplates>,
    lead: &Lead,
) -> String {
    let lang = v.preferred_language.as_deref().unwrap_or("es");
    // M15.44 — operator-supplied template overrides the
    // hardcoded ES/EN strings when set.
    if let Some(t) = templates {
        if let Some(tpl) = t.lead_transitioned.as_ref().and_then(|set| set.for_lang(lang)) {
            let ctx = json!({
                "state_from": format!("{from:?}"),
                "state_to": format!("{to:?}"),
                "reason": reason,
                "seller": v.name,
                "seller_email": v.primary_email,
                "lead_id": lead.id.0,
                "subject": lead.subject,
            });
            return render_template(tpl, &ctx);
        }
    }
    let arrow = format!("{from:?} → {to:?}");
    if lang == "en" {
        format!("🔄 Lead transitioned · {arrow}\nReason: {reason}\nSeller: {ve}", ve = v.primary_email)
    } else {
        format!("🔄 Lead transicionó · {arrow}\nMotivo: {reason}\nSeller: {ve}", ve = v.primary_email)
    }
}

fn render_intent_summary(
    confidence: f32,
    evidence: &str,
    v: &Seller,
    templates: Option<&NotificationTemplates>,
    lead: &Lead,
) -> String {
    let lang = v.preferred_language.as_deref().unwrap_or("es");
    let pct = (confidence * 100.0).round() as i32;
    if let Some(t) = templates {
        if let Some(tpl) = t.meeting_intent.as_ref().and_then(|set| set.for_lang(lang)) {
            let ctx = json!({
                "confidence_pct": pct,
                "evidence": evidence,
                "seller": v.name,
                "seller_email": v.primary_email,
                "lead_id": lead.id.0,
                "subject": lead.subject,
            });
            return render_template(tpl, &ctx);
        }
    }
    if lang == "en" {
        format!(
            "📅 Meeting intent detected ({pct}%)\nEvidence: {evidence}\nSeller: {ve}",
            ve = v.primary_email
        )
    } else {
        format!(
            "📅 Intent de reunión detectado ({pct}%)\nEvidencia: {evidence}\nSeller: {ve}",
            ve = v.primary_email
        )
    }
}

/// Common classifier shared between `lead_created` +
/// `lead_replied` (and future kinds). Reads the per-event
/// toggle via `is_enabled` closure so the call sites stay
/// declarative.
fn classify<F>(
    tenant_id: &TenantId,
    sellers: &SellerLookup,
    templates: Option<&TemplateLookup>,
    lead: &Lead,
    parsed: &ParsedInbound,
    kind: EmailNotificationKind,
    is_enabled: F,
) -> NotificationOutcome
where
    F: FnOnce(&nexo_tool_meta::marketing::SellerNotificationSettings) -> bool,
{
    let map = sellers.load();
    let v = match map.get(&lead.seller_id) {
        Some(v) => v,
        None => return NotificationOutcome::SellerMissing,
    };
    let settings = match &v.notification_settings {
        Some(s) => s,
        None => return NotificationOutcome::NotConfigured,
    };
    if !is_enabled(settings) {
        return NotificationOutcome::EventDisabled;
    }
    let agent_id = match &v.agent_id {
        Some(a) => a.clone(),
        None => return NotificationOutcome::NoAgentBound,
    };
    if matches!(settings.channel, NotificationChannel::Disabled) {
        return NotificationOutcome::ChannelDisabled;
    }

    let templates_snap = templates.map(|t| t.load());
    let summary = render_summary(
        kind.clone(),
        parsed,
        v,
        templates_snap.as_ref().map(|s| s.as_ref()),
        lead,
    );
    let payload = EmailNotification {
        kind,
        tenant_id: TenantIdRef(tenant_id.as_str().into()),
        agent_id: agent_id.clone(),
        lead_id: lead.id.clone(),
        seller_id: v.id.clone(),
        seller_email: v.primary_email.clone(),
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
/// seller's `preferred_language` when set; defaults to
/// Spanish (operator base case).
fn render_summary(
    kind: EmailNotificationKind,
    parsed: &ParsedInbound,
    v: &Seller,
    templates: Option<&NotificationTemplates>,
    lead: &Lead,
) -> String {
    let lang = v.preferred_language.as_deref().unwrap_or("es");
    let from = parsed
        .from_display_name
        .as_deref()
        .unwrap_or(&parsed.from_email);
    // M15.44 — operator-supplied template overrides hardcoded
    // ES/EN strings when set. Pick by kind + locale; missing
    // template → fall through to the hardcoded match below.
    if let Some(t) = templates {
        let set = match kind {
            EmailNotificationKind::LeadCreated => t.lead_created.as_ref(),
            EmailNotificationKind::LeadReplied => t.lead_replied.as_ref(),
            // Transitioned + MeetingIntent + DraftPending get
            // their own render_* fns; they shouldn't reach
            // here, but defensive None keeps the fall-through
            // safe.
            _ => None,
        };
        if let Some(tpl) = set.and_then(|s| s.for_lang(lang)) {
            let ctx = json!({
                "from": from,
                "from_email": parsed.from_email,
                "subject": parsed.subject,
                "seller": v.name,
                "seller_email": v.primary_email,
                "lead_id": lead.id.0,
            });
            return render_template(tpl, &ctx);
        }
    }
    match (kind, lang) {
        (EmailNotificationKind::LeadCreated, "en") => format!(
            "📧 New lead from {from}\nSubject: {subj}\nSeller: {ve}",
            subj = parsed.subject,
            ve = v.primary_email,
        ),
        (EmailNotificationKind::LeadCreated, _) => format!(
            "📧 Nuevo lead de {from}\nAsunto: {subj}\nSeller: {ve}",
            subj = parsed.subject,
            ve = v.primary_email,
        ),
        (EmailNotificationKind::LeadReplied, "en") => format!(
            "💬 {from} replied\nThread: {subj}\nSeller: {ve}",
            subj = parsed.subject,
            ve = v.primary_email,
        ),
        (EmailNotificationKind::LeadReplied, _) => format!(
            "💬 {from} respondió\nHilo: {subj}\nSeller: {ve}",
            subj = parsed.subject,
            ve = v.primary_email,
        ),
        // Kinds with their own dedicated renderers
        // (`render_transition_summary`, `render_intent_summary`).
        // Reach this path only if a future publisher routes
        // them through `classify` instead. Render a reasonable
        // human string so the operator never sees `Debug`-
        // formatted output (closes F16).
        (EmailNotificationKind::LeadTransitioned, "en") => format!(
            "🔄 Lead transitioned · {from}\nThread: {subj}\nSeller: {ve}",
            subj = parsed.subject,
            ve = v.primary_email,
        ),
        (EmailNotificationKind::LeadTransitioned, _) => format!(
            "🔄 Lead transicionó · {from}\nHilo: {subj}\nSeller: {ve}",
            subj = parsed.subject,
            ve = v.primary_email,
        ),
        (EmailNotificationKind::MeetingIntent, "en") => format!(
            "📅 Meeting intent from {from}\nThread: {subj}\nSeller: {ve}",
            subj = parsed.subject,
            ve = v.primary_email,
        ),
        (EmailNotificationKind::MeetingIntent, _) => format!(
            "📅 Intent de reunión de {from}\nHilo: {subj}\nSeller: {ve}",
            subj = parsed.subject,
            ve = v.primary_email,
        ),
        (EmailNotificationKind::DraftPending, "en") => format!(
            "✉️ Draft pending review · {from}\nThread: {subj}\nSeller: {ve}",
            subj = parsed.subject,
            ve = v.primary_email,
        ),
        (EmailNotificationKind::DraftPending, _) => format!(
            "✉️ Draft pendiente de revisión · {from}\nHilo: {subj}\nSeller: {ve}",
            subj = parsed.subject,
            ve = v.primary_email,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nexo_tool_meta::marketing::{
        DomainKind, LeadId, LeadState, NotificationChannel, PersonId, SellerId,
        SellerNotificationSettings,
    };

    fn seller_with(
        id: &str,
        agent_id: Option<&str>,
        settings: Option<SellerNotificationSettings>,
    ) -> Seller {
        Seller {
            id: SellerId(id.into()),
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
            smtp_credential: None,
        }
    }

    fn lead_with(seller: &str) -> Lead {
        Lead {
            id: LeadId("l-1".into()),
            tenant_id: TenantIdRef("acme".into()),
            thread_id: "th-1".into(),
            subject: "Cotización CRM".into(),
            person_id: PersonId("p-1".into()),
            seller_id: SellerId(seller.into()),
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

    fn lookup(rows: Vec<Seller>) -> SellerLookup {
        seller_lookup_from_list(rows)
    }

    #[test]
    fn missing_seller_short_circuits() {
        let lk = lookup(vec![]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
            &lead_with("ghost"),
            &parsed_inbound(),
        );
        assert_eq!(out, NotificationOutcome::SellerMissing);
    }

    #[test]
    fn seller_without_settings_short_circuits() {
        let v = seller_with("pedro", Some("pedro-agent"), None);
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        assert_eq!(out, NotificationOutcome::NotConfigured);
    }

    #[test]
    fn event_toggle_off_short_circuits() {
        let mut s = SellerNotificationSettings::default();
        s.on_lead_created = false;
        let v = seller_with("pedro", Some("pedro-agent"), Some(s));
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        assert_eq!(out, NotificationOutcome::EventDisabled);
    }

    #[test]
    fn no_agent_bound_short_circuits() {
        let v = seller_with(
            "pedro",
            None,
            Some(SellerNotificationSettings::default()),
        );
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        assert_eq!(out, NotificationOutcome::NoAgentBound);
    }

    #[test]
    fn channel_disabled_short_circuits() {
        let mut s = SellerNotificationSettings::default();
        s.channel = NotificationChannel::Disabled;
        let v = seller_with("pedro", Some("pedro-agent"), Some(s));
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        assert_eq!(out, NotificationOutcome::ChannelDisabled);
    }

    #[test]
    fn happy_path_returns_publish_with_topic_and_payload() {
        let s = SellerNotificationSettings::default();
        let v = seller_with("pedro", Some("pedro-agent"), Some(s));
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        match out {
            NotificationOutcome::Publish { topic, payload } => {
                assert_eq!(topic, "agent.email.notification.pedro-agent");
                assert_eq!(payload.kind, EmailNotificationKind::LeadCreated);
                assert_eq!(payload.agent_id, "pedro-agent");
                assert_eq!(payload.seller_id.0, "pedro");
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
        let mut v = seller_with(
            "pedro",
            Some("pedro-agent"),
            Some(SellerNotificationSettings::default()),
        );
        v.preferred_language = Some("en".into());
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
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
        let mut s = SellerNotificationSettings::default();
        s.channel = NotificationChannel::Email {
            from_instance: "ventas-acme".into(),
            to: "ops@acme.com".into(),
        };
        let v = seller_with("pedro", Some("pedro-agent"), Some(s));
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
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
    fn lead_replied_classifier_uses_dedicated_toggle() {
        // Settings has on_lead_created=true, on_lead_replied=false.
        // Classifier for replied should short-circuit
        // EventDisabled while created would fire normally.
        let mut s = SellerNotificationSettings::default();
        s.on_lead_replied = false;
        let v = seller_with("pedro", Some("pedro-agent"), Some(s));
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_replied(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        assert_eq!(out, NotificationOutcome::EventDisabled);
    }

    #[test]
    fn lead_replied_happy_path_publishes_with_distinct_kind() {
        let s = SellerNotificationSettings::default();
        let v = seller_with("pedro", Some("pedro-agent"), Some(s));
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_replied(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        match out {
            NotificationOutcome::Publish { topic, payload } => {
                assert_eq!(topic, "agent.email.notification.pedro-agent");
                assert_eq!(payload.kind, EmailNotificationKind::LeadReplied);
                // ES default: replied summary uses 💬 + "respondió"
                assert!(
                    payload.summary.contains("respondió"),
                    "ES summary should use 'respondió': {}",
                    payload.summary
                );
                assert!(payload.summary.starts_with("💬"));
            }
            other => panic!("expected Publish, got {other:?}"),
        }
    }

    #[test]
    fn lead_replied_english_summary_uses_replied_verb() {
        let mut v = seller_with(
            "pedro",
            Some("pedro-agent"),
            Some(SellerNotificationSettings::default()),
        );
        v.preferred_language = Some("en".into());
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_replied(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        match out {
            NotificationOutcome::Publish { payload, .. } => {
                assert!(
                    payload.summary.contains("replied"),
                    "EN summary should use 'replied': {}",
                    payload.summary
                );
            }
            other => panic!("expected Publish, got {other:?}"),
        }
    }

    #[test]
    fn lead_transitioned_classifier_uses_dedicated_toggle() {
        // Default settings have on_lead_transitioned=false so
        // the classifier should EventDisabled even with full
        // seller + agent setup.
        let s = SellerNotificationSettings::default();
        let v = seller_with("pedro", Some("pedro-agent"), Some(s));
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_transitioned(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
            &lead_with("pedro"),
            LeadState::MeetingScheduled,
            LeadState::Qualified,
            "demo agendada",
        );
        assert_eq!(out, NotificationOutcome::EventDisabled);
    }

    #[test]
    fn lead_transitioned_with_toggle_on_publishes() {
        let mut s = SellerNotificationSettings::default();
        s.on_lead_transitioned = true;
        let v = seller_with("pedro", Some("pedro-agent"), Some(s));
        let lk = lookup(vec![v]);
        let out = maybe_notify_lead_transitioned(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
            &lead_with("pedro"),
            LeadState::MeetingScheduled,
            LeadState::Qualified,
            "demo agendada confirmada",
        );
        match out {
            NotificationOutcome::Publish { topic, payload } => {
                assert_eq!(topic, "agent.email.notification.pedro-agent");
                assert_eq!(payload.kind, EmailNotificationKind::LeadTransitioned);
                assert!(
                    payload.summary.contains("MeetingScheduled → Qualified"),
                    "summary should carry from→to: {}",
                    payload.summary
                );
                assert!(payload.summary.contains("demo agendada confirmada"));
            }
            other => panic!("expected Publish, got {other:?}"),
        }
    }

    #[test]
    fn meeting_intent_classifier_uses_dedicated_toggle() {
        let mut s = SellerNotificationSettings::default();
        s.on_meeting_intent = false;
        let v = seller_with("pedro", Some("pedro-agent"), Some(s));
        let lk = lookup(vec![v]);
        let out = maybe_notify_meeting_intent(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
            &lead_with("pedro"),
            0.85,
            "Yes, Tuesday at 3pm",
        );
        assert_eq!(out, NotificationOutcome::EventDisabled);
    }

    #[test]
    fn meeting_intent_publishes_with_confidence_in_summary() {
        let s = SellerNotificationSettings::default();
        let v = seller_with("pedro", Some("pedro-agent"), Some(s));
        let lk = lookup(vec![v]);
        let out = maybe_notify_meeting_intent(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
            &lead_with("pedro"),
            0.85,
            "Yes, Tuesday at 3pm",
        );
        match out {
            NotificationOutcome::Publish { topic, payload } => {
                assert_eq!(topic, "agent.email.notification.pedro-agent");
                assert_eq!(payload.kind, EmailNotificationKind::MeetingIntent);
                assert!(
                    payload.summary.contains("85%"),
                    "summary should carry rounded confidence pct: {}",
                    payload.summary
                );
                assert!(payload.summary.contains("Tuesday at 3pm"));
            }
            other => panic!("expected Publish, got {other:?}"),
        }
    }

    #[test]
    fn lookup_swap_picks_up_new_seller() {
        // Build empty lookup, then store a fresh map with a
        // seller — the next `load()` returns the new map.
        let lk = lookup(vec![]);
        let v = seller_with(
            "pedro",
            Some("pedro-agent"),
            Some(SellerNotificationSettings::default()),
        );
        let new_map = std::iter::once((v.id.clone(), v.clone())).collect();
        lk.store(Arc::new(new_map));
        let out = maybe_notify_lead_created(
            &TenantId::new("acme").unwrap(),
            &lk,
            None,
            &lead_with("pedro"),
            &parsed_inbound(),
        );
        assert!(matches!(out, NotificationOutcome::Publish { .. }));
    }

    // M15.47 — F16: render_summary fallback for the 3 kinds
    // that don't normally route through this function. The
    // dedicated renderers (`render_transition_summary`,
    // `render_intent_summary`) own those kinds today, but if
    // a future publisher uses `classify` for them, the
    // operator should NOT see `Debug`-formatted output.

    #[test]
    fn render_summary_lead_transitioned_kind_uses_human_text() {
        let v = seller_with("pedro", None, None);
        let s = render_summary(
            EmailNotificationKind::LeadTransitioned,
            &parsed_inbound(),
            &v,
            None,
            &lead_with("pedro"),
        );
        // ES default; emoji + verb visible — NOT `LeadTransitioned`
        // Debug literal.
        assert!(s.contains("🔄"));
        assert!(s.contains("transicionó"));
        assert!(!s.contains("LeadTransitioned"));
    }

    #[test]
    fn render_summary_meeting_intent_kind_uses_human_text() {
        let v = seller_with("pedro", None, None);
        let s = render_summary(
            EmailNotificationKind::MeetingIntent,
            &parsed_inbound(),
            &v,
            None,
            &lead_with("pedro"),
        );
        assert!(s.contains("📅"));
        assert!(s.contains("Intent de reunión"));
        assert!(!s.contains("MeetingIntent"));
    }

    #[test]
    fn render_summary_draft_pending_kind_uses_human_text() {
        let v = seller_with("pedro", None, None);
        let s = render_summary(
            EmailNotificationKind::DraftPending,
            &parsed_inbound(),
            &v,
            None,
            &lead_with("pedro"),
        );
        assert!(s.contains("✉️"));
        assert!(s.contains("Draft pendiente"));
        assert!(!s.contains("DraftPending"));
    }

    #[test]
    fn render_summary_english_locale_for_fallback_kinds() {
        let mut v = seller_with("pedro", None, None);
        v.preferred_language = Some("en".into());
        for kind in [
            EmailNotificationKind::LeadTransitioned,
            EmailNotificationKind::MeetingIntent,
            EmailNotificationKind::DraftPending,
        ] {
            let s = render_summary(
                kind.clone(),
                &parsed_inbound(),
                &v,
                None,
                &lead_with("pedro"),
            );
            // Defensive — neither locale should ever leak the
            // Rust enum variant name to the operator.
            assert!(!s.contains(&format!("{kind:?}")), "leaked Debug for {kind:?}: {s}");
            assert!(s.contains("Thread:") || s.contains("Draft"));
        }
    }
}
