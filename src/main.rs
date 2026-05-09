// Subprocess entrypoint. The daemon's plugin-discovery walker
// finds this binary next to its `nexo-plugin.toml` (Phase 81.5)
// and spawns it as a long-lived child speaking JSON-RPC 2.0
// over stdio per `nexo-plugin-contract.md` v1.10.0.
//
// Two surfaces run concurrently:
//
// 1. **Stdio JSON-RPC** via `PluginAdapter::run_stdio` — the
//    canonical plugin contract. Handles `initialize`,
//    `tool.invoke`, broker events, shutdown.
//
// 2. **Loopback HTTP admin** on `${MARKETING_HTTP_PORT}` — the
//    operator-UI proxy reads through this. Independent of the
//    plugin contract; speaks bearer auth + `X-Tenant-Id`.
//
// Both surfaces share the same per-tenant `LeadStore`. The
// HTTP server runs in `tokio::spawn` so the main task can
// drive `run_stdio()` (the daemon kills the subprocess if
// stdout closes, so stdio MUST stay on the main task).

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;

use nexo_marketing::admin::{self, AdminState};
use nexo_marketing::firehose::LeadEventBus;
use nexo_marketing::identity::adapters::{
    display_name::DisplayNameParser, reply_to::ReplyToReader,
};
use nexo_marketing::lead::{router::load_rule_set, router_handle, LeadRouter, LeadStore};
use nexo_marketing::plugin::{
    broker::handle_inbound_event, dispatch::dispatch as plugin_dispatch,
    tool_defs::marketing_tool_defs, IdentityDeps, PluginDeps,
};
use nexo_marketing::tenant::TenantId;
use nexo_microapp_sdk::enrichment::FallbackChain;
use nexo_microapp_sdk::identity::{
    open_pool, PersonEmailStore, PersonStore, SqliteCompanyStore, SqlitePersonEmailStore,
    SqlitePersonStore,
};
use nexo_microapp_sdk::plugin::{BrokerSender, PluginAdapter, ToolContext, ToolInvocation};
use nexo_microapp_sdk::BrokerEvent;

