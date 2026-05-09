//! Loopback HTTP admin API consumed by the agent-creator
//! microapp's `/api/marketing/*` proxy.
//!
//! Bearer auth via `${MARKETING_ADMIN_TOKEN}`; every endpoint
//! requires the `X-Tenant-Id` header that the microapp stamps
//! from its own auth context (the operator never sends a
//! tenant id directly — the microapp resolves it from the
//! bearer + injects it into the proxied request).
//!
//! Multi-store mounting: this module receives a per-tenant
//! `LeadStore` map at boot. The auth middleware resolves the
//! tenant id → store and rejects requests for tenants the
//! extension hasn't been provisioned for.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::get;
use axum::Router;

use crate::firehose::LeadEventBus;
use crate::lead::{LeadStore, RouterHandle};
use crate::notification::{SellerLookup, TemplateLookup};
use crate::tenant::TenantId;

pub mod audit;
pub mod auth;
pub mod config;
pub mod firehose;
pub mod healthz;
pub mod leads;
pub mod marketing_state;
pub mod persons;
pub mod scoring;
pub mod spam_filter;
pub mod telemetry;
pub mod tracking;

pub use auth::{require_tenant_id, AuthState, AUTH_HEADER, TENANT_HEADER};

/// Shared router state — Arc-cloned per request so axum
/// extractors get cheap access. Not `Clone` itself; consumers
/// share it via `axum::extract::State<Arc<AdminState>>`.
pub struct AdminState {
    pub bearer_token: String,
    pub stores: HashMap<TenantId, Arc<LeadStore>>,
    /// Lead lifecycle broadcast bus consumed by `/firehose`.
    /// Default is a no-op bus (zero subscribers, publishes
    /// drop silently) so unit tests + the older HTTP-only path
    /// keep compiling without explicit wiring.
    pub firehose: Arc<LeadEventBus>,
    /// Where the per-tenant YAML config files live. Set via
    /// `with_state_root`; the GET `/config/*` endpoints read
    /// from `<state_root>/marketing/<tenant_id>/<file>.yaml`.
    /// Empty → endpoints surface `config_state_root_not_set`.
    pub state_root: Option<PathBuf>,
    /// Router handle for the live-reload pipeline. Same Arc
    /// the broker hop captured at boot — `PUT /config/rules`
    /// rebuilds the `LeadRouter` from the freshly written YAML
    /// and swaps it into this handle so subsequent broker
    /// events route through the new rules without a process
    /// restart.
    pub router: Option<RouterHandle>,
    /// M15.38 — seller lookup for notification routing.
    /// `PUT /config/sellers` rebuilds the `HashMap` from
    /// disk + swaps it into this handle so the broker hop's
    /// next notification publish routes via the fresh
    /// `agent_id` / `notification_settings`.
    pub seller_lookup: Option<SellerLookup>,
    /// M15.44 — template lookup for operator-supplied
    /// notification summaries. PUT
    /// `/config/notification_templates` rebuilds + swaps the
    /// inner Arc so the broker hop's next render picks up
    /// the override without a process restart.
    pub template_lookup: Option<TemplateLookup>,
    /// M15.23.a — engagement tracking deps (HMAC signer +
    /// SQLite store + public base URL). When `Some`, the
    /// public ingest routes `/t/o/...` + `/t/c/...` mount on
    /// the admin router (auth-bypassed, since they're hit by
    /// recipients' email clients). `None` ⇒ tracking
    /// disabled and the ingest routes 404.
    pub tracking: Option<Arc<crate::tracking::TrackingDeps>>,
    /// M15.23.c — AI decision audit log. When `Some`, the
    /// `/audit` query endpoint mounts + producers (broker
    /// hop, transition tools, notification publish) record
    /// events. `None` ⇒ audit disabled (tests / minimal
    /// setups).
    pub audit: Option<Arc<crate::audit::AuditLog>>,
    /// M15.23.d — operator-supplied topic guardrails. When
    /// `Some`, `PUT /config/topic_guardrails` rebuilds +
    /// swaps the inner `Arc` so subsequent broker-hop scans
    /// pick up the override without a process restart.
    pub guardrails: Option<crate::guardrails::GuardrailHandle>,
    /// M15.21 slice 2 — outbound publisher (compliance gate
    /// + idempotency tracking + topic composition). One
    /// instance per process (matches the per-tenant
    /// extension boundary today). When `None`, the approve
    /// handler refuses with `outbound_disabled`.
    pub outbound: Option<Arc<crate::broker::OutboundPublisher>>,
    /// M15.21 slice 2 — `BrokerSender` cloned out of the
    /// first `on_broker_event` invocation. The admin server
    /// runs in parallel to the broker subscriber so the
    /// sender isn't available at boot — we capture it
    /// lazily on the first event. Loads via
    /// `broker_sender.load_full()` returning
    /// `Option<Arc<BrokerSender>>`. `None` ⇒ publish refuses
    /// with a clear "broker not yet attached" error.
    pub broker_sender: Arc<
        arc_swap::ArcSwapOption<nexo_microapp_sdk::plugin::BrokerSender>,
    >,
    /// M15.21 slice 2 — tracking deps shared with the
    /// existing engagement endpoints. Reused here to mint
    /// the per-send `MsgId` + inject the open-pixel + rewrite
    /// links before the outbound goes through the broker.
    /// `None` ⇒ outbound publishes WITHOUT tracking signals
    /// (operator hasn't wired the secret yet).
    pub tracking_for_outbound: Option<Arc<crate::tracking::TrackingDeps>>,
    /// M15.21 slice 4 — pluggable draft generator. The
    /// `POST /leads/:id/drafts/generate` endpoint resolves
    /// lead + seller + last inbound, hands the context to
    /// this generator, and persists the response as a
    /// pending draft row. `None` ⇒ the endpoint refuses
    /// with `draft_generator_disabled` so deployments that
    /// haven't wired one yet surface a clear error.
    pub draft_generator:
        Option<Arc<dyn crate::draft::DraftGenerator + Send + Sync>>,
    /// Hot-swappable draft template handle. Same Arc the
    /// `TemplateDraftGenerator` reads from, so
    /// `PUT /config/draft_template` lands the new template
    /// in-memory and the next draft generation picks it up
    /// without a restart. `None` ⇒ template config endpoints
    /// 503.
    pub draft_template: Option<crate::draft::DraftTemplateHandle>,
    /// M15.21.b — person store the lead drawer reads to
    /// surface `enrichment_status` + `enrichment_confidence`
    /// per lead. Same Arc the broker hop's identity
    /// resolver writes to, so operator confirmations are
    /// visible to the next inbound. `None` ⇒ the
    /// `/persons` endpoints 503 with `persons_not_loaded`.
    pub persons: Option<Arc<dyn nexo_microapp_sdk::identity::PersonStore>>,
    /// M15.21.b — company store backing the operator's
    /// "Inferred: Acme Corp" prompt. Same Arc the
    /// enrichment chain populates. `None` ⇒ enrichment
    /// confirm endpoints accept `primary_name` updates
    /// only; the company side surfaces 503 when the
    /// operator tries to set a company.
    pub companies: Option<Arc<dyn nexo_microapp_sdk::identity::CompanyStore>>,
    /// Per-tenant spam / promo filter cache. Same `RulesCache`
    /// captured by the broker hop at boot — every admin write
    /// invalidates the cached entry so subsequent inbounds see
    /// the fresh state. `None` ⇒ `/admin/spam-filter/*` 503s.
    pub spam_filter: Option<crate::spam_filter::RulesCache>,
    /// In-memory coalescing lock keyed by draft signature
    /// (option **J**). Two concurrent generate requests for
    /// the same signature serialize on this lock; the second
    /// re-checks the DB after acquiring + finds the freshly
    /// persisted draft → returns cached. Always wired (cheap
    /// when unused) so handler tests don't have to thread an
    /// `Option` through.
    pub draft_locks: crate::draft_lock::DraftLockMap,
    /// Per-tenant scoring config cache (thresholds + keyword
    /// lists). Same `Arc` shared with the broker hop's
    /// `score_lead_with_config` call; admin writes invalidate.
    pub scoring: Option<crate::scoring::ScoringConfigCache>,
    /// Per-tenant on/off toggle. Operator-facing kill switch.
    /// Read by the dispatcher (tools), broker hop (notification
    /// publish gate), and `generate_draft_handler` (refuses
    /// LLM-spending automation while paused).
    pub marketing_state: Option<crate::marketing_state::MarketingStateCache>,
}

