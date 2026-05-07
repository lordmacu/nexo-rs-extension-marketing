//! Per-tenant sqlite store for leads.
//!
//! Each tenant gets its own DB file under
//! `${state_root}/marketing/<tenant_id>/leads.db`. Stronger
//! isolation than a shared schema — operator query bug can't
//! accidentally cross tenants because the file boundary is
//! the tenant boundary.
//!
//! The store works with `Lead` from `nexo_tool_meta::marketing`
//! so the wire shape stays bit-equivalent across extension /
//! microapp / frontend.

use serde_json;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::path::Path;
use std::str::FromStr;

use crate::error::MarketingError;
use crate::lead::state::validate_transition;
use crate::tenant::TenantId;
use nexo_tool_meta::marketing::{
    IntentClass, Lead, LeadId, LeadState, PersonId, SentimentBand, TenantIdRef, VendedorId,
};

/// Input for `LeadStore::create_lead`. Caller fills the fields
/// they have at creation time; the store stamps `state =
/// Cold`, `score = 0`, `sentiment = Neutral`, `intent =
/// Browsing` until the agent's tools update them.
#[derive(Debug, Clone)]
pub struct NewLead {
    pub id: LeadId,
    pub thread_id: String,
    pub subject: String,
    pub person_id: PersonId,
    pub vendedor_id: VendedorId,
    pub last_activity_ms: i64,
    pub why_routed: Vec<String>,
}

const MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS leads (
    id                  TEXT NOT NULL,
    tenant_id           TEXT NOT NULL,
    thread_id           TEXT NOT NULL,
    subject             TEXT NOT NULL,
    person_id           TEXT NOT NULL,
    vendedor_id         TEXT NOT NULL,
    state               TEXT NOT NULL,
    score               INTEGER NOT NULL DEFAULT 0,
    sentiment           TEXT NOT NULL DEFAULT 'neutral',
    intent              TEXT NOT NULL DEFAULT 'browsing',
    topic_tags_json     TEXT NOT NULL DEFAULT '[]',
    last_activity_ms    INTEGER NOT NULL,
    next_check_at_ms    INTEGER,
    followup_attempts   INTEGER NOT NULL DEFAULT 0,
    why_routed_json     TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (tenant_id, id)
);
CREATE INDEX IF NOT EXISTS idx_leads_thread          ON leads(tenant_id, thread_id);
CREATE INDEX IF NOT EXISTS idx_leads_person          ON leads(tenant_id, person_id);
CREATE INDEX IF NOT EXISTS idx_leads_vendedor_state  ON leads(tenant_id, vendedor_id, state);
CREATE INDEX IF NOT EXISTS idx_leads_next_check      ON leads(tenant_id, next_check_at_ms)
    WHERE next_check_at_ms IS NOT NULL;
"#;

/// Per-tenant lead store. The struct holds its tenant id +
/// pool; `Clone` is cheap (Arc-shared pool) so callers spawn
/// without copying. Cross-tenant access is impossible by
/// construction — the file path encodes the tenant.
#[derive(Clone)]
pub struct LeadStore {
    pool: SqlitePool,
    tenant_id: TenantId,
}

impl LeadStore {
    /// Open or create the per-tenant DB at
    /// `<state_root>/marketing/<tenant_id>/leads.db`. Pass
    /// `:memory:` as `state_root` for tests (every tenant
    /// gets a fresh in-memory pool — pools don't share since
    /// each `:memory:` URI opens its own database).
    pub async fn open(
        state_root: impl AsRef<Path>,
        tenant_id: TenantId,
    ) -> Result<Self, MarketingError> {
        let pool = open_pool(state_root.as_ref(), &tenant_id).await?;
        sqlx::query(MIGRATION_SQL).execute(&pool).await?;
        Ok(Self { pool, tenant_id })
    }

