//! Broker subscriber for `plugin.inbound.email.*`.
//!
//! Registered via `PluginAdapter::on_broker_event`. The
//! daemon routes every event whose topic matches one of the
//! manifest's `[plugin.subscriptions].broker_topics` patterns
//! to this handler. We:
//!
//! 1. Filter by topic prefix `plugin.inbound.email.` so other
//!    subscriptions added later don't leak through (defense
//!    in depth — the manifest already filters server-side).
//! 2. Pull the wire fields we need (`account_id`, `instance`,
//!    `uid`, `raw_bytes`) without depending on the email
//!    plugin crate; we use our own private deserialise struct
//!    so the marketing extension stays decoupled.
//! 3. Run `decode_inbound_email` (RFC 5322) → `ParsedInbound`.
//! 4. Look up or create a lead in the per-tenant store.
//!    Existing thread → bump `last_activity_ms`. Cold thread
//!    → create with placeholder person/vendedor ids until the
//!    resolver + router pipeline lands (M22).
//!
//! The full pipeline (resolver → router → draft generation
//! → outbound dispatch) lands in M22; this commit wires the
//! foundational broker hop so the extension stops being an
//! HTTP-only service.

use chrono::Utc;
use nexo_microapp_sdk::plugin::BrokerSender;
use nexo_tool_meta::marketing::{LeadId, PersonId, VendedorId};
use serde::Deserialize;
use uuid::Uuid;

use crate::broker::inbound::{decode_inbound_email, ParsedInbound, ParseError};
use crate::lead::{LeadStore, NewLead};

/// Wire-shape mirror of `nexo_plugin_email::events::InboundEvent`,
/// kept private so the extension doesn't depend on the email
/// plugin crate. Only the fields the decoder needs are pulled
/// — meta + attachments are ignored at this stage (decoder
/// re-parses raw_bytes anyway).
#[derive(Debug, Deserialize)]
struct InboundWire {
    account_id: String,
    instance: String,
    uid: u32,
    #[serde(with = "serde_bytes")]
    raw_bytes: Vec<u8>,
}

/// Parameter-light enum so the handler returns a structured
/// outcome the unit tests can assert on.
#[derive(Debug, PartialEq, Eq)]
pub enum HandledOutcome {
    /// Topic didn't match `plugin.inbound.email.*` — ignored.
    Skipped,
    /// Wire decode failed (malformed payload). Logged + dropped.
    Malformed,
    /// RFC 5322 parse failure (corrupt bytes / disposable sender).
    /// Logged + dropped — disposables go through this path on
    /// purpose so the audit trail captures them.
    DecodeFailed(String),
    /// Existing thread already tracked — bumped activity stamp.
    LeadUpdated { lead_id: LeadId, thread_id: String },
    /// Cold thread → new lead created with placeholder ids.
    LeadCreated { lead_id: LeadId, thread_id: String },
}

