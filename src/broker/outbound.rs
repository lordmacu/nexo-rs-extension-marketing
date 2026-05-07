//! Outbound publisher.
//!
//! Composes the compliance gate (M15.23) + idempotency
//! (thread_id, draft_id) + the email plugin's
//! `OutboundCommand` shape, returning `OutboundDispatchResult`
//! the binary publishes to `plugin.outbound.email.<instance>`.
//!
//! Pure (no async-nats client here) so unit tests cover every
//! branch — the binary's broker layer wraps this with the
//! actual `Broker::publish` call.

use std::collections::HashSet;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::compliance::{Decision, OutboundGate};
use crate::tenant::TenantId;

/// What to publish to `plugin.outbound.email.<instance>`.
/// Mirrors the email plugin's `OutboundCommand` (we don't
/// re-export the type to avoid a heavyweight cargo dep on
/// `nexo-plugin-email`; the binary serialises this struct
/// into the wire shape directly via serde-rename).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundEmail {
    pub to: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cc: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
}

/// Per-call inputs the publisher needs to compose the
/// command + audit row.
#[derive(Debug, Clone)]
pub struct DispatchInput {
    pub thread_id: String,
    pub draft_id: String,
    pub vendedor_smtp_instance: String,
    pub email: OutboundEmail,
}

/// What the publisher returns. The binary's broker layer
/// reads `Publish` and calls `Broker::publish(topic, payload)`.
/// `Skipped` short-circuits when an idempotent retry hit a
/// previously-sent draft. `Blocked` carries the gate's reason
/// for the caller's audit trail.
#[derive(Debug, Clone)]
pub enum OutboundDispatchResult {
    Publish {
        topic: String,
        command: OutboundEmail,
        idempotency_key: String,
    },
    Skipped {
        idempotency_key: String,
        reason: String,
    },
    Blocked {
        idempotency_key: String,
        reason: String,
    },
}

/// Per-tenant outbound publisher. Holds the compliance gate
/// + an in-memory set of already-dispatched
/// `(thread_id, draft_id)` keys so a double-approve from the
/// operator doesn't double-send.
pub struct OutboundPublisher {
    tenant_id: TenantId,
    gate: OutboundGate,
    dispatched: Mutex<HashSet<String>>,
}

impl OutboundPublisher {
    pub fn new(tenant_id: TenantId, gate: OutboundGate) -> Self {
        Self {
            tenant_id,
            gate,
            dispatched: Mutex::new(HashSet::new()),
        }
    }

    pub fn tenant_id(&self) -> &TenantId {
        &self.tenant_id
    }