    pub fn tenant_id(&self) -> &TenantId {
        &self.tenant_id
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Insert a new lead in `Cold` state. Idempotent on
    /// `(tenant_id, id)` — re-running with the same id is a
    /// no-op (returns the existing row).
    pub async fn create(&self, input: NewLead) -> Result<Lead, MarketingError> {
        let why_json = serde_json::to_string(&input.why_routed)
            .unwrap_or_else(|_| "[]".to_string());
        sqlx::query(
            "INSERT INTO leads \
             (id, tenant_id, thread_id, subject, person_id, vendedor_id, \
              state, score, sentiment, intent, topic_tags_json, \
              last_activity_ms, next_check_at_ms, followup_attempts, why_routed_json) \
             VALUES (?,?,?,?,?,?,'cold',0,'neutral','browsing','[]',?,?,0,?) \
             ON CONFLICT(tenant_id, id) DO NOTHING",
        )
        .bind(&input.id.0)
        .bind(self.tenant_id.as_str())
        .bind(&input.thread_id)
        .bind(&input.subject)
        .bind(&input.person_id.0)
        .bind(&input.vendedor_id.0)
        .bind(input.last_activity_ms)
        .bind(Option::<i64>::None)
        .bind(&why_json)
        .execute(&self.pool)
        .await?;
        self.get(&input.id)
            .await?
            .ok_or_else(|| MarketingError::Sqlite(sqlx::Error::RowNotFound))
    }

    /// Fetch by lead id. `None` when no row exists.
    pub async fn get(&self, lead_id: &LeadId) -> Result<Option<Lead>, MarketingError> {
        let row = sqlx::query_as::<_, LeadRow>(SELECT_LEAD)
            .bind(self.tenant_id.as_str())
            .bind(&lead_id.0)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(LeadRow::into_lead))
    }

    /// Look up by thread id — there's at most one lead per
    /// thread per tenant (the email plugin's threading.rs
    /// produces stable thread ids). Used by the inbound
    /// pipeline to decide create-vs-update.
    pub async fn find_by_thread(
        &self,
        thread_id: &str,
    ) -> Result<Option<Lead>, MarketingError> {
        let row = sqlx::query_as::<_, LeadRow>(
            "SELECT id, tenant_id, thread_id, subject, person_id, vendedor_id, \
                    state, score, sentiment, intent, topic_tags_json, \
                    last_activity_ms, next_check_at_ms, followup_attempts, why_routed_json \
             FROM leads WHERE tenant_id = ? AND thread_id = ?",
        )
        .bind(self.tenant_id.as_str())
        .bind(thread_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(LeadRow::into_lead))
    }

    /// Apply a state transition. Validates the legal-transition
    /// table first; updates the row if legal; returns the
    /// updated row. Caller stamps `last_activity_ms = now`
    /// outside this method when appropriate.
    ///
    /// Caller is responsible for emitting the
    /// `agent.lead.transition.<tenant_id>.<lead_id>` NATS
    /// event — the store doesn't know the broker. Keeps tests
    /// fast + sync.
    pub async fn transition(
        &self,
        lead_id: &LeadId,
        to: LeadState,
    ) -> Result<Lead, MarketingError> {
        let current = self
            .get(lead_id)
            .await?
            .ok_or_else(|| MarketingError::Sqlite(sqlx::Error::RowNotFound))?;
        validate_transition(lead_id, current.state, to)?;
        sqlx::query(
            "UPDATE leads SET state = ? WHERE tenant_id = ? AND id = ?",
        )
        .bind(state_str(to))
        .bind(self.tenant_id.as_str())
        .bind(&lead_id.0)
        .execute(&self.pool)
        .await?;
        self.get(lead_id)
            .await?
            .ok_or_else(|| MarketingError::Sqlite(sqlx::Error::RowNotFound))
    }