impl AdminState {
    pub fn new(bearer_token: String) -> Self {
        Self {
            bearer_token,
            stores: HashMap::new(),
            firehose: Arc::new(LeadEventBus::new()),
            state_root: None,
            router: None,
            seller_lookup: None,
            template_lookup: None,
            tracking: None,
            audit: None,
            guardrails: None,
            outbound: None,
            broker_sender: Arc::new(arc_swap::ArcSwapOption::empty()),
            tracking_for_outbound: None,
            draft_generator: None,
            persons: None,
            companies: None,
            draft_template: None,
            spam_filter: None,
            draft_locks: crate::draft_lock::DraftLockMap::new(),
            scoring: None,
            marketing_state: None,
        }
    }

    pub fn with_store(mut self, store: Arc<LeadStore>) -> Self {
        self.stores
            .insert(store.tenant_id().clone(), store);
        self
    }

    /// Inject the shared `LeadEventBus`. The broker handler +
    /// the `/firehose` SSE route MUST resolve to the same bus
    /// instance — call this with the same Arc the broker
    /// captured at boot.
    pub fn with_firehose(mut self, firehose: Arc<LeadEventBus>) -> Self {
        self.firehose = firehose;
        self
    }

    /// Set the state root used by the `/config/*` endpoints.
    /// Call once at boot with the same path the lead store
    /// + identity DB use; the YAML loaders compute the
    /// per-tenant subdir internally.
    pub fn with_state_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.state_root = Some(root.into());
        self
    }

    /// Inject the router handle the broker hop captured at
    /// boot. `PUT /config/rules` swaps a freshly built
    /// `LeadRouter` into this Arc; subsequent broker events
    /// pick up the new rules without a process restart.
    pub fn with_router(mut self, router: RouterHandle) -> Self {
        self.router = Some(router);
        self
    }

    /// Inject the seller lookup the broker hop captured at
    /// boot. `PUT /config/sellers` swaps a freshly loaded
    /// `HashMap<SellerId, Seller>` into this Arc so the
    /// next notification publish reads the updated
    /// `notification_settings`.
    pub fn with_seller_lookup(mut self, lookup: SellerLookup) -> Self {
        self.seller_lookup = Some(lookup);
        self
    }

    /// Inject the template lookup the broker hop + tools
    /// captured at boot. `PUT /config/notification_templates`
    /// swaps a freshly loaded `NotificationTemplates` into
    /// this Arc so subsequent renders pick up the override.
    pub fn with_template_lookup(mut self, lookup: TemplateLookup) -> Self {
        self.template_lookup = Some(lookup);
        self
    }

    /// Inject the engagement-tracking deps. When `Some`, the
    /// public `/t/o/...` + `/t/c/...` routes mount on the
    /// router. The same `Arc` should be threaded through the
    /// outbound publisher so URL signing + ingest verification
    /// share one signer instance.
    pub fn with_tracking(
        mut self,
        deps: Arc<crate::tracking::TrackingDeps>,
    ) -> Self {
        self.tracking = Some(deps);
        self
    }

    /// Inject the AI decision audit log. When `Some`, the
    /// `/audit` query endpoint mounts + the broker hop /
    /// tools record entries. The same `Arc` should be
    /// threaded through `PluginDeps` so producers + the
    /// query endpoint share one store.
    pub fn with_audit(mut self, log: Arc<crate::audit::AuditLog>) -> Self {
        self.audit = Some(log);
        self
    }

    /// Inject the topic guardrail handle the broker hop
    /// captured at boot. `PUT /config/topic_guardrails`
    /// rebuilds + swaps the inner Arc so subsequent scans
    /// pick up the new rules.
    pub fn with_guardrails(
        mut self,
        handle: crate::guardrails::GuardrailHandle,
    ) -> Self {
        self.guardrails = Some(handle);
        self
    }

    /// M15.21 slice 2 — wire the outbound publisher. Same
    /// `Arc` shared between this admin surface (approve
    /// handler) and any future tools that publish drafts
    /// post-LLM-generation.
    pub fn with_outbound(
        mut self,
        publisher: Arc<crate::broker::OutboundPublisher>,
    ) -> Self {
        self.outbound = Some(publisher);
        self
    }

    /// M15.21 slice 2 — wire the broker-sender cell. Same
    /// `ArcSwapOption` the broker subscriber populates on
    /// its first invocation. Caller passes the same `Arc`
    /// instance both here and into the closure so the
    /// approve handler reads what the closure stored.
    pub fn with_broker_sender_cell(
        mut self,
        cell: Arc<
            arc_swap::ArcSwapOption<nexo_microapp_sdk::plugin::BrokerSender>,
        >,
    ) -> Self {
        self.broker_sender = cell;
        self
    }

    /// M15.21 slice 2 — same tracking deps the engagement
    /// endpoints already consume. Reusing the Arc keeps the
    /// signer + store + base_url consistent across the
    /// outbound prep + ingest verify paths.
    pub fn with_tracking_for_outbound(
        mut self,
        deps: Arc<crate::tracking::TrackingDeps>,
    ) -> Self {
        self.tracking_for_outbound = Some(deps);
        self
    }

    /// M15.21 slice 4 — wire the draft generator. Boot
    /// constructs a [`crate::draft::TemplateDraftGenerator`]
    /// with the bundled default template; future wiring can
    /// swap in `AgentDraftGenerator` (NATS-RPC to the bound
    /// agent's LLM) without touching the admin surface.
    pub fn with_draft_generator(
        mut self,
        gen: Arc<dyn crate::draft::DraftGenerator + Send + Sync>,
    ) -> Self {
        self.draft_generator = Some(gen);
        self
    }

    /// M15.21.b — wire the person store. Same Arc the
    /// broker hop's identity resolver writes to so the
    /// lead drawer reads the freshest `enrichment_status`
    /// + `enrichment_confidence` and operator
    /// confirmations land where the resolver picks them
    /// up.
    pub fn with_persons(
        mut self,
        persons: Arc<dyn nexo_microapp_sdk::identity::PersonStore>,
    ) -> Self {
        self.persons = Some(persons);
        self
    }

    /// M15.21.b — wire the company store. Required when
    /// operators confirm or edit a company name from the
    /// lead drawer; without it the confirm endpoint refuses
    /// company writes (name updates still work).
    pub fn with_companies(
        mut self,
        companies: Arc<dyn nexo_microapp_sdk::identity::CompanyStore>,
    ) -> Self {
        self.companies = Some(companies);
        self
    }

    /// Wire the shared draft template handle. Boot
    /// constructs the handle, seeds it from disk (or the
    /// bundled default), threads it both into the
    /// `TemplateDraftGenerator` and into AdminState so PUT
    /// hot-swaps land where the generator reads.
    pub fn with_draft_template(
        mut self,
        handle: crate::draft::DraftTemplateHandle,
    ) -> Self {
        self.draft_template = Some(handle);
        self
    }

    /// Wire the shared spam-filter cache. Boot constructs the
    /// `RulesCache` once and threads it both into `PluginDeps`
    /// (read path on the broker hop) and into `AdminState`
    /// (write path that invalidates the cache after PUT/POST/
    /// DELETE), so admin edits land on the very next inbound.
    pub fn with_spam_filter(
        mut self,
        cache: crate::spam_filter::RulesCache,
    ) -> Self {
        self.spam_filter = Some(cache);
        self
    }

    /// Wire the shared scoring config cache. Same `Arc` lives
    /// in `PluginDeps`; admin endpoint invalidates on write.
    pub fn with_scoring(
        mut self,
        cache: crate::scoring::ScoringConfigCache,
    ) -> Self {
        self.scoring = Some(cache);
        self
    }

    /// Wire the shared marketing on/off cache. Same `Arc` lives
    /// in `PluginDeps`; admin endpoint invalidates on write.
    pub fn with_marketing_state(
        mut self,
        cache: crate::marketing_state::MarketingStateCache,
    ) -> Self {
        self.marketing_state = Some(cache);
        self
    }

    pub fn lookup_store(&self, tenant_id: &TenantId) -> Option<Arc<LeadStore>> {
        self.stores.get(tenant_id).cloned()
    }
}

