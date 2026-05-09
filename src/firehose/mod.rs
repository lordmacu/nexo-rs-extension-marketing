//! In-process broadcast bus for lead lifecycle events.
//!
//! **F29 sweep:** marketing-specific by design.
//! [`LeadFirehoseEvent`] carries CRM-shaped variants
//! (`Created` / `ThreadBumped` / `Transitioned` /
//! `FollowupOverridden`) keyed off `LeadId`. The
//! `tokio::broadcast` glue around them is 30 LOC of bus
//! plumbing — already mirrored by the SDK's `events` feature
//! (which the audit log consumes). Lifting this would
//! duplicate the SDK's primitive without a second consumer.
//!
//! Producers (`crate::plugin::broker`) publish typed
//! [`LeadFirehoseEvent`] values whenever they touch a lead;
//! consumers (`crate::admin::firehose` SSE endpoint) subscribe
//! through `LeadEventBus::subscribe` and stream filtered events
//! to the operator UI.
//!
//! Tenant scoping happens at the consumer: every event carries
//! a `tenant_id`, and the SSE handler only forwards frames
//! whose tenant matches the subscriber's `X-Tenant-Id`. The bus
//! itself is single-channel — multi-tenant deployments share
//! one broadcast (filtering is cheap and the volume is operator
//! scale, not consumer scale).

use serde::Serialize;
use tokio::sync::broadcast;

use nexo_tool_meta::marketing::{LeadId, LeadState, TenantIdRef};

/// Broadcast channel buffer. 256 frames ≈ a small burst before
/// slow subscribers see `Lagged(n)`. Operator UIs reconcile via
/// REST polling on lagged frames so this doesn't need to be
/// huge.
const BUFFER: usize = 256;

/// Typed lead lifecycle frame. Mirrors the wire shape the
/// operator UI consumes via SSE — keep it stable across
/// rolling deploys.
///
/// `kind` discriminates the variant for readers that don't
/// pattern-match (e.g. JS clients): `"created"` /
/// `"thread_bumped"` / `"transitioned"`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeadFirehoseEvent {
    /// New lead written to the per-tenant store. Carries enough
    /// data for the UI to insert into its list without an extra
    /// REST round-trip.
    Created {
        tenant_id: TenantIdRef,
        lead_id: LeadId,
        thread_id: String,
        subject: String,
        from_email: String,
        seller_id: String,
        state: LeadState,
        at_ms: i64,
        why_routed: Vec<String>,
    },
    /// Subsequent inbound on an already-tracked thread —
    /// `last_activity_ms` should bump on the UI side.
    ThreadBumped {
        tenant_id: TenantIdRef,
        lead_id: LeadId,
        thread_id: String,
        at_ms: i64,
    },
    /// State machine transition (cold → engaged → meeting →
    /// qualified | lost). `at_ms` is the wall-clock the
    /// transition was committed in the lead store.
    Transitioned {
        tenant_id: TenantIdRef,
        lead_id: LeadId,
        from: LeadState,
        to: LeadState,
        at_ms: i64,
        reason: String,
    },
    /// M15.21.followup-override — operator-driven bypass of
    /// the followup cadence. `action` is `"skip"` (cancels the
    /// pending iteration) or `"postpone"` (bumps
    /// `next_check_at_ms` to a specific timestamp).
    /// `next_check_at_ms` carries the new value (`None` after
    /// skip; `Some(ms)` after postpone) so the UI can refresh
    /// its drawer without an extra REST round-trip.
    FollowupOverridden {
        tenant_id: TenantIdRef,
        lead_id: LeadId,
        action: String,
        next_check_at_ms: Option<i64>,
        reason: String,
        at_ms: i64,
    },
}

impl LeadFirehoseEvent {
    /// Tenant id of the event, used by the SSE filter.
    pub fn tenant_id(&self) -> &str {
        match self {
            Self::Created { tenant_id, .. }
            | Self::ThreadBumped { tenant_id, .. }
            | Self::Transitioned { tenant_id, .. }
            | Self::FollowupOverridden { tenant_id, .. } => &tenant_id.0,
        }
    }
}