const DEFAULT_BIND: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 18766;
const MANIFEST: &str = include_str!("../plugin.toml");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    nexo_marketing::init_tracing();
    let version = env!("CARGO_PKG_VERSION");
    tracing::info!(%version, "nexo-marketing starting");

    let bearer = env::var("MARKETING_ADMIN_TOKEN")
        .context("MARKETING_ADMIN_TOKEN env required (matches plugin.http_server.token_env)")?;
    let port: u16 = env::var("MARKETING_HTTP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let state_root = env::var("NEXO_EXTENSION_STATE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".dev-state/ext-state"));

    // Bootstrap a single-tenant store keyed off `MARKETING_TENANT_ID`
    // (operator default install). Multi-tenant mounting lands in a
    // follow-up — for now both surfaces target the configured tenant.
    let tenant = env::var("MARKETING_TENANT_ID")
        .ok()
        .and_then(|s| TenantId::new(s).ok())
        .unwrap_or_else(|| {
            TenantId::new("default").expect("'default' is a valid kebab-case tenant id")
        });

    let lead_store = Arc::new(
        LeadStore::open(&state_root, tenant.clone())
            .await
            .with_context(|| format!("open lead store at {}", state_root.display()))?,
    );
    tracing::info!(tenant = %tenant, "lead store mounted");

    let rule_set = load_rule_set(&state_root, &tenant)
        .with_context(|| format!("load rule set for {tenant}"))?;
    let router = router_handle(LeadRouter::new(tenant.clone(), rule_set));

    // ─── Seller lookup (M15.38) ─────────────────────────────
    // Boot snapshot from sellers.yaml; PUT
    // /config/sellers rebuilds + swaps under the broker
    // hop's nose.
    let sellers_initial =
        nexo_marketing::config::load_sellers(&state_root, &tenant).unwrap_or_default();
    let seller_lookup =
        nexo_marketing::notification::seller_lookup_from_list(sellers_initial);

    // ─── Notification templates lookup (M15.44) ─────────────
    // Boot snapshot from notification_templates.yaml; PUT
    // /config/notification_templates rebuilds + swaps. None
    // entries → renderers fall back to framework defaults.
    let templates_initial =
        nexo_marketing::config::load_notification_templates(&state_root, &tenant)
            .unwrap_or_default();
    let template_lookup =
        nexo_marketing::notification::template_lookup_from(templates_initial);

    // ─── Identity stores + resolver chain ─────────────────────
    // One pool per tenant; backs Person + PersonEmail + Company
    // stores. Migration runs eagerly (the SDK's open_pool calls
    // it). Today's chain is the cheap deterministic adapters
    // (`display_name` + `reply_to`); the LLM extractor + scraper
    // adapters wire in M22 once the LLM client + scraper config
    // come from the operator's YAML.
    let identity_db_path = tenant.state_dir(&state_root).join("identity.db");
    if let Some(parent) = identity_db_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let identity_pool = open_pool(&identity_db_path)
        .await
        .with_context(|| format!("open identity pool at {}", identity_db_path.display()))?;
    // Touch the companies store so its tables migrate (the
    // resolver path doesn't write to it yet, but the schema
    // must be present for downstream company enrichment.
    // M15.21.b — keep the company store live so the lead
    // drawer's enrichment override endpoint can upsert
    // operator-confirmed company rows.
    let companies: Arc<dyn nexo_microapp_sdk::identity::CompanyStore> =
        Arc::new(SqliteCompanyStore::new(identity_pool.clone()));
    let persons: Arc<dyn PersonStore> =
        Arc::new(SqlitePersonStore::new(identity_pool.clone()));
    let person_emails: Arc<dyn PersonEmailStore> =
        Arc::new(SqlitePersonEmailStore::new(identity_pool.clone()));
    // M15.23.e — phone store rides on the same identity pool
    // so the duplicate matcher's email + phone signals share
    // one transactional surface.
    let person_phones: Arc<dyn nexo_microapp_sdk::identity::PersonPhoneStore> =
        Arc::new(nexo_microapp_sdk::identity::SqlitePersonPhoneStore::new(
            identity_pool.clone(),
        ));
    // F23 — LID ↔ PN mapping store. Same identity pool;
    // table created via the SDK's identity migration. WA
    // ingest collapses cross-namespace inbounds into a
    // single Person row when the mapping is populated.
    // Phase 82.15.bx+ — auto-enrichment cache lives on the
    // same identity pool: shares one SQLite file, one
    // migration touchpoint, keeps the per-tenant state dir
    // tidy. Migrate the table before any reads.
    let enrichment_pool = identity_pool.clone();
    nexo_microapp_sdk::enrichment::cache::migrate(&enrichment_pool)
        .await
        .context("enrichment_cache migrate")?;
    let enrichment_cache: Arc<dyn nexo_microapp_sdk::enrichment::EnrichmentCache> =
        Arc::new(nexo_microapp_sdk::enrichment::cache::SqliteEnrichmentCache::new(
            enrichment_pool,
        ));
    let lid_pn_mappings: Arc<dyn nexo_microapp_sdk::identity::LidPnMappingStore> =
        Arc::new(
            nexo_microapp_sdk::identity::SqliteLidPnMappingStore::new(
                identity_pool.clone(),
            ),
        );
    // Phase 82.15.bx+ — domain scraper for corporate
    // auto-enrichment. Default config: 4 concurrent fetches,
    // 8s timeout, robots-aware. Cheap to clone (Arc semaphore
    // + reqwest pool) so the broker hop spawns scrape tasks
    // freely.
    let scraper = match nexo_marketing::enrichment::Scraper::new(
        nexo_marketing::enrichment::ScraperConfig::default(),
    ) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(
                target: "nexo_marketing",
                error = %e,
                "scraper init failed; auto-enrichment disabled"
            );
            None
        }
    };
    let enrichment = scraper.map(|sc| {
        nexo_marketing::plugin::EnrichmentDeps::new(
            sc,
            enrichment_cache.clone(),
            companies.clone(),
        )
    });
    if enrichment.is_some() {
        tracing::info!(
            target: "nexo_marketing",
            "auto-enrichment ready (scraper + cache + companies)"
        );
    }
    let chain = Arc::new(FallbackChain::new(
        vec![Box::new(DisplayNameParser), Box::new(ReplyToReader)],
        0.7,
    ));
    let identity = IdentityDeps::new(persons.clone(), person_emails, chain)
        .with_person_phones(person_phones)
        .with_lid_pn_mappings(lid_pn_mappings);
    tracing::info!(tenant = %tenant, "identity stores + resolver chain ready");

    // M15.53 / F9 — single dedup cache shared between the broker
    // hop's notification publish (inbound emails) + the tools'
    // post-success publish (LeadTransitioned, MeetingIntent).
    // Same `Arc`, same TTL window, so a redelivery in either
    // surface is suppressed by the other.
    //
    // F25 — when the operator builds with `--features dedup-sled`
    // AND sets `MARKETING_DEDUP_SLED=1`, the cache opens a
    // sled keyspace under the tenant state dir so a NATS
    // redelivery after a process restart still suppresses
    // the duplicate publish. Default builds + unset env stay
    // in-memory (covers ~95 % of the threat).
    let dedup = Arc::new(build_dedup_cache(&state_root, &tenant)?);

    // M15.23.d — operator-supplied topic guardrails. Same
    // arc_swap pattern as the router + seller lookup so the
    // admin PUT can hot-swap a fresh set without a process
    // restart. Empty file ⇒ empty handle (no rules fire).
    let guardrail_rules =
        nexo_marketing::config::load_topic_guardrails(&state_root, &tenant)
            .unwrap_or_default();
    let guardrails = match nexo_marketing::guardrails::handle_from_rules(
        guardrail_rules,
    ) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(
                tenant = %tenant,
                error = %e,
                "topic_guardrails.yaml refused to compile — running with empty set"
            );
            nexo_marketing::guardrails::empty_handle()
        }
    };
    let broker_guardrails = guardrails.clone();

    // M15.23.c — AI decision audit log. One SQLite file per
    // tenant under `<state_root>/<tenant>/audit.db`, table
    // `marketing_audit_events`. Same `EventStore<T>` infra the
    // SDK ships for the firehose so producers (broker hop +
    // tools + notification publish) and the `/audit` query
    // endpoint share one Arc.
    let audit_db_path = tenant.state_dir(&state_root).join("audit.db");
    if let Some(parent) = audit_db_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create {}", parent.display()))?;
    }
    let audit_store = nexo_microapp_sdk::events::EventStore::<
        nexo_marketing::audit::AuditEvent,
    >::open(&audit_db_path, nexo_marketing::audit::AUDIT_TABLE)
    .await
    .with_context(|| format!("open audit log at {}", audit_db_path.display()))?;
    let audit_log = Arc::new(nexo_marketing::audit::AuditLog::new(Arc::new(
        audit_store,
    )));
    tracing::info!(
        tenant = %tenant,
        path = %audit_db_path.display(),
        "audit log ready"
    );

    // ─── Spam / promo filter (per-tenant rules) ────────────────
    // Migrate the schema on the same identity pool — single
    // SQLite file per tenant, same model as the enrichment
    // cache. The cache `Arc` is captured both by the broker hop
    // (read path) and the admin endpoints (invalidate-on-write
    // path) so PUT /admin/spam-filter takes effect on the very
    // next inbound.
    nexo_marketing::spam_filter::store::migrate(&identity_pool)
        .await
        .context("spam_filter migrate")?;
    let spam_filter_store =
        nexo_marketing::spam_filter::SpamFilterStore::new(identity_pool.clone());
    let spam_filter_cache =
        nexo_marketing::spam_filter::RulesCache::new(spam_filter_store.clone());
    tracing::info!(tenant = %tenant, "spam filter ready");

    // ─── Scoring config (per-tenant tunables) ──────────────────
    nexo_marketing::scoring::migrate_scoring(&identity_pool)
        .await
        .context("scoring migrate")?;
    let scoring_store =
        nexo_marketing::scoring::ScoringConfigStore::new(identity_pool.clone());
    let scoring_cache =
        nexo_marketing::scoring::ScoringConfigCache::new(scoring_store);
    tracing::info!(tenant = %tenant, "scoring config ready");

    // ─── Marketing on/off toggle ───────────────────────────────
    nexo_marketing::marketing_state::migrate(&identity_pool)
        .await
        .context("marketing_state migrate")?;
    let marketing_state_store =
        nexo_marketing::marketing_state::MarketingStateStore::new(identity_pool.clone());
    let marketing_state_cache =
        nexo_marketing::marketing_state::MarketingStateCache::new(marketing_state_store);
    tracing::info!(tenant = %tenant, "marketing on/off cache ready");

    // ─── Broker depth observability (audit fix #8) ─────────────
    let broker_metrics = nexo_marketing::broker_metrics::BrokerMetrics::new();
    let broker_metrics_for_closure = broker_metrics.clone();

    let plugin_deps = PluginDeps::new(tenant.clone(), lead_store.clone(), router.clone())
        .with_identity(identity.clone())
        .with_sellers(seller_lookup.clone())
        .with_templates(template_lookup.clone())
        .with_dedup(dedup.clone())
        .with_audit(audit_log.clone())
        .with_spam_filter(spam_filter_cache.clone())
        .with_scoring(scoring_cache.clone())
        .with_marketing_state(marketing_state_cache.clone());

    // ─── Lead lifecycle bus (firehose) ────────────────────────
    // Shared between the broker handler (producer) and the
    // `/firehose` SSE route (consumer). Same Arc instance —
    // otherwise events vanish into a parallel universe.
    let firehose_bus = Arc::new(LeadEventBus::new());

    // ─── Tracking (M15.23.a) ──────────────────────────────────
    // Optional. Operator opts in by setting both
    // `MARKETING_TRACKING_SECRET` (≥ 16 random bytes) +
    // `MARKETING_TRACKING_BASE_URL` (public URL the recipients'
    // pixel + click hits resolve to). Both empty → tracking
    // stays off and the ingest routes self-404. Either set
    // without the other is a misconfiguration we surface at
    // boot.
    let tracking_deps = match (
        env::var("MARKETING_TRACKING_SECRET").ok(),
        env::var("MARKETING_TRACKING_BASE_URL").ok(),
    ) {
        (Some(secret), Some(base_url)) if !secret.is_empty() && !base_url.is_empty() => {
            let signer =
                nexo_microapp_sdk::tracking::TrackingTokenSigner::new(
                    secret.into_bytes(),
                )
                .context("MARKETING_TRACKING_SECRET must be ≥ 16 bytes")?;
            let pool_path = tenant.state_dir(&state_root).join("tracking.db");
            if let Some(parent) = pool_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            let pool = nexo_microapp_sdk::tracking::open_pool(&pool_path)
                .await
                .with_context(|| {
                    format!("open tracking pool at {}", pool_path.display())
                })?;
            let store: Arc<dyn nexo_microapp_sdk::tracking::TrackingStore> =
                Arc::new(
                    nexo_microapp_sdk::tracking::SqliteTrackingStore::new(pool),
                );
            tracing::info!(
                tenant = %tenant,
                base_url = %base_url,
                "tracking enabled — pixel + click ingest mounted"
            );
            Some(Arc::new(nexo_marketing::tracking::TrackingDeps::new(
                signer, store, base_url,
            )))
        }
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!(
                "MARKETING_TRACKING_SECRET and MARKETING_TRACKING_BASE_URL must both be set or both unset"
            );
        }
        _ => {
            tracing::info!("tracking disabled — set MARKETING_TRACKING_SECRET + MARKETING_TRACKING_BASE_URL to enable");
            None
        }
    };

    // ─── Outbound publisher (M15.21 slice 2) ──────────────────
    // Single instance per process — same tenant boundary the
    // marketing extension already runs single-tenant under.
    // Compliance gate uses framework defaults
    // (anti-loop / opt-out / PII / rate-limit). Operator
    // overrides via the existing `compliance` module surface.
    let outbound_publisher = Arc::new(
        nexo_marketing::broker::OutboundPublisher::new(
            tenant.clone(),
            nexo_marketing::compliance::OutboundGate::with_defaults(),
        ),
    );
    // M15.21 slice 2 — `BrokerSender` cell. Populated lazily
    // on the first `on_broker_event` invocation; the admin
    // approve handler reads from it. Cloned `Arc` shared
    // between the closure (writer) and AdminState (reader).
    let broker_sender_cell: Arc<
        arc_swap::ArcSwapOption<nexo_microapp_sdk::plugin::BrokerSender>,
    > = Arc::new(arc_swap::ArcSwapOption::empty());

    // ─── Draft generator (M15.21 slice 4) ─────────────────────
    // Default impl: sandboxed-Handlebars rendering of the
    // bundled template. Future deployments swap in
    // `AgentDraftGenerator` (NATS-RPC to the bound agent's
    // LLM) by replacing this Arc — admin surface stays the
    // same since both impls satisfy `DraftGenerator`.
    //
    // Hot-swappable template: boot seeds the handle from
    // disk (when the operator has dropped a per-tenant
    // `draft_template.hbs`) and otherwise from the bundled
    // default. AdminState shares the same Arc so
    // `PUT /config/draft_template` lands the new template
    // straight into the generator's read path.
    let draft_template_handle = nexo_marketing::draft::default_template_handle();
    if let Ok(Some(body)) =
        nexo_marketing::config::load_draft_template(&state_root, &tenant)
    {
        draft_template_handle.store(Arc::new(body));
        tracing::info!(
            tenant = %tenant,
            "loaded per-tenant draft template from disk"
        );
    }
    let template_gen =
        nexo_marketing::draft::TemplateDraftGenerator::from_handle(
            draft_template_handle.clone(),
        );
    // Phase 82.10.t.x — wrap the template generator in the
    // LLM-driven `AgentDraftGenerator`. Reads the seller's
    // denormalised `system_prompt` + `ModelRef` (stamped by
    // the operator microapp at PUT-time), builds a multi-turn
    // chat from `ctx.thread_history`, and calls the daemon's
    // `complete_llm` plugin RPC. Falls back to the template
    // generator on any failure path so the operator always
    // gets a draft to review.
    let agent_gen = nexo_marketing::draft::AgentDraftGenerator::new(
        broker_sender_cell.clone(),
        template_gen,
    );
    let draft_generator: Arc<
        dyn nexo_marketing::draft::DraftGenerator + Send + Sync,
    > = Arc::new(agent_gen);

    // ─── Surface 1: HTTP admin ────────────────────────────────
    let mut admin_state_builder = AdminState::new(bearer)
        .with_store(lead_store.clone())
        .with_firehose(firehose_bus.clone())
        .with_state_root(state_root.clone())
        .with_router(router.clone())
        .with_seller_lookup(seller_lookup.clone())
        .with_template_lookup(template_lookup.clone())
        .with_audit(audit_log.clone())
        .with_guardrails(guardrails.clone())
        .with_outbound(outbound_publisher.clone())
        .with_broker_sender_cell(broker_sender_cell.clone())
        .with_draft_generator(draft_generator)
        .with_draft_template(draft_template_handle.clone())
        .with_persons(persons.clone())
        .with_companies(companies.clone())
        .with_spam_filter(spam_filter_cache.clone())
        .with_scoring(scoring_cache.clone())
        .with_marketing_state(marketing_state_cache.clone())
        .with_broker_metrics(broker_metrics.clone());
    if let Some(deps) = tracking_deps.clone() {
        admin_state_builder = admin_state_builder
            .with_tracking(deps.clone())
            .with_tracking_for_outbound(deps);
    }
    let admin_state = Arc::new(admin_state_builder);
    let app = admin::router(admin_state);
    let bind = format!("{DEFAULT_BIND}:{port}");
    let addr: SocketAddr = bind.parse().context("parse bind addr")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, "admin HTTP listening");
    let http_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "admin HTTP server crashed");
        }
    });

    // ─── Surface 2: stdio JSON-RPC plugin contract ────────────
    let dispatch_deps = plugin_deps.clone();
    let broker_store = lead_store.clone();
    let broker_router = router.clone();
    let broker_identity = identity.clone();
    let broker_tenant = tenant.clone();
    let broker_firehose = firehose_bus.clone();
    let broker_sellers = seller_lookup.clone();
    let broker_templates = template_lookup.clone();
    let broker_dedup = dedup.clone();
    let broker_sender_cell_for_closure = broker_sender_cell.clone();
    let broker_audit = audit_log.clone();
    // Phase 82.15.bx+ — clone the auto-enrichment triplet for
    // capture by the broker hop closure. `Option<EnrichmentDeps>`
    // — `None` keeps today's behaviour when scraper init failed
    // (network sandbox, e.g.).
    let broker_enrichment = enrichment.clone();
    // Spam filter cache: same Arc pair (cache + admin) so a PUT
    // /admin/spam-filter on the http server invalidates the
    // entry the broker hop will read on the next inbound.
    let broker_spam_filter = spam_filter_cache.clone();
    let broker_scoring = scoring_cache.clone();
    let broker_marketing_state = marketing_state_cache.clone();
    // M15.23.e WA half — tenant resolver dedicated to
    // WhatsApp inbounds. Falls back to the same default
    // tenant the email side uses so single-tenant operators
    // don't need extra config; multi-tenant operators map
    // WA instance labels via the same `MARKETING_TENANT_ID`
    // env (extended later to a YAML when multi-WA-instance
    // deployments arrive).
    let whatsapp_resolver: Arc<nexo_marketing::broker::StaticTenantResolver> = Arc::new(
        nexo_marketing::broker::StaticTenantResolver::new(std::iter::empty())
            .with_default(tenant.clone()),
    );
    let broker_whatsapp_resolver = whatsapp_resolver.clone();
    PluginAdapter::new(MANIFEST)?
        .with_server_version(version)
        .declare_tools(marketing_tool_defs())
        // Phase 81.17.c.ctx — tools get a ToolContext with
        // BrokerSender so M15.41 can publish LeadTransitioned
        // + MeetingIntent notifications from inside tool
        // bodies. Plain `on_tool` works too for backwards-
        // compat (browser plugin uses it); ToolContext is
        // additive.
        .on_tool_with_context(move |inv: ToolInvocation, ctx: ToolContext| {
            let deps = dispatch_deps.clone();
            async move { plugin_dispatch(deps, inv, Some(&ctx)).await }
        })
        .on_broker_event(
            move |topic: String, event: BrokerEvent, broker: BrokerSender| {
                // M15.21 slice 2 — capture the BrokerSender on
                // every event so the admin approve handler
                // (running in a separate axum task) can publish
                // outbound. Cheap clone (Arc internals); idempotent
                // — store-overwrite pattern means later events
                // can refresh the cached handle if the SDK ever
                // rotates it.
                broker_sender_cell_for_closure
                    .store(Some(Arc::new(broker.clone())));
                let store = broker_store.clone();
                let router_handle = broker_router.clone();
                let identity = broker_identity.clone();
                let tenant = broker_tenant.clone();
                let firehose = broker_firehose.clone();
                let sellers = broker_sellers.clone();
                let templates = broker_templates.clone();
                let dedup = broker_dedup.clone();
                let audit_log = broker_audit.clone();
                let guardrails = broker_guardrails.clone();
                let whatsapp_resolver = broker_whatsapp_resolver.clone();
                let enrichment = broker_enrichment.clone();
                let spam_filter = broker_spam_filter.clone();
                let scoring_clone = broker_scoring.clone();
                let mstate_clone = broker_marketing_state.clone();
                let metrics = broker_metrics_for_closure.clone();
                async move {
                    // Audit fix #8 — depth tracking guard. RAII
                    // dec on drop so an early return / panic
                    // doesn't leave the counter stuck.
                    let _depth_guard = metrics.enter();
                    // M15.39 — single broker subscriber, two
                    // dispatchers. Topic prefix routes between:
                    //   - inbound email pipeline (lead create /
                    //     bump / firehose / notify publish)
                    //   - notification forwarder (route to WA /
                    //     email outbound based on baked target)
                    if topic.starts_with("agent.email.notification") {
                        let _ = nexo_marketing::forwarder::handle_notification_event(
                            &topic,
                            event.payload,
                            &broker,
                        )
                        .await;
                        return;
                    }
                    // M15.23.e WA half — WhatsApp inbound
                    // ingest. Decoupled from the email
                    // pipeline: only Person + PersonPhone
                    // upsert. The agent runtime handles the
                    // message body.
                    if topic.starts_with("plugin.inbound.whatsapp") {
                        if let Some(phone_store) =
                            identity.person_phones.clone()
                        {
                            let _ = nexo_marketing::whatsapp_ingest::handle_inbound_whatsapp_event(
                                &topic,
                                event.payload,
                                identity.persons.clone(),
                                phone_store,
                                identity.lid_pn_mappings.clone(),
                                whatsapp_resolver.as_ref(),
                                Some(audit_log.as_ref()),
                            )
                            .await;
                        }
                        return;
                    }
                    // `load_full()` is a lock-free atomic Arc
                    // bump — we get a snapshot of the current
                    // router that won't disappear under us
                    // even if `PUT /config/rules` swaps.
                    let router_snapshot = router_handle.load_full();
                    let _ = handle_inbound_event(
                        &topic,
                        event.payload,
                        &tenant,
                        &store,
                        Some(router_snapshot.as_ref()),
                        Some(&identity),
                        Some(firehose.as_ref()),
                        Some(&sellers),
                        Some(&templates),
                        Some(broker),
                        Some(dedup.as_ref()),
                        Some(audit_log.as_ref()),
                        Some(&guardrails),
                        enrichment.as_ref(),
                        Some(&spam_filter),
                        Some(&scoring_clone),
                        Some(&mstate_clone),
                    )
                    .await;
                    metrics.record_processed();
                }
            },
        )
        .run_stdio()
        .await
        .context("plugin stdio loop crashed")?;

    // When the daemon spawns this binary as a real plugin
    // subprocess, stdin is the plugin contract pipe and
    // `run_stdio` blocks until the daemon shuts us down. When
    // a sibling-process launcher (e.g. `scripts/dev-daemon.sh`
    // backgrounding the binary) has no plugin-contract pipe,
    // stdin EOFs immediately and `run_stdio` returns Ok(()) —
    // dropping the spawned HTTP task because main() exits.
    // Keep the binary alive on the HTTP task so the admin
    // surface (`/api/marketing/*` proxy target) stays
    // reachable until something explicitly stops the process.
    if let Err(e) = http_handle.await {
        tracing::warn!(error = %e, "admin HTTP task aborted");
    }
    Ok(())
}