    /// Compose the outbound. Caller publishes the returned
    /// `Publish.command` to `topic` via async-nats.
    ///
    /// `recipient_for_rate_limit` is normally the first `to`
    /// address; the gate uses it as the rate-limit bucket so
    /// the same lead's followup chain doesn't burst.
    pub fn dispatch(&self, input: DispatchInput) -> OutboundDispatchResult {
        let idempotency_key = format!("{}:{}", input.thread_id, input.draft_id);

        // Idempotency — short-circuit a double-approve.
        {
            let mut seen = self.dispatched.lock().unwrap();
            if seen.contains(&idempotency_key) {
                return OutboundDispatchResult::Skipped {
                    idempotency_key,
                    reason: "already_dispatched".into(),
                };
            }
            // Reserve the slot now; release on Block so the
            // operator can re-approve a fixed body.
            seen.insert(idempotency_key.clone());
        }

        let recipient = input
            .email
            .to
            .first()
            .cloned()
            .unwrap_or_default();
        if recipient.is_empty() {
            // Release the reservation so the caller can retry
            // with a corrected payload.
            self.dispatched.lock().unwrap().remove(&idempotency_key);
            return OutboundDispatchResult::Blocked {
                idempotency_key,
                reason: "recipient_empty".into(),
            };
        }

        let mut email = input.email.clone();
        match self.gate.check(&recipient, &email.body) {
            Decision::Send => {}
            Decision::ModifyAndSend { body, .. } => {
                email.body = body;
            }
            Decision::Block { reason } => {
                self.dispatched.lock().unwrap().remove(&idempotency_key);
                return OutboundDispatchResult::Blocked {
                    idempotency_key,
                    reason,
                };
            }
        }

        let topic = format!("plugin.outbound.email.{}", input.vendedor_smtp_instance);
        OutboundDispatchResult::Publish {
            topic,
            command: email,
            idempotency_key,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(thread: &str, draft: &str, body: &str) -> DispatchInput {
        DispatchInput {
            thread_id: thread.into(),
            draft_id: draft.into(),
            vendedor_smtp_instance: "acme-pedro".into(),
            email: OutboundEmail {
                to: vec!["juan@acme.com".into()],
                cc: vec![],
                bcc: vec![],
                subject: "Re: cotización".into(),
                body: body.into(),
                in_reply_to: Some("abc-001@acme.com".into()),
                references: vec!["abc-001@acme.com".into()],
            },
        }
    }

    fn fresh() -> OutboundPublisher {
        OutboundPublisher::new(
            TenantId::new("acme").unwrap(),
            OutboundGate::with_defaults(),
        )
    }

    #[test]
    fn happy_path_publishes() {
        let p = fresh();
        let r = p.dispatch(input("th-1", "d-1", "Hola Juan, te confirmo el martes."));
        match r {
            OutboundDispatchResult::Publish {
                topic,
                command,
                idempotency_key,
            } => {
                assert_eq!(topic, "plugin.outbound.email.acme-pedro");
                assert_eq!(idempotency_key, "th-1:d-1");
                assert_eq!(command.to, vec!["juan@acme.com".to_string()]);
            }
            other => panic!("expected Publish, got {other:?}"),
        }
    }

    #[test]
    fn double_approve_returns_skipped_second_time() {
        let p = fresh();
        let _ = p.dispatch(input("th-1", "d-1", "Hello once."));
        let r = p.dispatch(input("th-1", "d-1", "Hello once."));
        match r {
            OutboundDispatchResult::Skipped { reason, .. } => {
                assert_eq!(reason, "already_dispatched");
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn empty_recipient_blocks_and_releases_reservation() {
        let p = fresh();
        let mut i = input("th-1", "d-1", "body");
        i.email.to.clear();
        let r = p.dispatch(i.clone());
        assert!(matches!(r, OutboundDispatchResult::Blocked { .. }));
        // Reservation released → retry with a fixed payload
        // succeeds.
        i.email.to = vec!["juan@acme.com".into()];
        let r2 = p.dispatch(i);
        assert!(matches!(r2, OutboundDispatchResult::Publish { .. }));
    }

    #[test]
    fn pii_in_body_publishes_redacted() {
        let p = fresh();
        let r = p.dispatch(input(
            "th-2",
            "d-2",
            "Llamame al +57 311 555 5555 cuando puedas.",
        ));
        match r {
            OutboundDispatchResult::Publish { command, .. } => {
                assert!(!command.body.contains("311 555 5555"));
            }
            other => panic!("expected Publish (modified), got {other:?}"),
        }
    }

    #[test]
    fn opt_out_in_outbound_blocks_and_releases_reservation() {
        let p = fresh();
        let r = p.dispatch(input(
            "th-3",
            "d-3",
            "Click unsubscribe to stop receiving emails.",
        ));
        match r {
            OutboundDispatchResult::Blocked { reason, .. } => {
                assert!(reason.starts_with("opt_out:"));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
        // Re-dispatch with a fixed body succeeds — reservation
        // got released.
        let r2 = p.dispatch(input("th-3", "d-3", "Hola, te confirmo el martes."));
        assert!(matches!(r2, OutboundDispatchResult::Publish { .. }));
    }

    #[test]
    fn anti_loop_blocks_third_identical_body() {
        let p = fresh();
        // Different draft ids so idempotency doesn't fire — we
        // want anti-loop to be the trigger.
        for i in 0..2 {
            let r = p.dispatch(input("th-4", &format!("d-{i}"), "exact same body"));
            assert!(matches!(r, OutboundDispatchResult::Publish { .. }));
        }
        let r = p.dispatch(input("th-4", "d-2", "exact same body"));
        match r {
            OutboundDispatchResult::Blocked { reason, .. } => {
                assert!(reason.starts_with("anti_loop:"));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }
}
