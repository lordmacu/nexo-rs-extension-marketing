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
use nexo_marketing::lead::{router::load_rule_set, LeadRouter, LeadStore};
use nexo_marketing::plugin::{
    broker::handle_inbound_event, dispatch::dispatch as plugin_dispatch,
    tool_defs::marketing_tool_defs, PluginDeps,
};
use nexo_marketing::tenant::TenantId;
use nexo_microapp_sdk::plugin::{BrokerSender, PluginAdapter, ToolInvocation};
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
    let router = Arc::new(LeadRouter::new(tenant.clone(), rule_set));

    let plugin_deps = PluginDeps::new(tenant.clone(), lead_store.clone(), router);

    // ─── Surface 1: HTTP admin ────────────────────────────────
    let admin_state = Arc::new(AdminState::new(bearer).with_store(lead_store.clone()));
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
    PluginAdapter::new(MANIFEST)?
        .with_server_version(version)
        .declare_tools(marketing_tool_defs())
        .on_tool(move |inv: ToolInvocation| {
            let deps = dispatch_deps.clone();
            async move { plugin_dispatch(deps, inv).await }
        })
        .on_broker_event(
            move |topic: String, event: BrokerEvent, broker: BrokerSender| {
                let store = broker_store.clone();
                async move {
                    let _ =
                        handle_inbound_event(&topic, event.payload, &store, Some(broker))
                            .await;
                }
            },
        )
        .run_stdio()
        .await
        .context("plugin stdio loop crashed")?;

    Ok(())
}
