//! Notification forwarder (M15.39).
//!
//! **F29 sweep:** marketing-specific by design. Subject
//! routing logic + idempotency dedup uses the lifted SDK
//! `dedup` cache; this module is the CRM-shaped consumer
//! that knows the wa-bridge / email-plugin outbound topic
//! conventions.
//!
//! Closes the operator-notification loop opened in M15.38.
//! Subscribes to `agent.email.notification.*` (the very topic
//! [`crate::notification`] publishes to) and routes the payload
//! to the bound channel's outbound topic:
//!
//! - `Whatsapp { instance }` →
//!   `plugin.outbound.whatsapp.<instance>` carrying the
//!   `summary` as the message body. The wabridge plugin
//!   subscribes here and delivers via the existing WA session.
//! - `Email { from_instance, to }` →
//!   `plugin.outbound.email.<from_instance>` carrying `to` +
//!   `summary`. The framework email plugin subscribes here and
//!   sends via the operator's SMTP.
//! - `Disabled` → never published in the first place; this
//!   branch exists for defensive completeness when someone
//!   bypasses the publisher gate.
//!
//! ## Why baked-in instances?
//!
//! Plugins (Phase 81.5 subprocesses) cannot call admin RPC,
//! so they cannot fetch `agent.inbound_bindings` to resolve
//! "which WA instance does this agent use?" at notification
//! time. The frontend resolves it at seller-save time and
//! bakes the resolved instance string into
//! `seller.notification_settings.channel`. The publisher
//! ([`crate::notification::maybe_notify_lead_created`])
//! propagates the channel verbatim into the
//! `EmailNotification` payload; the forwarder reads from
//! there. Stale bindings (operator re-pairs WA after save)
//! require a seller re-save — the form surfaces the warning.

use nexo_microapp_sdk::plugin::BrokerSender;
use nexo_microapp_sdk::BrokerEvent;
use nexo_tool_meta::marketing::{EmailNotification, NotificationChannel};
use serde_json::json;

/// Discriminated outcome of one forwarder invocation. Pure
/// classification — actual `broker.publish` happens in the
/// broker hop with the live `BrokerSender`. Splitting keeps
/// unit tests pure (no broker mock needed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardOutcome {
    /// Topic doesn't match the forwarder prefix — caller's
    /// other dispatch arm (inbound email pipeline) should
    /// handle this event.
    Skipped,
    /// Payload didn't decode as `EmailNotification` — log
    /// + drop. Operator's broker may carry stale or
    /// foreign-source frames.
    Malformed,
    /// `Disabled` channel — defensive; publisher should have
    /// skipped, but if a foreign producer leaks one through,
    /// drop quietly.
    NoChannel,
    /// Resolved instance is empty — frontend forgot to
    /// resolve, or the binding was deleted. Logged as warn
    /// so the operator notices.
    UnresolvedInstance { kind: &'static str },
    /// Caller should `BrokerSender::publish(topic, event)`.
    Forward {
        topic: String,
        body: serde_json::Value,
    },
}

/// Static outbound message envelope. Mirrors the framework's
/// `OutboundCommand` shape loosely — kept small so the
/// forwarder doesn't depend on the email/wabridge crates.
fn whatsapp_outbound_body(notif: &EmailNotification) -> serde_json::Value {
    json!({
        "kind": "send_text",
        "to": format!("agent:{}", notif.agent_id),
        "body": notif.summary,
        "metadata": {
            "source": "marketing.notification",
            "lead_id": notif.lead_id.0,
            "seller_id": notif.seller_id.0,
        }
    })
}

fn email_outbound_body(notif: &EmailNotification, to: &str) -> serde_json::Value {
    json!({
        "kind": "send",
        "from_instance": match &notif.channel {
            NotificationChannel::Email { from_instance, .. } => from_instance.as_str(),
            _ => "",
        },
        "to": [to],
        "subject": format!("[Marketing] {}", notif.subject),
        "body_text": notif.summary,
        "metadata": {
            "source": "marketing.notification",
            "lead_id": notif.lead_id.0,
            "seller_id": notif.seller_id.0,
        }
    })
}

