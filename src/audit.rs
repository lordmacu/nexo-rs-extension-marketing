//! AI-decision audit log (M15.23.c).
//!
//! **F29 sweep:** marketing-specific by design.
//! `AuditEvent` carries 5 CRM-shaped variants
//! (`RoutingDecided` / `LeadTransitioned` /
//! `NotificationPublished` / `TopicGuardrailFired` /
//! `DuplicatePersonDetected`). The wrapper around the
//! generic `EventStore<T>` (already in SDK) is 60 LOC of
//! glue that doesn't justify a generic lift.
//!
//! Persists every routing decision, every state transition,
//! every operator-notification publish to a tenant-scoped
//! sqlite log. Compliance officer reads it through
//! `GET /audit?lead_id=&kind=&since_ms=&limit=` to validate
//! the AI didn't make decisions outside the operator-
//! configured envelope.
//!
//! ## Storage
//!
//! Reuses the SDK's `events` feature — every audit row lands
//! in `EventStore<AuditEvent>` exactly the way the firehose
//! lifecycle log does. One table (`marketing_audit_events`)
//! per tenant, scoped through the `tenant_id` column. The
//! same `EventBroadcastState` pattern can subscribe a live
//! SSE feed later if compliance asks for one — the firehose
//! infra handles it.
//!
//! ## Producers
//!
//! - **Broker hop** `lead create` branch records
//!   [`AuditEvent::RoutingDecided`] every time a seller is
//!   chosen (or dropped, or fallen back).
//! - **State-transition tools** record
//!   [`AuditEvent::LeadTransitioned`] post-success.
//! - **Notification publish path** records
//!   [`AuditEvent::NotificationPublished`] for each
//!   broker-hop / tool publish.
//!
//! Every producer threads `Option<&AuditLog>` so tests +
//! minimal setups can skip the recorder without ceremony.

use std::sync::Arc;

use nexo_microapp_sdk::events::{EventMetadata, EventStore, ListFilter};
use nexo_microapp_sdk::scoring::ScoreReason;
use nexo_tool_meta::marketing::NotificationChannel;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Table name the audit log writes to. Distinct from the
/// firehose default so the two stores can co-exist on the
/// same SQLite file (different tables, same connection pool).
pub const AUDIT_TABLE: &str = "marketing_audit_events";

/// One audit row. `tag = "kind"` so JSON consumers (operator
/// UI, compliance export) discriminate without re-reading
/// the schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditEvent {
    /// Routing dispatcher chose a seller (or dropped the
    /// inbound, or fell through to `unassigned`). One row
    /// per inbound that reached the dispatcher.
    RoutingDecided {
        /// Tenant scope.
        tenant_id: String,
        /// Lead the dispatcher created (or would have).
        /// `None` when the rule fired `Drop` and no lead row
        /// exists.
        lead_id: Option<String>,
        /// Sender email for ops query convenience.
        from_email: String,
        /// Final seller assignment. `None` ⇒ `Drop` outcome.
        /// `Some(\"unassigned\")` ⇒ availability gate / no-
        /// target fallback.
        chosen_seller_id: Option<String>,
        /// Matched routing rule id, if any.
        rule_id: Option<String>,
        /// Full audit chain — same vector the operator UI
        /// renders in the lead drawer's "Por qué este lead
        /// llegó aquí" tooltip.
        why: Vec<String>,
        /// Heuristic score the lead landed with (0..=100).
        score: u8,
        /// Per-rule score contributions.
        score_reasons: Vec<ScoreReason>,
        /// Wall-clock at_ms.
        at_ms: u64,
    },
    /// State machine transition committed.
    LeadTransitioned {
        /// Tenant scope.
        tenant_id: String,
        /// Lead id.
        lead_id: String,
        /// `cold` | `engaged` | `meeting_scheduled` | `qualified` | `lost`.
        from: String,
        /// Same set as `from`.
        to: String,
        /// Operator-supplied reason.
        reason: String,
        /// Wall-clock at_ms.
        at_ms: u64,
    },
    /// One duplicate-person candidate surfaced for a lead's
    /// resolved person. Recorded once per `(lead, candidate)`
    /// pair so the operator's compliance view can render the
    /// merge-prompt history without scanning the live store.
    DuplicatePersonDetected {
        /// Tenant scope.
        tenant_id: String,
        /// Lead the candidate was matched against.
        lead_id: String,
        /// Candidate person id (the row that may be the
        /// same human as the lead's resolved person).
        candidate_person_id: String,
        /// Lead's resolved person id (\"this is who we
        /// thought the lead was — but maybe it's actually
        /// the candidate\").
        resolved_person_id: String,
        /// Stable signal label
        /// (\`email_match\` / \`phone_match\` /
        /// \`name_company_fuzzy\`).
        signal: String,
        /// 0.0..1.0 confidence band.
        confidence: f32,
        /// Operator-facing detail (\"matched email
        /// \\\`juan@globex.io\\\`\").
        detail: String,
        /// Wall-clock at_ms.
        at_ms: u64,
    },
    /// One topic guardrail fired against the inbound body.
    /// Recorded BEFORE the routing decision so the operator's
    /// timeline shows the guardrail tag adjacent to the
    /// `RoutingDecided` row.
    TopicGuardrailFired {
        /// Tenant scope.
        tenant_id: String,
        /// Lead the guardrail attached to. `None` when the
        /// inbound was dropped pre-create (resolver / rule
        /// drop) — operator still sees the row by querying
        /// `kind = topic_guardrail_fired`.
        lead_id: Option<String>,
        /// Sender email for triage convenience.
        from_email: String,
        /// Stable rule id from the operator's YAML.
        rule_id: String,
        /// Operator-facing rule name.
        rule_name: String,
        /// `force_approval` | `block`.
        action: String,
        /// ±30-char excerpt around the matched fragment.
        excerpt: String,
        /// Wall-clock at_ms.
        at_ms: u64,
    },
    /// Operator-targeted notification published to the
    /// broker (LeadCreated / LeadReplied / LeadTransitioned /
    /// MeetingIntent).
    NotificationPublished {
        /// Tenant scope.
        tenant_id: String,
        /// Lead id the notification fired for.
        lead_id: String,
        /// Seller routed to.
        seller_id: String,
        /// Notification kind label.
        notification_kind: String,
        /// Channel that handled the notification:
        /// `\"whatsapp\"` / `\"email\"` / `\"disabled\"` /
        /// `\"deduped\"`.
        channel: String,
        /// Wall-clock at_ms.
        at_ms: u64,
    },
}

