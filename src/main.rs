// Subprocess entrypoint. The daemon's plugin-discovery walker
// finds this binary next to its `nexo-plugin.toml` (Phase 81.5)
// and spawns it as a long-lived child speaking JSON-RPC 2.0
// over stdio per `nexo-plugin-contract.md` v1.10.0.
//
// Today the binary boots the loopback admin HTTP server so the
// agent-creator microapp's `/api/marketing/*` proxy can talk
// to us. The plugin contract handshake (stdio JSON-RPC) +
// broker subscriber lands in the next milestone — this commit
// keeps the binary scaffolded around the admin surface.

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;

use nexo_marketing::admin::{self, AdminState};
use nexo_marketing::lead::LeadStore;
use nexo_marketing::tenant::TenantId;

const DEFAULT_BIND: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 18766;

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
    // (operator default install). Multi-tenant mounting lands in
    // Bloque H once the microapp wires the tenant→store map at
    // startup; for now the binary supports a single tenant which
    // covers the dev-daemon.sh smoke flow.
    let tenant = env::var("MARKETING_TENANT_ID")
        .ok()
        .and_then(|s| TenantId::new(s).ok())
        .unwrap_or_else(|| {
            TenantId::new("default").expect("'default' is a valid kebab-case tenant id")
        });

    let lead_store = LeadStore::open(&state_root, tenant.clone())
        .await
        .with_context(|| format!("open lead store at {}", state_root.display()))?;
    tracing::info!(tenant = %tenant, "lead store mounted");

    let admin_state = Arc::new(
        AdminState::new(bearer)
            .with_store(Arc::new(lead_store)),
    );
    let app = admin::router(admin_state);

    let bind = format!("{DEFAULT_BIND}:{port}");
    let addr: SocketAddr = bind.parse().context("parse bind addr")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, "admin HTTP listening");

    axum::serve(listener, app)
        .await
        .context("admin HTTP server crashed")?;

    Ok(())
}
