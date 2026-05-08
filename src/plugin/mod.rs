//! Plugin contract glue (Phase 81.5).
//!
//! Wires the extension's tool handlers + broker subscriber to
//! the SDK's `PluginAdapter` so the daemon can spawn the
//! `nexo-marketing` binary as a subprocess, hand it
//! `tool.invoke` calls over JSON-RPC stdio, and route
//! `plugin.inbound.email.*` events through to the inbound
//! decoder.
//!
//! Sibling pattern of `nexo-rs-plugin-browser/src/main.rs`:
//! a single dispatch closure routes by `tool_name`, and a
//! single broker handler decodes the email envelope. The
//! HTTP admin surface (operator UI proxy) keeps running in
//! a `tokio::spawn` alongside the stdio loop.

use std::sync::Arc;

use nexo_microapp_sdk::enrichment::FallbackChain;
use nexo_microapp_sdk::identity::{PersonEmailStore, PersonStore};

use crate::firehose::LeadEventBus;
use crate::lead::{LeadStore, RouterHandle};
use crate::notification::{SellerLookup, TemplateLookup};
use crate::notification_dedup::DedupCache;
use crate::tenant::TenantId;

pub mod broker;
pub mod dispatch;
pub mod tool_defs;

/// Bundle the three identity-related Arcs the broker handler
/// needs to upsert resolved persons. Pre-bundling keeps the
/// `PluginDeps` field count manageable + lets handlers spawn
/// without juggling 5+ Arcs per call.
#[derive(Clone)]
pub struct IdentityDeps {
    pub persons: Arc<dyn PersonStore>,
    pub person_emails: Arc<dyn PersonEmailStore>,
    pub chain: Arc<FallbackChain>,
}

impl IdentityDeps {
    pub fn new(
        persons: Arc<dyn PersonStore>,
        person_emails: Arc<dyn PersonEmailStore>,
        chain: Arc<FallbackChain>,
    ) -> Self {
        Self {
            persons,
            person_emails,
            chain,
        }
    }
}

/// Shared dependencies the dispatch closure + broker handler
/// need at runtime. Cloned per-call (each field is `Arc`-cheap).
#[derive(Clone)]
pub struct PluginDeps {
    pub tenant_id: TenantId,
    pub lead_store: Arc<LeadStore>,
    /// `RouterHandle` instead of `Arc<LeadRouter>` so the
    /// dispatch + broker paths see swaps committed by
    /// `PUT /config/rules` without a process restart. Each
    /// call site loads a snapshot via `router.load_full()`.
    pub router: RouterHandle,
    /// `None` means "identity disabled" — the broker hop falls
    /// back to placeholder ids. Production deployments always
    /// have it; some tests opt out to keep the fixture small.
    pub identity: Option<IdentityDeps>,
    /// Seller index for notification routing. `None` =
    /// notifications disabled. PUT `/config/sellers`
    /// rebuilds + swaps under the broker hop's nose
    /// (same `arc_swap` pattern as the router live-reload).
    pub sellers: Option<SellerLookup>,
    /// M15.44 — operator-supplied notification templates.
    /// `None` = renderers fall back to framework defaults.
    /// PUT `/config/notification_templates` rebuilds + swaps
    /// the inner Arc using the same arc_swap pattern.
    pub templates: Option<TemplateLookup>,
    /// M15.53 / F9 — in-memory dedup cache for notification
    /// publishes. NATS at-least-once redelivery (transient
    /// disconnect) → broker hop runs twice → would publish
    /// the same `EmailNotification` twice. The cache holds
    /// a 1h time-bucket key per (tenant, lead, kind, minute);
    /// duplicate hits short-circuit the publish silently.
    /// `None` disables dedup (tests + minimal setups).
    pub dedup: Option<Arc<DedupCache>>,
    /// M15.23.c — AI decision audit log. When `Some`, the
    /// broker hop's create path + the state-transition tools
    /// + the notification publish branches record entries.
    /// `None` ⇒ producers skip the recorder silently
    /// (tests + minimal setups).
    pub audit: Option<Arc<crate::audit::AuditLog>>,
}

impl PluginDeps {
    pub fn new(
        tenant_id: TenantId,
        lead_store: Arc<LeadStore>,
        router: RouterHandle,
    ) -> Self {
        Self {
            tenant_id,
            lead_store,
            router,
            identity: None,
            sellers: None,
            templates: None,
            dedup: None,
            audit: None,
        }
    }

    /// Builder-style wiring for the seller lookup. The same
    /// `Arc` is captured by the broker hop closure (read path)
    /// + the admin handler (PUT sellers rebuild path).
    pub fn with_sellers(mut self, lookup: SellerLookup) -> Self {
        self.sellers = Some(lookup);
        self
    }

    /// Builder-style wiring for the templates lookup. Captured
    /// by the broker hop + tools dispatch + PUT
    /// /config/notification_templates rebuild path.
    pub fn with_templates(mut self, lookup: TemplateLookup) -> Self {
        self.templates = Some(lookup);
        self
    }

    /// Builder-style wiring for the identity stores + resolver
    /// chain. Call this at boot when the extension has them
    /// open; tests skip it for the placeholder path.
    pub fn with_identity(mut self, identity: IdentityDeps) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Builder-style wiring for the notification dedup cache.
    /// Same `Arc` captured by the broker hop + every tool that
    /// publishes notifications, so all publish call sites share
    /// one TTL window.
    pub fn with_dedup(mut self, cache: Arc<DedupCache>) -> Self {
        self.dedup = Some(cache);
        self
    }

    /// Builder-style wiring for the audit log. Same `Arc`
    /// captured by the admin `/audit` query endpoint + every
    /// producer (broker hop + tools + notification publish)
    /// so the operator's compliance view sees a unified
    /// timeline.
    pub fn with_audit(mut self, log: Arc<crate::audit::AuditLog>) -> Self {
        self.audit = Some(log);
        self
    }
}