/// In-memory broadcast bus shared across the broker handler +
/// the SSE endpoint. `Clone` is cheap (`broadcast::Sender` is
/// `Arc`-shared); each subscriber gets its own
/// `broadcast::Receiver` via [`LeadEventBus::subscribe`].
#[derive(Clone)]
pub struct LeadEventBus {
    tx: broadcast::Sender<LeadFirehoseEvent>,
}

impl Default for LeadEventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl LeadEventBus {
    /// Build a bus with the default buffer size (256 frames).
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BUFFER);
        Self { tx }
    }

    /// Build a bus with a custom buffer size — exposed for
    /// tests that want a tight buffer to reproduce the lagged
    /// path.
    pub fn with_buffer(buffer: usize) -> Self {
        let (tx, _rx) = broadcast::channel(buffer.max(1));
        Self { tx }
    }

    /// Publish a frame. Failure means there are zero
    /// subscribers — that's fine (no operator dashboard
    /// connected); we drop silently.
    pub fn publish(&self, event: LeadFirehoseEvent) {
        let _ = self.tx.send(event);
    }

    /// Subscribe and receive frames from this point forward.
    pub fn subscribe(&self) -> broadcast::Receiver<LeadFirehoseEvent> {
        self.tx.subscribe()
    }

    /// How many subscribers are currently attached. Useful for
    /// observability + tests.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev_created(tenant: &str, lead: &str) -> LeadFirehoseEvent {
        LeadFirehoseEvent::Created {
            tenant_id: TenantIdRef(tenant.into()),
            lead_id: LeadId(lead.into()),
            thread_id: "th-1".into(),
            subject: "Demo".into(),
            from_email: "x@y.com".into(),
            seller_id: "unassigned".into(),
            state: LeadState::Cold,
            at_ms: 0,
            why_routed: vec![],
        }
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_silent() {
        let bus = LeadEventBus::new();
        // Should not panic, should not error — operator
        // dashboard not connected → frame quietly dropped.
        bus.publish(ev_created("acme", "l1"));
    }

    #[tokio::test]
    async fn subscriber_receives_published_frame() {
        let bus = LeadEventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(ev_created("acme", "l1"));
        let got = rx.recv().await.unwrap();
        assert_eq!(got.tenant_id(), "acme");
    }

    #[tokio::test]
    async fn tenant_id_accessor_covers_every_variant() {
        let created = ev_created("acme", "l1");
        let bumped = LeadFirehoseEvent::ThreadBumped {
            tenant_id: TenantIdRef("acme".into()),
            lead_id: LeadId("l1".into()),
            thread_id: "th-1".into(),
            at_ms: 0,
        };
        let trans = LeadFirehoseEvent::Transitioned {
            tenant_id: TenantIdRef("acme".into()),
            lead_id: LeadId("l1".into()),
            from: LeadState::Cold,
            to: LeadState::Engaged,
            at_ms: 0,
            reason: "manual".into(),
        };
        assert_eq!(created.tenant_id(), "acme");
        assert_eq!(bumped.tenant_id(), "acme");
        assert_eq!(trans.tenant_id(), "acme");
    }

    #[tokio::test]
    async fn receiver_count_reports_subscribers() {
        let bus = LeadEventBus::new();
        assert_eq!(bus.receiver_count(), 0);
        let _r1 = bus.subscribe();
        assert_eq!(bus.receiver_count(), 1);
        let _r2 = bus.subscribe();
        assert_eq!(bus.receiver_count(), 2);
        drop(_r1);
        assert_eq!(bus.receiver_count(), 1);
    }

    #[tokio::test]
    async fn small_buffer_yields_lagged_for_slow_subscriber() {
        let bus = LeadEventBus::with_buffer(2);
        let mut rx = bus.subscribe();
        for i in 0..6 {
            bus.publish(ev_created("acme", &format!("l{i}")));
        }
        // First recv on a buffered-out subscriber should be
        // Lagged so the SSE stream can surface a `lagged` frame.
        let err = rx.recv().await.unwrap_err();
        assert!(
            matches!(err, tokio::sync::broadcast::error::RecvError::Lagged(_)),
            "expected Lagged, got {err:?}"
        );
    }

    #[test]
    fn json_carries_kind_discriminator() {
        let frame = ev_created("acme", "l1");
        let s = serde_json::to_string(&frame).unwrap();
        assert!(s.contains(r#""kind":"created""#));
        assert!(s.contains(r#""tenant_id":"acme""#));
    }
}