impl EventMetadata for AuditEvent {
    fn kind(&self) -> &str {
        match self {
            Self::RoutingDecided { .. } => "routing_decided",
            Self::LeadTransitioned { .. } => "lead_transitioned",
            Self::NotificationPublished { .. } => "notification_published",
            Self::TopicGuardrailFired { .. } => "topic_guardrail_fired",
            Self::DuplicatePersonDetected { .. } => "duplicate_person_detected",
        }
    }

    fn agent_id(&self) -> &str {
        // The SDK's index is per-`agent_id` — for marketing
        // we partition on `lead_id` instead so the operator
        // can pull a lead's full history with a single query.
        // Empty when the audit row isn't lead-scoped (Drop
        // outcomes carry `None`).
        match self {
            Self::RoutingDecided { lead_id, .. } => lead_id.as_deref().unwrap_or(""),
            Self::LeadTransitioned { lead_id, .. } => lead_id.as_str(),
            Self::NotificationPublished { lead_id, .. } => lead_id.as_str(),
            Self::TopicGuardrailFired { lead_id, .. } => lead_id.as_deref().unwrap_or(""),
            Self::DuplicatePersonDetected { lead_id, .. } => lead_id.as_str(),
        }
    }

    fn tenant_id(&self) -> Option<&str> {
        Some(match self {
            Self::RoutingDecided { tenant_id, .. } => tenant_id.as_str(),
            Self::LeadTransitioned { tenant_id, .. } => tenant_id.as_str(),
            Self::NotificationPublished { tenant_id, .. } => tenant_id.as_str(),
            Self::TopicGuardrailFired { tenant_id, .. } => tenant_id.as_str(),
            Self::DuplicatePersonDetected { tenant_id, .. } => tenant_id.as_str(),
        })
    }

    fn at_ms(&self) -> u64 {
        match self {
            Self::RoutingDecided { at_ms, .. } => *at_ms,
            Self::LeadTransitioned { at_ms, .. } => *at_ms,
            Self::NotificationPublished { at_ms, .. } => *at_ms,
            Self::TopicGuardrailFired { at_ms, .. } => *at_ms,
            Self::DuplicatePersonDetected { at_ms, .. } => *at_ms,
        }
    }
}

/// Stable channel label for audit rows + the operator UI's
/// compliance view. `\"deduped\"` is reserved for callers that
/// want to record the F9 dedup short-circuit without claiming
/// the publish actually fired.
pub fn channel_label(channel: &NotificationChannel) -> &'static str {
    match channel {
        NotificationChannel::Disabled => "disabled",
        NotificationChannel::Whatsapp { .. } => "whatsapp",
        NotificationChannel::Email { .. } => "email",
    }
}