/// F25 — build the notification dedup cache with the right
/// backend for the operator's deployment posture.
///
/// Build matrix:
/// - Default build (no `dedup-sled` feature): in-memory
///   only.
/// - `dedup-sled` feature compiled AND `MARKETING_DEDUP_SLED=1`
///   env set ⇒ persistent sled keyspace at
///   `<state_root>/<tenant>/notification_dedup.sled`.
/// - `dedup-sled` feature compiled but env unset ⇒ stays
///   in-memory. Operator opts in explicitly.
fn build_dedup_cache(
    state_root: &std::path::Path,
    tenant: &nexo_marketing::tenant::TenantId,
) -> anyhow::Result<nexo_marketing::notification_dedup::DedupCache> {
    #[cfg(feature = "dedup-sled")]
    {
        let opt_in = env::var("MARKETING_DEDUP_SLED")
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        if opt_in {
            let path = tenant
                .state_dir(state_root)
                .join("notification_dedup.sled");
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            tracing::info!(
                tenant = %tenant,
                path = %path.display(),
                "notification dedup using sled (cross-restart) backend"
            );
            return nexo_marketing::notification_dedup::DedupCache::with_sled(
                &path,
                nexo_marketing::notification_dedup::DEFAULT_TTL,
            )
            .with_context(|| {
                format!(
                    "open notification_dedup sled at {}",
                    path.display()
                )
            });
        }
    }
    let _ = state_root;
    let _ = tenant;
    Ok(nexo_marketing::notification_dedup::DedupCache::new())
}