/// Build the protected router. Every route mounts under the
/// auth middleware so unauthenticated callers never reach a
/// handler.
pub fn router(state: Arc<AdminState>) -> Router {
    let auth_layer = axum::middleware::from_fn_with_state(
        Arc::clone(&state),
        auth::bearer_and_tenant_middleware,
    );
    let protected = Router::new()
        .route("/healthz", get(healthz::handler))
        .route("/leads", get(leads::list_handler))
        .route("/leads/:lead_id", get(leads::get_handler))
        .route("/leads/:lead_id/thread", get(leads::thread_handler))
        // Operator-driven manual state transition.
        .route(
            "/leads/:lead_id/transition",
            axum::routing::post(leads::transition_handler),
        )
        // M15.21.notes — free-form operator scratch pad.
        .route(
            "/leads/:lead_id/notes",
            axum::routing::put(leads::update_notes_handler),
        )
        // M15.21.followup-override — skip / postpone bypass.
        .route(
            "/leads/:lead_id/followup/override",
            axum::routing::post(leads::followup_override_handler),
        )
        .route(
            "/leads/:lead_id/drafts",
            get(leads::list_drafts_handler).post(leads::create_draft_handler),
        )
        .route(
            "/leads/:lead_id/drafts/:message_id",
            axum::routing::put(leads::update_draft_handler)
                .delete(leads::delete_draft_handler),
        )
        .route(
            "/leads/:lead_id/drafts/:message_id/reject",
            axum::routing::post(leads::reject_draft_handler),
        )
        .route(
            "/leads/:lead_id/drafts/:message_id/approve",
            axum::routing::post(leads::approve_draft_handler),
        )
        // M15.21 slice 4 — operator-pull draft generation.
        // Path is a sibling of the slice-1 collection so
        // the static segment doesn't collide with the
        // `:message_id` param.
        .route(
            "/leads/:lead_id/drafts/generate",
            axum::routing::post(leads::generate_draft_handler),
        )
        // Tenant-wide pending drafts queue. Path is a
        // top-level sibling of `/leads` so it doesn't need
        // a lead id in the URL.
        .route("/drafts", get(leads::drafts_inbox_handler))
        // M15.21.b — person + enrichment surface.
        .route("/persons/:person_id", get(persons::get_handler))
        .route(
            "/persons/:person_id/confirm-enrichment",
            axum::routing::post(persons::confirm_enrichment_handler),
        )
        .route(
            "/config/mailboxes",
            get(config::list_mailboxes).put(config::put_mailboxes),
        )
        .route(
            "/config/sellers",
            get(config::list_sellers).put(config::put_sellers),
        )
        .route(
            "/config/rules",
            get(config::get_rules).put(config::put_rules),
        )
        .route(
            "/config/followup_profiles",
            get(config::list_followup_profiles).put(config::put_followup_profiles),
        )
        .route(
            "/config/notification_templates",
            get(config::get_notification_templates)
                .put(config::put_notification_templates),
        )
        .route(
            "/config/topic_guardrails",
            get(config::list_topic_guardrails)
                .put(config::put_topic_guardrails),
        )
        .route(
            "/config/templates",
            get(config::list_templates).put(config::put_templates),
        )
        // Draft template hot-swap. GET returns the active
        // template body + source ("default" / "tenant"); PUT
        // validates by sandbox-rendering against a fixture
        // context first, then writes + swaps in-memory.
        .route(
            "/config/draft_template",
            get(config::get_draft_template).put(config::put_draft_template),
        )
        // Sandbox-render preview without persisting.
        .route(
            "/config/draft_template/preview",
            axum::routing::post(config::preview_draft_template),
        )
        .route(
            "/config/snippets",
            get(config::list_snippets).put(config::put_snippets),
        )
        .route("/firehose", get(firehose::handler))
        .route(
            "/tracking/msg/:msg_id/engagement",
            get(tracking::engagement_handler),
        )
        .route("/audit", get(audit::handler))
        // Spam / promo filter — tenant-customizable rules +
        // strictness preset. Read path is cached; writes
        // invalidate.
        .route("/spam-filter", get(spam_filter::get_handler))
        .route(
            "/spam-filter/config",
            axum::routing::put(spam_filter::put_config_handler),
        )
        .route(
            "/spam-filter/rules",
            axum::routing::post(spam_filter::add_rule_handler),
        )
        .route(
            "/spam-filter/rules/:rule_id",
            axum::routing::delete(spam_filter::delete_rule_handler),
        )
        .route(
            "/spam-filter/test",
            axum::routing::post(spam_filter::test_handler),
        )
        // Marketing on/off toggle. Pause halts automated
        // effects (notifications, draft generation, follow-up
        // sweeps) without losing inbound traffic.
        .route(
            "/marketing/state",
            get(marketing_state::get_handler).put(marketing_state::put_handler),
        )
        // Tenant-tunable scoring weights + keyword lists.
        // GET reads, PUT replaces with the supplied
        // ScoringConfig, DELETE resets to bundled defaults.
        .route(
            "/scoring/config",
            get(scoring::get_handler)
                .put(scoring::put_handler)
                .delete(scoring::delete_handler),
        )
        // M15.24 — operator dashboard aggregate snapshot.
        .route("/telemetry", get(telemetry::handler))
        .layer(auth_layer);

    // M15.23.a.3 — public ingest routes for the open pixel +
    // click redirector. The bearer-auth middleware can't apply
    // here (recipients' email clients don't carry our token);
    // forgery is blocked by the HMAC tag in the URL. Mounted
    // unconditionally — when `state.tracking == None` the
    // handlers self-404.
    let public = Router::new()
        .route("/t/o/:tenant/:msg", get(tracking::open_handler))
        .route("/t/c/:tenant/:msg/:link", get(tracking::click_handler));

    protected.merge(public).with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lead::LeadStore;
    use std::path::PathBuf;

    #[tokio::test]
    async fn admin_state_with_store_indexes_by_tenant() {
        let s = LeadStore::open(PathBuf::from(":memory:"), TenantId::new("acme").unwrap())
            .await
            .unwrap();
        let st = AdminState::new("token".into()).with_store(Arc::new(s));
        assert!(st.lookup_store(&TenantId::new("acme").unwrap()).is_some());
        assert!(st.lookup_store(&TenantId::new("globex").unwrap()).is_none());
    }
}