/// Channel label for the dedup short-circuit. Returned as a
/// `&'static str` so it composes with [`channel_label`] in
/// match arms without an extra alloc.
pub const CHANNEL_DEDUPED: &str = "deduped";

/// Reasons recording an audit row can fail. Producers map
/// every variant to a `tracing::warn` and continue — audit
/// failure NEVER blocks the live path (a routing decision
/// that went through but didn't get logged is a recovery
/// issue for compliance, not a fatal error).
#[derive(Debug, Error)]
pub enum AuditError {
    /// Underlying SQLx error.
    #[error("audit store: {0}")]
    Store(String),
}

impl AuditError {
    /// Build from any `Display`-able underlying error. Used
    /// to bridge the SDK's `events::Error` + sqlx errors
    /// without leaking their types through the public surface.
    pub fn from_display<E: std::fmt::Display>(e: E) -> Self {
        AuditError::Store(e.to_string())
    }
}

/// Tenant-scoped audit log handle. Wraps the SDK's
/// `EventStore<AuditEvent>` so producers don't need to know
/// the trait + table name. `Arc`-shared at boot.
#[derive(Clone)]
pub struct AuditLog {
    store: Arc<EventStore<AuditEvent>>,
}

impl AuditLog {
    /// Wrap an open store.
    pub fn new(store: Arc<EventStore<AuditEvent>>) -> Self {
        Self { store }
    }

    /// Append one audit row. Returns the live error so the
    /// caller can decide between bailing or continuing
    /// (default: log warn + continue).
    pub async fn record(&self, event: AuditEvent) -> Result<(), AuditError> {
        self.store
            .append(&event)
            .await
            .map_err(AuditError::from_display)
    }

    /// Filtered list. Caller passes the SDK `ListFilter`
    /// directly — no abstraction layer.
    pub async fn list(&self, filter: &ListFilter) -> Result<Vec<AuditEvent>, AuditError> {
        self.store
            .list(filter)
            .await
            .map_err(AuditError::from_display)
    }

    /// Convenience: every audit row for one lead, oldest
    /// first (caller flips the order in the UI when needed).
    pub async fn list_for_lead(
        &self,
        tenant_id: &str,
        lead_id: &str,
        limit: usize,
    ) -> Result<Vec<AuditEvent>, AuditError> {
        let filter = ListFilter {
            agent_id: Some(lead_id.to_string()),
            kind: None,
            tenant_id: Some(tenant_id.to_string()),
            since_ms: None,
            limit,
        };
        let mut rows = self.list(&filter).await?;
        // EventStore returns DESC by `at_ms`; flip to ASC
        // for the operator's "first → last" timeline view.
        rows.reverse();
        Ok(rows)
    }

