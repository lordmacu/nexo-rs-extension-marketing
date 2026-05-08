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
const MANIFEST: &str = include_str!("../nexo-plugin.toml");

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
    // must be present for downstream company enrichment).
    let _ = SqliteCompanyStore::new(identity_pool.clone());
    let persons: Arc<dyn PersonStore> =
        Arc::new(SqlitePersonStore::new(identity_pool.clone()));
    let person_emails: Arc<dyn PersonEmailStore> =
        Arc::new(SqlitePersonEmailStore::new(identity_pool));
    let chain = Arc::new(FallbackChain::new(
        vec![Box::new(DisplayNameParser), Box::new(ReplyToReader)],
        0.7,
    ));
    let identity = IdentityDeps::new(persons, person_emails, chain);
    tracing::info!(tenant = %tenant, "identity stores + resolver chain ready");

    // M15.53 / F9 — single dedup cache shared between the broker
    // hop's notification publish (inbound emails) + the tools'
    // post-success publish (LeadTransitioned, MeetingIntent).
    // Same `Arc`, same TTL window, so a redelivery in either
    // surface is suppressed by the other.
    let dedup = Arc::new(nexo_marketing::notification_dedup::DedupCache::new());

    let plugin_deps = PluginDeps::new(tenant.clone(), lead_store.clone(), router.clone())
        .with_identity(identity.clone())
        .with_sellers(seller_lookup.clone())
        .with_templates(template_lookup.clone())
        .with_dedup(dedup.clone());

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

    // ─── Surface 1: HTTP admin ────────────────────────────────
    let mut admin_state_builder = AdminState::new(bearer)
        .with_store(lead_store.clone())
        .with_firehose(firehose_bus.clone())
        .with_state_root(state_root.clone())
        .with_router(router.clone())
        .with_seller_lookup(seller_lookup.clone())
        .with_template_lookup(template_lookup.clone());
    if let Some(deps) = tracking_deps.clone() {
        admin_state_builder = admin_state_builder.with_tracking(deps);
    }
    let admin_state = Arc::new(admin_state_builder);
    let app = admin::router(admin_state);
    let bind = format!("{DEFAULT_BIND}:{port}");
    let addr: SocketAddr = bind.parse().context("parse bind addr")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, "admin HTTP listening");
    tokio::spawn(async move {
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
                let store = broker_store.clone();
                let router_handle = broker_router.clone();
                let identity = broker_identity.clone();
                let tenant = broker_tenant.clone();
                let firehose = broker_firehose.clone();
                let sellers = broker_sellers.clone();
                let templates = broker_templates.clone();
                let dedup = broker_dedup.clone();
                async move {
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
                    )
                    .await;
                }
            },
        )
        .run_stdio()
        .await
        .context("plugin stdio loop crashed")?;

    Ok(())
}
