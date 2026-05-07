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
use std::sync::Arc;

use axum::routing::get;
use axum::Router;

use crate::lead::LeadStore;
use crate::tenant::TenantId;

pub mod auth;
pub mod healthz;
pub mod leads;

pub use auth::{require_tenant_id, AuthState, AUTH_HEADER, TENANT_HEADER};

/// Shared router state — Arc-cloned per request so axum
/// extractors get cheap access. Not `Clone` itself; consumers
/// share it via `axum::extract::State<Arc<AdminState>>`.
pub struct AdminState {
    pub bearer_token: String,
    pub stores: HashMap<TenantId, Arc<LeadStore>>,
}

impl AdminState {
    pub fn new(bearer_token: String) -> Self {
        Self {
            bearer_token,
            stores: HashMap::new(),
        }
    }

    pub fn with_store(mut self, store: Arc<LeadStore>) -> Self {
        self.stores
            .insert(store.tenant_id().clone(), store);
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