/// Decode the broker payload into `EmailNotification` then
/// classify the routing decision.
pub fn classify_forward(topic: &str, payload: &serde_json::Value) -> ForwardOutcome {
    if !topic.starts_with("agent.email.notification.")
        && topic != "agent.email.notification"
    {
        return ForwardOutcome::Skipped;
    }
    let notif: EmailNotification = match serde_json::from_value(payload.clone()) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(
                target: "extension.marketing.forwarder",
                topic, error = %e,
                "agent.email.notification payload failed decode"
            );
            return ForwardOutcome::Malformed;
        }
    };
    match &notif.channel {
        NotificationChannel::Disabled => ForwardOutcome::NoChannel,
        NotificationChannel::Whatsapp { instance } => {
            if instance.is_empty() {
                tracing::warn!(
                    target: "extension.marketing.forwarder",
                    agent_id = %notif.agent_id,
                    seller_id = %notif.seller_id.0,
                    "Whatsapp channel has empty instance — frontend did not resolve binding before save"
                );
                return ForwardOutcome::UnresolvedInstance { kind: "whatsapp" };
            }
            ForwardOutcome::Forward {
                topic: format!("plugin.outbound.whatsapp.{instance}"),
                body: whatsapp_outbound_body(&notif),
            }
        }
        NotificationChannel::Email { from_instance, to } => {
            if from_instance.is_empty() || to.is_empty() {
                tracing::warn!(
                    target: "extension.marketing.forwarder",
                    agent_id = %notif.agent_id,
                    from_instance = %from_instance,
                    to_empty = to.is_empty(),
                    "Email channel has unresolved fields"
                );
                return ForwardOutcome::UnresolvedInstance { kind: "email" };
            }
            ForwardOutcome::Forward {
                topic: format!("plugin.outbound.email.{from_instance}"),
                body: email_outbound_body(&notif, to),
            }
        }
    }
}