    /// Set / clear the next followup deadline. Pass `None` to
    /// cancel the followup (e.g. client replied + `stop_on_reply`
    /// was true).
    pub async fn set_next_check(
        &self,
        lead_id: &LeadId,
        next_check_at_ms: Option<i64>,
        increment_attempts: bool,
    ) -> Result<Lead, MarketingError> {
        if increment_attempts {
            sqlx::query(
                "UPDATE leads SET next_check_at_ms = ?, \
                  followup_attempts = followup_attempts + 1 \
                 WHERE tenant_id = ? AND id = ?",
            )
            .bind(next_check_at_ms)
            .bind(self.tenant_id.as_str())
            .bind(&lead_id.0)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                "UPDATE leads SET next_check_at_ms = ? \
                 WHERE tenant_id = ? AND id = ?",
            )
            .bind(next_check_at_ms)
            .bind(self.tenant_id.as_str())
            .bind(&lead_id.0)
            .execute(&self.pool)
            .await?;
        }
        self.get(lead_id)
            .await?
            .ok_or_else(|| MarketingError::Sqlite(sqlx::Error::RowNotFound))
    }

    /// Iterate leads with `next_check_at_ms <= now_ms`. Used
    /// by the followup sweep tool. Returns at most `limit`
    /// rows so a swamped tenant doesn't blow the cron tick.
    pub async fn list_due_for_followup(
        &self,
        now_ms: i64,
        limit: u32,
    ) -> Result<Vec<Lead>, MarketingError> {
        let rows = sqlx::query_as::<_, LeadRow>(
            "SELECT id, tenant_id, thread_id, subject, person_id, vendedor_id, \
                    state, score, sentiment, intent, topic_tags_json, \
                    last_activity_ms, next_check_at_ms, followup_attempts, why_routed_json \
             FROM leads \
             WHERE tenant_id = ? AND next_check_at_ms IS NOT NULL \
                                 AND next_check_at_ms <= ? \
             ORDER BY next_check_at_ms ASC \
             LIMIT ?",
        )
        .bind(self.tenant_id.as_str())
        .bind(now_ms)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(LeadRow::into_lead).collect())
    }

    /// Count leads in a given state (for telemetry +
    /// dashboard panels).
    pub async fn count_by_state(&self, state: LeadState) -> Result<i64, MarketingError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM leads WHERE tenant_id = ? AND state = ?",
        )
        .bind(self.tenant_id.as_str())
        .bind(state_str(state))
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }
}

// ── Row mapping ────────────────────────────────────────────────

const SELECT_LEAD: &str =
    "SELECT id, tenant_id, thread_id, subject, person_id, vendedor_id, \
            state, score, sentiment, intent, topic_tags_json, \
            last_activity_ms, next_check_at_ms, followup_attempts, why_routed_json \
     FROM leads WHERE tenant_id = ? AND id = ?";

#[derive(Debug, sqlx::FromRow)]
struct LeadRow {
    id: String,
    tenant_id: String,
    thread_id: String,
    subject: String,
    person_id: String,
    vendedor_id: String,
    state: String,
    score: i64,
    sentiment: String,
    intent: String,
    topic_tags_json: String,
    last_activity_ms: i64,
    next_check_at_ms: Option<i64>,
    followup_attempts: i64,
    why_routed_json: String,
}

impl LeadRow {
    fn into_lead(self) -> Lead {
        let topic_tags: Vec<String> =
            serde_json::from_str(&self.topic_tags_json).unwrap_or_default();
        let why_routed: Vec<String> =
            serde_json::from_str(&self.why_routed_json).unwrap_or_default();
        Lead {
            id: LeadId(self.id),
            tenant_id: TenantIdRef(self.tenant_id),
            thread_id: self.thread_id,
            subject: self.subject,
            person_id: PersonId(self.person_id),
            vendedor_id: VendedorId(self.vendedor_id),
            state: parse_state(&self.state),
            score: self.score.clamp(0, 100) as u8,
            sentiment: parse_sentiment(&self.sentiment),
            intent: parse_intent(&self.intent),
            topic_tags,
            last_activity_ms: self.last_activity_ms,
            next_check_at_ms: self.next_check_at_ms,
            followup_attempts: self.followup_attempts.clamp(0, 255) as u8,
            why_routed,
        }
    }
}

fn state_str(s: LeadState) -> &'static str {
    match s {
        LeadState::Cold => "cold",
        LeadState::Engaged => "engaged",
        LeadState::MeetingScheduled => "meeting_scheduled",
        LeadState::Qualified => "qualified",
        LeadState::Lost => "lost",
    }
}

