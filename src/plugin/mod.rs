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

use nexo_microapp_sdk::enrichment::{EnrichmentCache, FallbackChain};
use nexo_microapp_sdk::identity::{
    CompanyStore, LidPnMappingStore, PersonEmailStore, PersonPhoneStore, PersonStore,
};

use crate::enrichment::Scraper;
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
    /// M15.23.e — phone identifier store. `None` ⇒ phone-
    /// based duplicate detection skipped (operator hasn't
    /// wired WhatsApp / SMS into the marketing extension's
    /// identity pool yet).
    pub person_phones: Option<Arc<dyn PersonPhoneStore>>,
    /// M15.23.e / F23 — LID ↔ PN mapping store. When set,
    /// the WA ingest collapses both namespaces into a
    /// single \`Person\` row whenever the protocol announces
    /// a migration. \`None\` ⇒ each namespace stays its own
    /// identity (pre-F23 behaviour).
    pub lid_pn_mappings: Option<Arc<dyn LidPnMappingStore>>,
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
            person_phones: None,
            lid_pn_mappings: None,
            chain,
        }
    }

    /// Builder-style wiring for the phone store. Boot path
    /// passes the same Arc the duplicate matcher consumes.
    pub fn with_person_phones(
        mut self,
        phones: Arc<dyn PersonPhoneStore>,
    ) -> Self {
        self.person_phones = Some(phones);
        self
    }

    /// Builder-style wiring for the LID ↔ PN mapping store
    /// (F23). Same Arc the WA ingest + duplicate matcher
    /// consult to collapse cross-namespace identities.
    pub fn with_lid_pn_mappings(
        mut self,
        mappings: Arc<dyn LidPnMappingStore>,
    ) -> Self {
        self.lid_pn_mappings = Some(mappings);
        self
    }
}

/// Phase 82.15.bx+ — auto-company-enrichment dependencies. The
/// broker hop consults these on every inbound: when the resolved
/// person's domain is `Corporate`, we look up the cache + spawn
/// a detached scraper task on cache miss to upsert a `Company`
/// row + link `person.company_id`. `None` on `PluginDeps`
/// disables auto-enrichment (tests + minimal setups stay clean).
#[derive(Clone)]
pub struct EnrichmentDeps {
    /// HTTP scraper for domain → `name`/`industry`/`description`.
    /// Cheap to clone (Arc-shared semaphore + reqwest pool).
    pub scraper: Scraper,
    /// 30-day TTL cache keyed by `(tenant_id, domain)`. Holds
    /// the serialised `Company` so subsequent inbounds from the
    /// same domain skip the scrape.
    pub cache: Arc<dyn EnrichmentCache>,
    /// Persistent `Company` store. The scrape callback upserts
    /// here + the broker hop's link path reads back to set
    /// `person.company_id`.
    pub companies: Arc<dyn CompanyStore>,
    /// Cache row TTL for corporate scrapes. Defaults to 30 days
    /// (matches the SDK convention); operator can override
    /// before passing in.
    pub corporate_ttl_ms: i64,
}

impl EnrichmentDeps {
    /// Default corporate TTL — 30 days in ms. Matches the SDK's
    /// `enrichment::cache` documentation.
    pub const DEFAULT_CORPORATE_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

    pub fn new(
        scraper: Scraper,
        cache: Arc<dyn EnrichmentCache>,
        companies: Arc<dyn CompanyStore>,
    ) -> Self {
        Self {
            scraper,
            cache,
            companies,
            corporate_ttl_ms: Self::DEFAULT_CORPORATE_TTL_MS,
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
    /// Phase 82.15.bx+ — auto-company-enrichment via the
    /// `DomainScraper` + `EnrichmentCache` + `CompanyStore`
    /// triplet. `None` keeps today's behaviour (placeholder
    /// `person.company_id = None` for every inbound regardless
    /// of domain kind). Production deployments wire this in
    /// `main.rs`; tests opt out.
    pub enrichment: Option<EnrichmentDeps>,
    /// Tenant-customizable spam / promo classifier. When set,
    /// every inbound runs through the rule-aware classifier
    /// before the lead pipeline; `Promo` verdicts drop the
    /// message (no lead, no thread bump) and emit an audit
    /// entry. `None` disables filtering — useful in tests +
    /// minimal setups where the operator hasn't enabled it.
    pub spam_filter: Option<crate::spam_filter::RulesCache>,
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
            enrichment: None,
            spam_filter: None,
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

    /// Builder-style wiring for the auto-enrichment triplet
    /// (scraper + cache + companies). When set, every inbound
    /// from a corporate domain triggers a cache lookup and (on
    /// miss) a detached scrape task that upserts a `Company`
    /// row + caches the result. The broker hop links
    /// `person.company_id` synchronously when the cache hit is
    /// available; on miss, the link is deferred until the next
    /// inbound from the same domain (operator visibility via
    /// firehose update is a follow-up).
    pub fn with_enrichment(mut self, deps: EnrichmentDeps) -> Self {
        self.enrichment = Some(deps);
        self
    }

    /// Builder-style wiring for the spam filter cache. The same
    /// `RulesCache` is captured by the broker hop (read path)
    /// + the admin endpoints (write + invalidate path) so a PUT
    /// /admin/spam-filter takes effect on the very next inbound.
    pub fn with_spam_filter(mut self, cache: crate::spam_filter::RulesCache) -> Self {
        self.spam_filter = Some(cache);
        self
    }
}