    /// Borrow the underlying store — exposed so callers can
    /// share its `pool()` for retention sweeps + adjacent
    /// queries.
    pub fn store(&self) -> &Arc<EventStore<AuditEvent>> {
        &self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh_log() -> AuditLog {
        let store = EventStore::<AuditEvent>::open_memory(AUDIT_TABLE)
            .await
            .unwrap();
        AuditLog::new(Arc::new(store))
    }

    fn routing_decided(tenant: &str, lead: &str, at: u64) -> AuditEvent {
        AuditEvent::RoutingDecided {
            tenant_id: tenant.into(),
            lead_id: Some(lead.into()),
            from_email: "x@y.com".into(),
            chosen_seller_id: Some("pedro".into()),
            rule_id: Some("warm-corp".into()),
            why: vec!["resolver:display_name".into(), "rule:warm-corp".into()],
            score: 65,
            score_reasons: vec![],
            at_ms: at,
        }
    }

    fn transitioned(tenant: &str, lead: &str, at: u64) -> AuditEvent {
        AuditEvent::LeadTransitioned {
            tenant_id: tenant.into(),
            lead_id: lead.into(),
            from: "engaged".into(),
            to: "meeting_scheduled".into(),
            reason: "demo agendada".into(),
            at_ms: at,
        }
    }

    fn notif_published(tenant: &str, lead: &str, at: u64) -> AuditEvent {
        AuditEvent::NotificationPublished {
            tenant_id: tenant.into(),
            lead_id: lead.into(),
            seller_id: "pedro".into(),
            notification_kind: "lead_created".into(),
            channel: "whatsapp".into(),
            at_ms: at,
        }
    }

    #[tokio::test]
    async fn record_and_list_for_lead_returns_chronological_rows() {
        let log = fresh_log().await;
        log.record(routing_decided("acme", "l1", 10)).await.unwrap();
        log.record(transitioned("acme", "l1", 20)).await.unwrap();
        log.record(notif_published("acme", "l1", 30)).await.unwrap();
        let rows = log.list_for_lead("acme", "l1", 100).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert!(matches!(rows[0], AuditEvent::RoutingDecided { .. }));
        assert!(matches!(rows[1], AuditEvent::LeadTransitioned { .. }));
        assert!(matches!(rows[2], AuditEvent::NotificationPublished { .. }));
    }

    #[tokio::test]
    async fn list_for_lead_is_tenant_scoped() {
        let log = fresh_log().await;
        log.record(routing_decided("acme", "l1", 10)).await.unwrap();
        log.record(routing_decided("globex", "l1", 11)).await.unwrap();
        let rows = log.list_for_lead("acme", "l1", 100).await.unwrap();
        assert_eq!(rows.len(), 1);
        let acme_match = rows
            .iter()
            .all(|r| matches!(r, AuditEvent::RoutingDecided { tenant_id, .. } if tenant_id == "acme"));
        assert!(acme_match);
    }

    #[tokio::test]
    async fn list_filter_by_kind_works() {
        let log = fresh_log().await;
        log.record(routing_decided("acme", "l1", 10)).await.unwrap();
        log.record(transitioned("acme", "l1", 20)).await.unwrap();
        log.record(notif_published("acme", "l1", 30)).await.unwrap();
        let filter = ListFilter {
            kind: Some("lead_transitioned".into()),
            tenant_id: Some("acme".into()),
            limit: 100,
            ..Default::default()
        };
        let rows = log.list(&filter).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(matches!(rows[0], AuditEvent::LeadTransitioned { .. }));
    }

    #[tokio::test]
    async fn list_filter_by_since_ms_works() {
        let log = fresh_log().await;
        log.record(routing_decided("acme", "l1", 10)).await.unwrap();
        log.record(transitioned("acme", "l1", 20)).await.unwrap();
        log.record(notif_published("acme", "l1", 30)).await.unwrap();
        let filter = ListFilter {
            since_ms: Some(20),
            tenant_id: Some("acme".into()),
            limit: 100,
            ..Default::default()
        };
        let rows = log.list(&filter).await.unwrap();
        // Two rows >= 20 (transitioned + notification).
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn drop_outcome_records_with_none_lead_id() {
        let log = fresh_log().await;
        let event = AuditEvent::RoutingDecided {
            tenant_id: "acme".into(),
            lead_id: None,
            from_email: "spam@mailinator.com".into(),
            chosen_seller_id: None,
            rule_id: Some("drop-disposable".into()),
            why: vec!["drop_by_rule".into(), "rule:drop-disposable".into()],
            score: 0,
            score_reasons: vec![],
            at_ms: 1,
        };
        log.record(event).await.unwrap();
        // Lead-id-less rows still queryable by kind.
        let filter = ListFilter {
            kind: Some("routing_decided".into()),
            tenant_id: Some("acme".into()),
            limit: 100,
            ..Default::default()
        };
        let rows = log.list(&filter).await.unwrap();
        assert_eq!(rows.len(), 1);
        match &rows[0] {
            AuditEvent::RoutingDecided {
                lead_id,
                chosen_seller_id,
                ..
            } => {
                assert!(lead_id.is_none());
                assert!(chosen_seller_id.is_none());
            }
            other => panic!("expected RoutingDecided, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn metadata_kind_strings_match_serde_tag() {
        let r = routing_decided("acme", "l1", 1);
        let t = transitioned("acme", "l1", 2);
        let n = notif_published("acme", "l1", 3);
        assert_eq!(r.kind(), "routing_decided");
        assert_eq!(t.kind(), "lead_transitioned");
        assert_eq!(n.kind(), "notification_published");
        // Serde tag matches.
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"kind\":\"routing_decided\""));
    }

    #[tokio::test]
    async fn agent_id_falls_back_to_empty_for_drop_rows() {
        let drop = AuditEvent::RoutingDecided {
            tenant_id: "acme".into(),
            lead_id: None,
            from_email: "x@y.com".into(),
            chosen_seller_id: None,
            rule_id: None,
            why: vec![],
            score: 0,
            score_reasons: vec![],
            at_ms: 1,
        };
        assert_eq!(drop.agent_id(), "");
    }
}