/// Drive the `ForwardOutcome` through `BrokerSender::publish`.
/// Failures log warn — never bubble (we don't want a transient
/// daemon hiccup to crash the broker hop). Returns the same
/// outcome for caller observability.
pub async fn handle_notification_event(
    topic: &str,
    payload: serde_json::Value,
    broker: &BrokerSender,
) -> ForwardOutcome {
    let outcome = classify_forward(topic, &payload);
    if let ForwardOutcome::Forward {
        topic: out_topic,
        body,
    } = &outcome
    {
        let event =
            BrokerEvent::new(out_topic.clone(), "marketing.forwarder", body.clone());
        if let Err(e) = broker.publish(out_topic, event).await {
            tracing::warn!(
                target: "extension.marketing.forwarder",
                error = %e,
                topic = %out_topic,
                "outbound publish failed (non-fatal)"
            );
        } else {
            tracing::info!(
                target: "extension.marketing.forwarder",
                topic = %out_topic,
                "notification forwarded"
            );
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    use nexo_tool_meta::marketing::{
        EmailNotificationKind, LeadId, TenantIdRef, SellerId,
    };

    fn notif(channel: NotificationChannel) -> EmailNotification {
        EmailNotification {
            kind: EmailNotificationKind::LeadCreated,
            tenant_id: TenantIdRef("acme".into()),
            agent_id: "pedro-agent".into(),
            lead_id: LeadId("l-1".into()),
            seller_id: SellerId("pedro".into()),
            seller_email: "pedro@acme.com".into(),
            from_email: "cliente@empresa.com".into(),
            subject: "Cotización CRM".into(),
            at_ms: 1_700_000_000_000,
            summary: "📧 Nuevo lead de Cliente\nAsunto: Cotización CRM".into(),
            channel,
        }
    }

    fn payload(n: &EmailNotification) -> serde_json::Value {
        serde_json::to_value(n).unwrap()
    }

    #[test]
    fn off_topic_event_is_skipped() {
        let n = notif(NotificationChannel::Whatsapp {
            instance: "personal".into(),
        });
        let out = classify_forward("plugin.outbound.whatsapp.foo", &payload(&n));
        assert_eq!(out, ForwardOutcome::Skipped);
    }

    #[test]
    fn malformed_payload_returns_malformed() {
        let out = classify_forward(
            "agent.email.notification.pedro-agent",
            &json!({ "totally": "wrong" }),
        );
        assert_eq!(out, ForwardOutcome::Malformed);
    }

    #[test]
    fn whatsapp_with_resolved_instance_routes_to_outbound() {
        let n = notif(NotificationChannel::Whatsapp {
            instance: "personal".into(),
        });
        let out = classify_forward(
            "agent.email.notification.pedro-agent",
            &payload(&n),
        );
        match out {
            ForwardOutcome::Forward { topic, body } => {
                assert_eq!(topic, "plugin.outbound.whatsapp.personal");
                assert_eq!(body["kind"], "send_text");
                assert!(body["body"]
                    .as_str()
                    .unwrap()
                    .contains("Nuevo lead"));
                assert_eq!(body["metadata"]["lead_id"], "l-1");
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[test]
    fn whatsapp_with_empty_instance_short_circuits() {
        let n = notif(NotificationChannel::Whatsapp {
            instance: String::new(),
        });
        let out = classify_forward(
            "agent.email.notification.pedro-agent",
            &payload(&n),
        );
        assert!(matches!(
            out,
            ForwardOutcome::UnresolvedInstance { kind: "whatsapp" }
        ));
    }

    #[test]
    fn email_with_resolved_fields_routes_to_outbound() {
        let n = notif(NotificationChannel::Email {
            from_instance: "ventas-acme".into(),
            to: "ops@acme.com".into(),
        });
        let out = classify_forward(
            "agent.email.notification.pedro-agent",
            &payload(&n),
        );
        match out {
            ForwardOutcome::Forward { topic, body } => {
                assert_eq!(topic, "plugin.outbound.email.ventas-acme");
                assert_eq!(body["kind"], "send");
                assert_eq!(body["from_instance"], "ventas-acme");
                assert_eq!(body["to"], json!(["ops@acme.com"]));
                assert!(body["subject"]
                    .as_str()
                    .unwrap()
                    .starts_with("[Marketing]"));
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[test]
    fn email_with_empty_to_short_circuits() {
        let n = notif(NotificationChannel::Email {
            from_instance: "ventas-acme".into(),
            to: String::new(),
        });
        let out = classify_forward(
            "agent.email.notification.pedro-agent",
            &payload(&n),
        );
        assert!(matches!(
            out,
            ForwardOutcome::UnresolvedInstance { kind: "email" }
        ));
    }

    #[test]
    fn disabled_channel_returns_no_channel() {
        let n = notif(NotificationChannel::Disabled);
        let out = classify_forward(
            "agent.email.notification.pedro-agent",
            &payload(&n),
        );
        assert_eq!(out, ForwardOutcome::NoChannel);
    }

    #[test]
    fn topic_with_dotted_agent_id_still_matches() {
        // Some operators use kebab-case-with.dots in agent ids
        // — verify the prefix match doesn't choke.
        let n = notif(NotificationChannel::Whatsapp {
            instance: "personal".into(),
        });
        let out = classify_forward(
            "agent.email.notification.acme.sales.pedro",
            &payload(&n),
        );
        assert!(matches!(out, ForwardOutcome::Forward { .. }));
    }

    #[test]
    fn bare_topic_without_suffix_also_matches() {
        // Defensive — if some publisher sends to the un-
        // suffixed root topic, the forwarder still tries.
        let n = notif(NotificationChannel::Whatsapp {
            instance: "personal".into(),
        });
        let out = classify_forward("agent.email.notification", &payload(&n));
        assert!(matches!(out, ForwardOutcome::Forward { .. }));
    }
}