fn parse_state(s: &str) -> LeadState {
    match s {
        "engaged" => LeadState::Engaged,
        "meeting_scheduled" => LeadState::MeetingScheduled,
        "qualified" => LeadState::Qualified,
        "lost" => LeadState::Lost,
        _ => LeadState::Cold,
    }
}

fn parse_sentiment(s: &str) -> SentimentBand {
    match s {
        "very_negative" => SentimentBand::VeryNegative,
        "negative" => SentimentBand::Negative,
        "positive" => SentimentBand::Positive,
        "very_positive" => SentimentBand::VeryPositive,
        _ => SentimentBand::Neutral,
    }
}

fn parse_intent(s: &str) -> IntentClass {
    match s {
        "comparing" => IntentClass::Comparing,
        "ready_to_buy" => IntentClass::ReadyToBuy,
        "objecting" => IntentClass::Objecting,
        "support_request" => IntentClass::SupportRequest,
        "out_of_scope" => IntentClass::OutOfScope,
        _ => IntentClass::Browsing,
    }
}

async fn open_pool(
    state_root: &Path,
    tenant_id: &TenantId,
) -> Result<SqlitePool, MarketingError> {
    let conn_str = if state_root.to_string_lossy() == ":memory:" {
        // Pure in-memory: each pool is a fresh DB instance
        // because sqlx::SqlitePool with `sqlite::memory:` opens
        // its own backing database per connection — fine for
        // single-conn pools (we set max=1 below for in-memory).
        "sqlite::memory:".to_string()
    } else {
        let dir = tenant_id.state_dir(state_root);
        std::fs::create_dir_all(&dir)
            .map_err(|e| MarketingError::Config(format!("create dir: {e}")))?;
        let db_path = dir.join("leads.db");
        format!("sqlite://{}", db_path.display())
    };
    let opts = SqliteConnectOptions::from_str(&conn_str)
        .map_err(|e| MarketingError::Config(e.to_string()))?
        .create_if_missing(true);
    // For in-memory, max=1 guarantees the single connection
    // owns the DB for the pool's lifetime; for file-backed,
    // 2 lets WAL readers + writers coexist.
    let max_conns = if state_root.to_string_lossy() == ":memory:" { 1 } else { 2 };
    let pool = SqlitePoolOptions::new()
        .max_connections(max_conns)
        .connect_with(opts)
        .await?;
    if state_root.to_string_lossy() != ":memory:" {
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await
            .ok();
    }
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    async fn fresh_store(t: &str) -> LeadStore {
        let tenant = TenantId::new(t).unwrap();
        LeadStore::open(PathBuf::from(":memory:"), tenant).await.unwrap()
    }

    fn input(id: &str, person: &str, vendedor: &str) -> NewLead {
        NewLead {
            id: LeadId(id.into()),
            thread_id: format!("th-{id}"),
            subject: "Re: cotización".into(),
            person_id: PersonId(person.into()),
            vendedor_id: VendedorId(vendedor.into()),
            last_activity_ms: 1_700_000_000_000,
            why_routed: vec!["fixture".into()],
        }
    }

    #[tokio::test]
    async fn create_then_get() {
        let s = fresh_store("acme").await;
        let lead = s.create(input("l1", "juan", "pedro")).await.unwrap();
        assert_eq!(lead.state, LeadState::Cold);
        let got = s.get(&LeadId("l1".into())).await.unwrap().unwrap();
        assert_eq!(got.id.0, "l1");
        assert_eq!(got.why_routed, vec!["fixture".to_string()]);
    }

    #[tokio::test]
    async fn create_idempotent() {
        let s = fresh_store("acme").await;
        s.create(input("l1", "juan", "pedro")).await.unwrap();
        s.create(input("l1", "juan", "pedro")).await.unwrap();
        // Still one row.
        let n = s.count_by_state(LeadState::Cold).await.unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn find_by_thread_returns_lead() {
        let s = fresh_store("acme").await;
        s.create(input("l1", "juan", "pedro")).await.unwrap();
        let got = s.find_by_thread("th-l1").await.unwrap().unwrap();
        assert_eq!(got.id.0, "l1");
    }

    #[tokio::test]
    async fn transition_cold_to_engaged_persists() {
        let s = fresh_store("acme").await;
        s.create(input("l1", "juan", "pedro")).await.unwrap();
        let updated = s
            .transition(&LeadId("l1".into()), LeadState::Engaged)
            .await
            .unwrap();
        assert_eq!(updated.state, LeadState::Engaged);
    }

    #[tokio::test]
    async fn transition_illegal_returns_typed_error() {
        let s = fresh_store("acme").await;
        s.create(input("l1", "juan", "pedro")).await.unwrap();
        let err = s
            .transition(&LeadId("l1".into()), LeadState::Qualified)
            .await
            .unwrap_err();
        assert!(matches!(err, MarketingError::InvalidTransition { .. }));
    }

    #[tokio::test]
    async fn set_next_check_increments_attempts() {
        let s = fresh_store("acme").await;
        s.create(input("l1", "juan", "pedro")).await.unwrap();
        let updated = s
            .set_next_check(&LeadId("l1".into()), Some(1_700_000_300_000), true)
            .await
            .unwrap();
        assert_eq!(updated.followup_attempts, 1);
        assert_eq!(updated.next_check_at_ms, Some(1_700_000_300_000));
    }

    #[tokio::test]
    async fn set_next_check_clear_keeps_attempts_when_not_incremented() {
        let s = fresh_store("acme").await;
        s.create(input("l1", "juan", "pedro")).await.unwrap();
        s.set_next_check(&LeadId("l1".into()), Some(1), true).await.unwrap();
        let updated = s
            .set_next_check(&LeadId("l1".into()), None, false)
            .await
            .unwrap();
        assert_eq!(updated.followup_attempts, 1);
        assert!(updated.next_check_at_ms.is_none());
    }

    #[tokio::test]
    async fn list_due_for_followup_filters_by_now() {
        let s = fresh_store("acme").await;
        s.create(input("due", "juan", "pedro")).await.unwrap();
        s.create(input("future", "ana", "pedro")).await.unwrap();
        s.set_next_check(&LeadId("due".into()), Some(100), false)
            .await
            .unwrap();
        s.set_next_check(&LeadId("future".into()), Some(10_000), false)
            .await
            .unwrap();
        let due = s.list_due_for_followup(500, 50).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id.0, "due");
    }

    #[tokio::test]
    async fn count_by_state_after_transition() {
        let s = fresh_store("acme").await;
        s.create(input("l1", "juan", "pedro")).await.unwrap();
        s.create(input("l2", "ana", "pedro")).await.unwrap();
        s.transition(&LeadId("l1".into()), LeadState::Engaged).await.unwrap();
        assert_eq!(s.count_by_state(LeadState::Cold).await.unwrap(), 1);
        assert_eq!(s.count_by_state(LeadState::Engaged).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn cross_tenant_isolation_via_separate_pools() {
        // Two tenants → two separate in-memory DBs (different
        // URIs because of the `_marketing_t=` suffix). Same
        // lead id 'l1' coexists as different rows.
        let s_acme = fresh_store("acme").await;
        let s_globex = fresh_store("globex").await;
        s_acme.create(input("l1", "juan", "pedro")).await.unwrap();
        // Globex's store sees no leads.
        let n = s_globex.count_by_state(LeadState::Cold).await.unwrap();
        assert_eq!(n, 0);
        // Acme's store still sees its own.
        let got = s_acme.get(&LeadId("l1".into())).await.unwrap();
        assert!(got.is_some());
    }

    #[tokio::test]
    async fn migration_idempotent_on_reopen() {
        let s = fresh_store("acme").await;
        // Re-running the migration on the same pool shouldn't
        // explode (CREATE IF NOT EXISTS covers it).
        sqlx::query(MIGRATION_SQL).execute(s.pool()).await.unwrap();
        let n = s.count_by_state(LeadState::Cold).await.unwrap();
        assert_eq!(n, 0);
    }
}
