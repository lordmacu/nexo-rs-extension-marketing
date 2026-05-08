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
use crate::notification::SellerLookup;
use crate::tenant::TenantId;

pub mod auth;
pub mod config;
pub mod firehose;
pub mod healthz;
pub mod leads;

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
    Router::new()
        .route("/healthz", get(healthz::handler))
        .route("/leads", get(leads::list_handler))
        .route("/leads/:lead_id", get(leads::get_handler))
        .route("/leads/:lead_id/thread", get(leads::thread_handler))
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
        .route("/firehose", get(firehose::handler))
        .layer(auth_layer)
        .with_state(state)
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
