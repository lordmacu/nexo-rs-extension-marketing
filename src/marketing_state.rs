//! Per-tenant marketing on/off toggle.
//!
//! Operator-facing kill switch so the operator can disable
//! every automated effect of the marketing extension while they
//! tune routing rules, edit prompts, swap a seller's agent,
//! etc., without losing inbound traffic during the edit window.
//!
//! ## Pause semantics
//!
//! "Disabled" means **don't fire automated effects**:
//! - Draft generation refuses (`/leads/:id/drafts/generate` →
//!   503 `marketing_paused`).
//! - Followup sweep tool returns `paused: true` + empty list
//!   so the agent runtime backs off.
//! - Notification publishes (LeadCreated / LeadReplied /
//!   MeetingIntent) skip.
//!
//! What still happens during pause:
//! - Inbound emails are still received + decoded + persisted
//!   as leads (so the operator's inbox keeps populating —
//!   they re-enable when ready and immediately have visibility
//!   into what arrived during the pause).
//! - Spam / promo filter still drops obvious noise pre-create
//!   (we don't want a paused tenant to also pause cleanup).
//! - Operator-driven actions (manual transition, manual
//!   draft create / edit / approve / send) keep working — the
//!   pause only halts AUTOMATED behaviour, not the operator's
//!   own actions.
//!
//! Re-enabling resumes everything. Existing leads + drafts +
//! threads are intact.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use thiserror::Error;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MarketingState {
    pub tenant_id: String,
    pub enabled: bool,
    /// Free-form note the operator left when toggling off so
    /// teammates know why the extension is paused. `None` /
    /// empty when enabled.
    pub paused_reason: Option<String>,
    pub updated_at_ms: i64,
}

impl MarketingState {
    pub fn enabled_for(tenant_id: &str, now_ms: i64) -> Self {
        Self {
            tenant_id: tenant_id.to_string(),
            enabled: true,
            paused_reason: None,
            updated_at_ms: now_ms,
        }
    }
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] sqlx::Error),
}

const MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS marketing_state (
    tenant_id     TEXT PRIMARY KEY,
    enabled       INTEGER NOT NULL DEFAULT 1,
    paused_reason TEXT,
    updated_at_ms INTEGER NOT NULL
);
"#;

/// Run the migration. Idempotent — safe on every boot.
pub async fn migrate(pool: &SqlitePool) -> Result<(), StateError> {
    sqlx::query(MIGRATION_SQL).execute(pool).await?;
    Ok(())
}

#[derive(Clone)]
pub struct MarketingStateStore {
    pool: SqlitePool,
}

impl MarketingStateStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn get(
        &self,
        tenant_id: &str,
        now_ms: i64,
    ) -> Result<MarketingState, StateError> {
        let row = sqlx::query(
            r#"SELECT enabled, paused_reason, updated_at_ms
               FROM marketing_state WHERE tenant_id = ?"#,
        )
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(MarketingState::enabled_for(tenant_id, now_ms));
        };
        Ok(MarketingState {
            tenant_id: tenant_id.to_string(),
            enabled: row.try_get::<i64, _>("enabled")? != 0,
            paused_reason: row.try_get("paused_reason")?,
            updated_at_ms: row.try_get("updated_at_ms")?,
        })
    }

    pub async fn put(&self, state: &MarketingState) -> Result<(), StateError> {
        sqlx::query(
            r#"INSERT INTO marketing_state
                 (tenant_id, enabled, paused_reason, updated_at_ms)
               VALUES (?, ?, ?, ?)
               ON CONFLICT(tenant_id) DO UPDATE SET
                  enabled       = excluded.enabled,
                  paused_reason = excluded.paused_reason,
                  updated_at_ms = excluded.updated_at_ms"#,
        )
        .bind(&state.tenant_id)
        .bind(state.enabled as i64)
        .bind(state.paused_reason.as_deref())
        .bind(state.updated_at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Hot-path cache: tenant_id → MarketingState. Read on every
/// gating decision (broker hop notification publish, draft
/// generate handler, followup sweep tool). Admin writes
/// invalidate.
#[derive(Clone)]
pub struct MarketingStateCache {
    store: MarketingStateStore,
    inner: Arc<RwLock<HashMap<String, MarketingState>>>,
}

impl MarketingStateCache {
    pub fn new(store: MarketingStateStore) -> Self {
        Self {
            store,
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn store(&self) -> &MarketingStateStore {
        &self.store
    }

    /// True when the tenant is enabled (default for un-set
    /// rows). On store failure, defaults to `true` so a
    /// transient SQLite hiccup never silently disables a
    /// production tenant.
    pub async fn is_enabled(&self, tenant_id: &str, now_ms: i64) -> bool {
        self.get(tenant_id, now_ms).await.enabled
    }

    pub async fn get(&self, tenant_id: &str, now_ms: i64) -> MarketingState {
        if let Some(hit) = self.inner.read().await.get(tenant_id).cloned() {
            return hit;
        }
        match self.store.get(tenant_id, now_ms).await {
            Ok(state) => {
                self.inner
                    .write()
                    .await
                    .insert(tenant_id.to_string(), state.clone());
                state
            }
            Err(e) => {
                tracing::warn!(
                    target: "marketing.state_cache",
                    tenant_id,
                    error = %e,
                    "marketing_state load failed; defaulting to enabled",
                );
                MarketingState::enabled_for(tenant_id, now_ms)
            }
        }
    }

    pub async fn invalidate(&self, tenant_id: &str) {
        self.inner.write().await.remove(tenant_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> SqlitePool {
        let p = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        migrate(&p).await.unwrap();
        p
    }

    #[tokio::test]
    async fn defaults_to_enabled_when_no_row() {
        let store = MarketingStateStore::new(pool().await);
        let s = store.get("acme", 1).await.unwrap();
        assert!(s.enabled);
        assert!(s.paused_reason.is_none());
    }

    #[tokio::test]
    async fn put_disabled_then_get_round_trips() {
        let store = MarketingStateStore::new(pool().await);
        let mut s = MarketingState::enabled_for("acme", 1);
        s.enabled = false;
        s.paused_reason = Some("tuning rules".into());
        s.updated_at_ms = 100;
        store.put(&s).await.unwrap();
        let back = store.get("acme", 999).await.unwrap();
        assert!(!back.enabled);
        assert_eq!(back.paused_reason.as_deref(), Some("tuning rules"));
        assert_eq!(back.updated_at_ms, 100);
    }

    #[tokio::test]
    async fn cache_invalidation_picks_up_new_value() {
        let store = MarketingStateStore::new(pool().await);
        let cache = MarketingStateCache::new(store.clone());

        // First read populates the cache with default-enabled.
        assert!(cache.is_enabled("acme", 1).await);

        // Backend write that bypasses the cache.
        let mut s = MarketingState::enabled_for("acme", 1);
        s.enabled = false;
        store.put(&s).await.unwrap();

        // Without invalidation: still cached as enabled.
        assert!(cache.is_enabled("acme", 2).await);

        // Invalidate → next read reflects disk truth.
        cache.invalidate("acme").await;
        assert!(!cache.is_enabled("acme", 3).await);
    }
}