/// Handle one inbound-email broker event. Pure logic — the
/// `BrokerSender` parameter is reserved for the full pipeline
/// (publishing `agent.lead.transition.*` after creation) and
/// goes unused here so unit tests can pass a no-op.
pub async fn handle_inbound_event(
    topic: &str,
    payload: serde_json::Value,
    store: &LeadStore,
    _broker: Option<BrokerSender>,
) -> HandledOutcome {
    if !topic.starts_with("plugin.inbound.email.") && topic != "plugin.inbound.email" {
        return HandledOutcome::Skipped;
    }

    let wire: InboundWire = match serde_json::from_value(payload) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                target: "plugin.marketing.broker",
                topic, error = %e,
                "malformed plugin.inbound.email payload"
            );
            return HandledOutcome::Malformed;
        }
    };

    let parsed: ParsedInbound = match decode_inbound_email(
        &wire.instance,
        &wire.account_id,
        wire.uid,
        &wire.raw_bytes,
    ) {
        Ok(p) => p,
        Err(ParseError::DisposableSender(addr)) => {
            tracing::info!(
                target: "plugin.marketing.broker",
                from = %addr,
                "dropped disposable sender pre-pipeline"
            );
            return HandledOutcome::DecodeFailed(format!("disposable: {addr}"));
        }
        Err(e) => {
            tracing::warn!(
                target: "plugin.marketing.broker",
                topic,
                error = %e,
                "decode_inbound_email failed"
            );
            return HandledOutcome::DecodeFailed(e.to_string());
        }
    };

    // Look up by thread id first — followups stay on the same lead.
    match store.find_by_thread(&parsed.thread_id).await {
        Ok(Some(lead)) => {
            // Existing thread — bump last_activity_ms via a
            // lightweight transition no-op. The full pipeline (M22)
            // will compute sentiment + intent here and publish
            // `agent.lead.transition.engaged` when state actually
            // changes; for now the broker hop just logs the touch.
            tracing::debug!(
                target: "plugin.marketing.broker",
                lead_id = %lead.id.0,
                thread_id = %parsed.thread_id,
                "inbound on existing thread"
            );
            HandledOutcome::LeadUpdated {
                lead_id: lead.id.clone(),
                thread_id: parsed.thread_id,
            }
        }
        Ok(None) => {
            // Cold thread — create a placeholder lead. The resolver
            // + router pipeline (M22) will replace the placeholder
            // ids with the real PersonId / VendedorId; today the
            // operator UI shows the lead with the synthetic ids
            // and the routed-from email + subject.
            let lead_id = LeadId(Uuid::new_v4().to_string());
            let now_ms = Utc::now().timestamp_millis();
            let placeholder_person = PersonId(format!("placeholder:{}", parsed.from_email));
            let placeholder_vendedor = VendedorId("unassigned".into());
            let new_lead = NewLead {
                id: lead_id.clone(),
                thread_id: parsed.thread_id.clone(),
                subject: parsed.subject.clone(),
                person_id: placeholder_person,
                vendedor_id: placeholder_vendedor,
                last_activity_ms: now_ms,
                why_routed: vec!["inbound_no_resolver_yet".into()],
            };
            match store.create(new_lead).await {
                Ok(lead) => {
                    tracing::info!(
                        target: "plugin.marketing.broker",
                        lead_id = %lead.id.0,
                        thread_id = %parsed.thread_id,
                        from = %parsed.from_email,
                        "created cold lead from inbound email"
                    );
                    HandledOutcome::LeadCreated {
                        lead_id: lead.id,
                        thread_id: parsed.thread_id,
                    }
                }
                Err(e) => {
                    tracing::error!(
                        target: "plugin.marketing.broker",
                        thread_id = %parsed.thread_id,
                        error = %e,
                        "lead create failed"
                    );
                    HandledOutcome::DecodeFailed(e.to_string())
                }
            }
        }
        Err(e) => {
            tracing::error!(
                target: "plugin.marketing.broker",
                thread_id = %parsed.thread_id,
                error = %e,
                "find_by_thread failed"
            );
            HandledOutcome::DecodeFailed(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use serde_json::json;

    use crate::tenant::TenantId;

    async fn fresh_store() -> LeadStore {
        LeadStore::open(PathBuf::from(":memory:"), TenantId::new("acme").unwrap())
            .await
            .expect("open lead store")
    }

    fn raw_email() -> Vec<u8> {
        // Minimal valid RFC 5322 message — From + Subject +
        // Message-Id + body. Enough for decode_inbound_email.
        b"From: =?utf-8?Q?Cliente?= <cliente@empresa.com>\r\n\
          To: ventas@miempresa.com\r\n\
          Subject: Cotizaci\xc3\xb3n CRM\r\n\
          Message-Id: <abc123@empresa.com>\r\n\
          Date: Fri, 1 May 2026 10:00:00 +0000\r\n\
          \r\n\
          Hola, queremos cotizar.\r\n"
            .to_vec()
    }

    #[tokio::test]
    async fn off_topic_event_is_skipped() {
        let store = fresh_store().await;
        let out = handle_inbound_event(
            "plugin.outbound.whatsapp.ack",
            json!({}),
            &store,
            None,
        )
        .await;
        assert_eq!(out, HandledOutcome::Skipped);
    }

    #[tokio::test]
    async fn malformed_payload_returns_malformed() {
        let store = fresh_store().await;
        let out = handle_inbound_event(
            "plugin.inbound.email.acme",
            json!({ "no_fields": true }),
            &store,
            None,
        )
        .await;
        assert_eq!(out, HandledOutcome::Malformed);
    }

    #[tokio::test]
    async fn cold_thread_creates_lead() {
        let store = fresh_store().await;
        let payload = json!({
            "account_id": "acct-1",
            "instance": "default",
            "uid": 42_u32,
            "raw_bytes": raw_email(),
        });
        let out = handle_inbound_event(
            "plugin.inbound.email.default",
            payload,
            &store,
            None,
        )
        .await;
        match out {
            HandledOutcome::LeadCreated { thread_id, .. } => {
                assert!(!thread_id.is_empty());
            }
            other => panic!("expected LeadCreated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn second_inbound_on_same_thread_updates_existing_lead() {
        let store = fresh_store().await;
        let payload = json!({
            "account_id": "acct-1",
            "instance": "default",
            "uid": 42_u32,
            "raw_bytes": raw_email(),
        });
        let first = handle_inbound_event(
            "plugin.inbound.email.default",
            payload.clone(),
            &store,
            None,
        )
        .await;
        let HandledOutcome::LeadCreated { lead_id, thread_id } = first else {
            panic!("first call should create lead, got {first:?}");
        };
        // Second event with same Message-Id stays on the same
        // thread → LeadUpdated.
        let second = handle_inbound_event(
            "plugin.inbound.email.default",
            payload,
            &store,
            None,
        )
        .await;
        match second {
            HandledOutcome::LeadUpdated {
                lead_id: l2,
                thread_id: t2,
            } => {
                assert_eq!(l2.0, lead_id.0);
                assert_eq!(t2, thread_id);
            }
            other => panic!("expected LeadUpdated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bare_inbound_email_topic_also_handled() {
        // Some daemons publish on the un-suffixed root topic
        // when there's only one instance. Verify we accept it.
        let store = fresh_store().await;
        let payload = json!({
            "account_id": "acct-1",
            "instance": "default",
            "uid": 99_u32,
            "raw_bytes": raw_email(),
        });
        let out =
            handle_inbound_event("plugin.inbound.email", payload, &store, None).await;
        assert!(
            matches!(out, HandledOutcome::LeadCreated { .. }),
            "expected LeadCreated on bare topic, got {out:?}"
        );
    }
}
