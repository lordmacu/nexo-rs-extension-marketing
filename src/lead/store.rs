//! Per-tenant sqlite store for leads.
//!
//! **F29 sweep:** marketing-specific by design. The columns
//! (`person_id`, `seller_id`, `state`, `topic_tags_json`,
//! `next_check_at_ms`, `followup_attempts`, `operator_notes`)
//! are CRM concepts — no other microapp would consume them
//! verbatim. Per-tenant file isolation is generic but the
//! SDK's `events` feature already covers that pattern; this
//! store layers a lead-shaped schema on top.
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
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;

use crate::error::MarketingError;
use crate::lead::state::validate_transition;
use crate::tenant::TenantId;
use nexo_tool_meta::marketing::{
    IntentClass, Lead, LeadId, LeadState, PersonId, SentimentBand, TenantIdRef, SellerId,
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
    pub seller_id: SellerId,
    pub last_activity_ms: i64,
    pub why_routed: Vec<String>,
    /// M15.23.f — initial heuristic score (0..=100). Caller
    /// runs the scorer at create time so the row lands with
    /// signal already attached. Default `0` keeps the broker
    /// hop's existing tests + the placeholder path working
    /// without touching every fixture.
    pub score: u8,
    /// M15.23.d — guardrail topic tags. One entry per
    /// guardrail rule that fired against the inbound body
    /// (`pricing_quotes`, `legal_questions`, …). Default
    /// empty.
    pub topic_tags: Vec<String>,
}

const MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS leads (
    id                  TEXT NOT NULL,
    tenant_id           TEXT NOT NULL,
    thread_id           TEXT NOT NULL,
    subject             TEXT NOT NULL,
    person_id           TEXT NOT NULL,
    seller_id         TEXT NOT NULL,
    state               TEXT NOT NULL,
    score               INTEGER NOT NULL DEFAULT 0,
    sentiment           TEXT NOT NULL DEFAULT 'neutral',
    intent              TEXT NOT NULL DEFAULT 'browsing',
    topic_tags_json     TEXT NOT NULL DEFAULT '[]',
    last_activity_ms    INTEGER NOT NULL,
    next_check_at_ms    INTEGER,
    followup_attempts   INTEGER NOT NULL DEFAULT 0,
    why_routed_json     TEXT NOT NULL DEFAULT '[]',
    -- M15.21.notes — free-form operator scratch pad (markdown).
    -- NULL = column never written; empty string is also legal
    -- and round-trips faithfully so the editor can clear it
    -- without re-introducing nulls.
    operator_notes      TEXT,
    PRIMARY KEY (tenant_id, id)
);
CREATE INDEX IF NOT EXISTS idx_leads_thread          ON leads(tenant_id, thread_id);
CREATE INDEX IF NOT EXISTS idx_leads_person          ON leads(tenant_id, person_id);
CREATE INDEX IF NOT EXISTS idx_leads_seller_state  ON leads(tenant_id, seller_id, state);
CREATE INDEX IF NOT EXISTS idx_leads_next_check      ON leads(tenant_id, next_check_at_ms)
    WHERE next_check_at_ms IS NOT NULL;

-- Outbound Message-Id → Lead mapping. Populated by the
-- compose handler (and any future call site that wants reply
-- threading) BEFORE the broker hop publishes the outbound.
-- The broker hop's inbound path queries this on every reply
-- so the recipient's `In-Reply-To` resolves to the
-- originating Lead even when the cold-outreach used a
-- synthetic thread_id.
CREATE TABLE IF NOT EXISTS outbound_message_ids (
    tenant_id      TEXT NOT NULL,
    rfc_message_id TEXT NOT NULL,
    lead_id        TEXT NOT NULL,
    thread_id      TEXT NOT NULL,
    sent_at_ms     INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, rfc_message_id)
);
CREATE INDEX IF NOT EXISTS idx_outbound_msg_ids_lead
    ON outbound_message_ids(tenant_id, lead_id);

CREATE TABLE IF NOT EXISTS thread_messages (
    tenant_id    TEXT NOT NULL,
    lead_id      TEXT NOT NULL,
    message_id   TEXT NOT NULL,
    direction    TEXT NOT NULL,
    from_label   TEXT NOT NULL,
    body         TEXT NOT NULL,
    at_ms        INTEGER NOT NULL,
    draft_status TEXT,
    subject      TEXT,
    -- Idempotent-draft signature: sha256 hex of the input
    -- identity tuple (lead state + last_inbound msg + seller +
    -- agent + prompt hash). Populated only on `direction =
    -- 'draft'` rows. Lookup target for the pre-LLM dedup gate
    -- in `generate_draft_handler` + the followup sweep tool.
    -- NULL on non-draft directions and on legacy draft rows
    -- created before the column landed.
    signature    TEXT,
    PRIMARY KEY (tenant_id, lead_id, message_id)
);
CREATE INDEX IF NOT EXISTS idx_thread_messages_lead
    ON thread_messages(tenant_id, lead_id, at_ms);
-- Hot path for `find_pending_draft_by_signature` — every
-- generate_draft request hits this index before deciding to
-- spend tokens on a fresh LLM call.
CREATE INDEX IF NOT EXISTS idx_thread_messages_pending_sig
    ON thread_messages(tenant_id, lead_id, signature)
    WHERE direction = 'draft' AND draft_status = 'pending'
      AND signature IS NOT NULL;
"#;

/// Idempotent ALTER for existing DBs that pre-date the
/// `subject` column. SQLite returns "duplicate column
/// name" when re-running on a fresh schema (column already
/// present from MIGRATION_SQL above) — we swallow that
/// case and propagate any other error.
const MIGRATION_SQL_ADD_SUBJECT: &str =
    "ALTER TABLE thread_messages ADD COLUMN subject TEXT";

/// M15.21.notes — same idempotent-ALTER discipline for the
/// new `operator_notes` column on the leads table. Pre-existing
/// DBs from earlier installs lack the column; running fresh
/// installs already get it via `MIGRATION_SQL`. The duplicate-
/// column error is the success signal for fresh installs.
const MIGRATION_SQL_ADD_OPERATOR_NOTES: &str =
    "ALTER TABLE leads ADD COLUMN operator_notes TEXT";

/// Outbound Message-Id → Lead mapping. New DBs already have it
/// from `MIGRATION_SQL`; idempotent CREATE for old DBs.
const MIGRATION_SQL_OUTBOUND_IDS: &str = r#"
CREATE TABLE IF NOT EXISTS outbound_message_ids (
    tenant_id      TEXT NOT NULL,
    rfc_message_id TEXT NOT NULL,
    lead_id        TEXT NOT NULL,
    thread_id      TEXT NOT NULL,
    sent_at_ms     INTEGER NOT NULL,
    PRIMARY KEY (tenant_id, rfc_message_id)
);
CREATE INDEX IF NOT EXISTS idx_outbound_msg_ids_lead
    ON outbound_message_ids(tenant_id, lead_id);
"#;

/// Idempotent-draft `signature` column on thread_messages.
/// Same swallow-duplicate-column discipline as the other
/// retroactive columns. Populated only on draft rows; the
/// dedup gate in `generate_draft_handler` reads via the
/// partial index `idx_thread_messages_pending_sig`.
const MIGRATION_SQL_ADD_SIGNATURE: &str =
    "ALTER TABLE thread_messages ADD COLUMN signature TEXT";

/// Partial index for the pending-draft dedup lookup. New DBs
/// already have it from `MIGRATION_SQL`; this ALTER targets
/// pre-existing DBs. `IF NOT EXISTS` makes it safe to re-run.
const MIGRATION_SQL_ADD_SIGNATURE_INDEX: &str = r#"
CREATE INDEX IF NOT EXISTS idx_thread_messages_pending_sig
    ON thread_messages(tenant_id, lead_id, signature)
    WHERE direction = 'draft' AND draft_status = 'pending'
      AND signature IS NOT NULL
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
        // Idempotent column-add for existing DBs created
        // before `subject` landed. New DBs already have it
        // from MIGRATION_SQL — the duplicate-column error
        // is the success signal there.
        if let Err(e) =
            sqlx::query(MIGRATION_SQL_ADD_SUBJECT).execute(&pool).await
        {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                return Err(MarketingError::Sqlite(e));
            }
        }
        // M15.21.notes — idempotent column-add on the leads
        // table. Same swallow-duplicate / propagate-others
        // discipline as the `subject` migration above.
        if let Err(e) = sqlx::query(MIGRATION_SQL_ADD_OPERATOR_NOTES)
            .execute(&pool)
            .await
        {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                return Err(MarketingError::Sqlite(e));
            }
        }
        // Idempotent-draft `signature` column. Same retro-add
        // discipline. The partial index is `IF NOT EXISTS` so
        // we run it unconditionally — it may already be present
        // on fresh DBs via `MIGRATION_SQL`.
        if let Err(e) = sqlx::query(MIGRATION_SQL_ADD_SIGNATURE)
            .execute(&pool)
            .await
        {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                return Err(MarketingError::Sqlite(e));
            }
        }
        sqlx::query(MIGRATION_SQL_ADD_SIGNATURE_INDEX)
            .execute(&pool)
            .await?;
        // Outbound Message-Id mapping table.
        sqlx::query(MIGRATION_SQL_OUTBOUND_IDS)
            .execute(&pool)
            .await?;
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
        let topic_tags_json = serde_json::to_string(&input.topic_tags)
            .unwrap_or_else(|_| "[]".to_string());
        sqlx::query(
            "INSERT INTO leads \
             (id, tenant_id, thread_id, subject, person_id, seller_id, \
              state, score, sentiment, intent, topic_tags_json, \
              last_activity_ms, next_check_at_ms, followup_attempts, why_routed_json) \
             VALUES (?,?,?,?,?,?,'cold',?,'neutral','browsing',?,?,?,0,?) \
             ON CONFLICT(tenant_id, id) DO NOTHING",
        )
        .bind(&input.id.0)
        .bind(self.tenant_id.as_str())
        .bind(&input.thread_id)
        .bind(&input.subject)
        .bind(&input.person_id.0)
        .bind(&input.seller_id.0)
        .bind(input.score as i64)
        .bind(&topic_tags_json)
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
            "SELECT id, tenant_id, thread_id, subject, person_id, seller_id, \
                    state, score, sentiment, intent, topic_tags_json, \
                    last_activity_ms, next_check_at_ms, followup_attempts, why_routed_json, \
                    operator_notes \
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
        // Audit fix #3 — terminal transitions auto-reject any
        // pending drafts so an operator can't accidentally
        // approve and send a draft to a lead that's already
        // closed (`qualified` = converted, `lost` = no-go).
        // Also clear `next_check_at_ms` so the cron stops
        // evaluating the lead.
        if matches!(to, LeadState::Qualified | LeadState::Lost) {
            let auto_rejected = self.reject_pending_drafts(lead_id).await?;
            sqlx::query(
                "UPDATE leads SET next_check_at_ms = NULL \
                 WHERE tenant_id = ? AND id = ?",
            )
            .bind(self.tenant_id.as_str())
            .bind(&lead_id.0)
            .execute(&self.pool)
            .await?;
            if auto_rejected > 0 {
                tracing::info!(
                    target: "marketing.lead.transition",
                    lead_id = %lead_id.0,
                    target_state = state_str(to),
                    auto_rejected_drafts = auto_rejected,
                    "terminal transition rejected pending drafts",
                );
            }
        }
        self.get(lead_id)
            .await?
            .ok_or_else(|| MarketingError::Sqlite(sqlx::Error::RowNotFound))
    }

    /// Mass-reject every `pending` draft for a lead. Used by
    /// `transition` on terminal states (qualified / lost) so an
    /// operator can't accidentally send a draft to an already-
    /// closed lead. Returns the count of rows affected so the
    /// caller can log / audit.
    pub async fn reject_pending_drafts(
        &self,
        lead_id: &LeadId,
    ) -> Result<u64, MarketingError> {
        let r = sqlx::query(
            "UPDATE thread_messages \
             SET draft_status = 'rejected' \
             WHERE tenant_id = ? AND lead_id = ? \
               AND direction = 'draft' AND draft_status = 'pending'",
        )
        .bind(self.tenant_id.as_str())
        .bind(&lead_id.0)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Apply uniform jitter inside `[-window/2, +window/2]` to a
    /// timestamp. Pure-fn over an injectable RNG so tests stay
    /// deterministic. Used by [`set_next_check_with_jitter`] to
    /// spread cron-driven followup ticks across a window so a
    /// burst of inbound traffic doesn't end up with N leads
    /// becoming due at the exact same millisecond.
    pub fn jitter_timestamp_ms<R: rand::Rng>(
        base_ms: i64,
        window_ms: u32,
        rng: &mut R,
    ) -> i64 {
        if window_ms == 0 {
            return base_ms;
        }
        let half = window_ms as i64 / 2;
        let offset = rng.gen_range(-half..=half);
        base_ms.saturating_add(offset)
    }

    /// Convenience over `set_next_check` that applies uniform
    /// jitter inside `[-window/2, +window/2]` ms before persist.
    /// `window_ms = 0` short-circuits to the exact value (used
    /// by operator-postpone where the timestamp is intentional).
    pub async fn set_next_check_with_jitter(
        &self,
        lead_id: &LeadId,
        next_check_at_ms: Option<i64>,
        increment_attempts: bool,
        jitter_window_ms: u32,
    ) -> Result<Lead, MarketingError> {
        let jittered = next_check_at_ms.map(|t| {
            Self::jitter_timestamp_ms(t, jitter_window_ms, &mut rand::thread_rng())
        });
        self.set_next_check(lead_id, jittered, increment_attempts).await
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

    /// M15.21.notes — replace the free-form `operator_notes`
    /// markdown for a lead. `Some(text)` writes verbatim
    /// (empty string allowed — round-trips faithfully so the
    /// editor can clear without re-introducing nulls).
    /// `None` writes SQL NULL so reads observe "field never
    /// authored". Returns the refreshed `Lead` so callers can
    /// stamp firehose / audit without a second round-trip.
    pub async fn update_operator_notes(
        &self,
        lead_id: &LeadId,
        notes: Option<String>,
    ) -> Result<Lead, MarketingError> {
        sqlx::query(
            "UPDATE leads SET operator_notes = ? \
             WHERE tenant_id = ? AND id = ?",
        )
        .bind(notes.as_deref())
        .bind(self.tenant_id.as_str())
        .bind(&lead_id.0)
        .execute(&self.pool)
        .await?;
        self.get(lead_id)
            .await?
            .ok_or_else(|| MarketingError::Sqlite(sqlx::Error::RowNotFound))
    }

    /// M15.24+ — list every lead in the tenant ordered by most
    /// recent activity. Powers the operator dashboard's "inbox"
    /// view (every received email regardless of follow-up state),
    /// distinct from [`Self::list_due_for_followup`] which the
    /// sweep loop consumes. `limit` caps the page so a tenant
    /// with thousands of cold leads doesn't blow the response;
    /// caller paginates via `last_activity_ms` cursor when needed.
    pub async fn list_all(&self, limit: u32) -> Result<Vec<Lead>, MarketingError> {
        let rows = sqlx::query_as::<_, LeadRow>(
            "SELECT id, tenant_id, thread_id, subject, person_id, seller_id, \
                    state, score, sentiment, intent, topic_tags_json, \
                    last_activity_ms, next_check_at_ms, followup_attempts, why_routed_json, \
                    operator_notes \
             FROM leads \
             WHERE tenant_id = ? \
             ORDER BY last_activity_ms DESC \
             LIMIT ?",
        )
        .bind(self.tenant_id.as_str())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(LeadRow::into_lead).collect())
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
            "SELECT id, tenant_id, thread_id, subject, person_id, seller_id, \
                    state, score, sentiment, intent, topic_tags_json, \
                    last_activity_ms, next_check_at_ms, followup_attempts, why_routed_json, \
                    operator_notes \
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

    /// M15.24 — count pending drafts across every lead.
    /// Powers the telemetry dashboard's "drafts awaiting
    /// approval" headline. Tenant-scoped by construction
    /// (the store carries its own `tenant_id`).
    pub async fn count_drafts_pending(&self) -> Result<i64, MarketingError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM thread_messages \
             WHERE tenant_id = ? AND direction = 'draft' \
             AND draft_status = 'pending'",
        )
        .bind(self.tenant_id.as_str())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Tenant-wide queue of pending drafts joined with their
    /// lead context. Powers the operator's "drafts inbox"
    /// where every pending row across every lead lands in
    /// one batch-approve list. Newest first so the operator
    /// sees fresh drafts at the top. `limit` clamps the
    /// result count; pass 0 for no extra rows.
    pub async fn list_pending_drafts_tenant_wide(
        &self,
        limit: u32,
    ) -> Result<Vec<PendingDraftRow>, MarketingError> {
        let rows: Vec<PendingDraftRowSql> = sqlx::query_as(
            "SELECT \
               m.lead_id AS lead_id, \
               m.message_id AS message_id, \
               m.from_label AS from_label, \
               m.body AS body, \
               m.at_ms AS at_ms, \
               l.subject AS lead_subject, \
               l.seller_id AS lead_seller_id, \
               l.person_id AS lead_person_id, \
               l.state AS lead_state \
             FROM thread_messages m \
             JOIN leads l \
               ON l.tenant_id = m.tenant_id AND l.id = m.lead_id \
             WHERE m.tenant_id = ? \
               AND m.direction = 'draft' \
               AND m.draft_status = 'pending' \
             ORDER BY m.at_ms DESC \
             LIMIT ?",
        )
        .bind(self.tenant_id.as_str())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(PendingDraftRowSql::into_row).collect())
    }

    /// M15.24 — count thread_messages by direction since
    /// `since_ms`. Powers the dashboard's "emails-in /
    /// emails-out (last 24h)" headline. Caller-supplied
    /// window so the same helper covers daily / weekly /
    /// monthly variants without a schema change.
    pub async fn count_messages_by_direction_since(
        &self,
        direction: MessageDirection,
        since_ms: i64,
    ) -> Result<i64, MarketingError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM thread_messages \
             WHERE tenant_id = ? AND direction = ? AND at_ms >= ?",
        )
        .bind(self.tenant_id.as_str())
        .bind(direction_str(direction))
        .bind(since_ms)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Append a message to a lead's thread. Idempotent on
    /// `(tenant_id, lead_id, message_id)` — re-running with the
    /// same message id is a no-op so the broker hop's natural
    /// at-least-once delivery doesn't duplicate rows. The
    /// caller's responsibility to make `message_id` stable
    /// (RFC 5322 `Message-Id` for inbound, draft uuid for
    /// drafts).
    pub async fn append_thread_message(
        &self,
        lead_id: &LeadId,
        msg: NewThreadMessage,
    ) -> Result<(), MarketingError> {
        sqlx::query(
            "INSERT INTO thread_messages \
             (tenant_id, lead_id, message_id, direction, from_label, body, at_ms, draft_status, subject, signature) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(tenant_id, lead_id, message_id) DO NOTHING",
        )
        .bind(self.tenant_id.as_str())
        .bind(&lead_id.0)
        .bind(&msg.message_id)
        .bind(direction_str(msg.direction))
        .bind(&msg.from_label)
        .bind(&msg.body)
        .bind(msg.at_ms)
        .bind(msg.draft_status.map(draft_status_str))
        .bind(msg.subject.as_deref())
        .bind(msg.signature.as_deref())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// M15.21 slice 1 — update a draft's body. Only works
    /// while the draft is `pending`; once approved or
    /// rejected the row is locked. Returns the rows-affected
    /// count so the caller can distinguish "not found / not
    /// pending" (0) from "updated" (1).
    pub async fn update_draft_body(
        &self,
        lead_id: &LeadId,
        message_id: &str,
        body: &str,
    ) -> Result<u64, MarketingError> {
        let r = sqlx::query(
            "UPDATE thread_messages \
             SET body = ? \
             WHERE tenant_id = ? AND lead_id = ? AND message_id = ? \
               AND direction = 'draft' AND draft_status = 'pending'",
        )
        .bind(body)
        .bind(self.tenant_id.as_str())
        .bind(&lead_id.0)
        .bind(message_id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Set / clear a draft's subject override. Same lock
    /// semantics as `update_draft_body` — only `pending`
    /// drafts mutate. Pass `None` to clear and inherit
    /// the lead subject at approve time.
    pub async fn update_draft_subject(
        &self,
        lead_id: &LeadId,
        message_id: &str,
        subject: Option<&str>,
    ) -> Result<u64, MarketingError> {
        let r = sqlx::query(
            "UPDATE thread_messages \
             SET subject = ? \
             WHERE tenant_id = ? AND lead_id = ? AND message_id = ? \
               AND direction = 'draft' AND draft_status = 'pending'",
        )
        .bind(subject)
        .bind(self.tenant_id.as_str())
        .bind(&lead_id.0)
        .bind(message_id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// M15.21 slice 1 — transition a draft's status. Refuses
    /// to update non-`draft` rows + non-`pending` drafts so a
    /// double-approve from the operator can't fire the
    /// publisher twice.
    pub async fn set_draft_status(
        &self,
        lead_id: &LeadId,
        message_id: &str,
        to: DraftStatus,
    ) -> Result<u64, MarketingError> {
        let r = sqlx::query(
            "UPDATE thread_messages \
             SET draft_status = ? \
             WHERE tenant_id = ? AND lead_id = ? AND message_id = ? \
               AND direction = 'draft' AND draft_status = 'pending'",
        )
        .bind(draft_status_str(to))
        .bind(self.tenant_id.as_str())
        .bind(&lead_id.0)
        .bind(message_id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// M15.21 slice 1 — operator dismisses a draft. Hard
    /// delete + tenant-scoped + `direction = draft`-scoped so
    /// an outbound or inbound row can't be deleted by mistake.
    pub async fn delete_draft(
        &self,
        lead_id: &LeadId,
        message_id: &str,
    ) -> Result<u64, MarketingError> {
        let r = sqlx::query(
            "DELETE FROM thread_messages \
             WHERE tenant_id = ? AND lead_id = ? AND message_id = ? \
               AND direction = 'draft'",
        )
        .bind(self.tenant_id.as_str())
        .bind(&lead_id.0)
        .bind(message_id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// M15.21 slice 1 — list every draft on a lead (any
    /// status). Caller-supplied `status_filter = Some(...)`
    /// narrows to a single state; `None` returns all three.
    pub async fn list_drafts(
        &self,
        lead_id: &LeadId,
        status_filter: Option<DraftStatus>,
    ) -> Result<Vec<ThreadMessage>, MarketingError> {
        let rows: Vec<ThreadMessageRow> = match status_filter {
            Some(status) => {
                sqlx::query_as(
                    "SELECT message_id, direction, from_label, body, at_ms, draft_status, subject \
                     FROM thread_messages \
                     WHERE tenant_id = ? AND lead_id = ? \
                       AND direction = 'draft' AND draft_status = ? \
                     ORDER BY at_ms ASC, message_id ASC",
                )
                .bind(self.tenant_id.as_str())
                .bind(&lead_id.0)
                .bind(draft_status_str(status))
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as(
                    "SELECT message_id, direction, from_label, body, at_ms, draft_status, subject \
                     FROM thread_messages \
                     WHERE tenant_id = ? AND lead_id = ? AND direction = 'draft' \
                     ORDER BY at_ms ASC, message_id ASC",
                )
                .bind(self.tenant_id.as_str())
                .bind(&lead_id.0)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows.into_iter().map(ThreadMessage::from).collect())
    }

    /// Load a lead's thread in chronological order (oldest
    /// first). Empty vec when the lead has no messages yet
    /// (placeholder leads created before thread persistence
    /// landed). Tenant-scoped — cross-tenant access impossible.
    pub async fn list_thread(
        &self,
        lead_id: &LeadId,
    ) -> Result<Vec<ThreadMessage>, MarketingError> {
        let rows: Vec<ThreadMessageRow> = sqlx::query_as(
            "SELECT message_id, direction, from_label, body, at_ms, draft_status, subject \
             FROM thread_messages \
             WHERE tenant_id = ? AND lead_id = ? \
             ORDER BY at_ms ASC, message_id ASC",
        )
        .bind(self.tenant_id.as_str())
        .bind(&lead_id.0)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ThreadMessage::from).collect())
    }

    /// Persist a CRM-supplied `Message-Id` so a future inbound
    /// reply (whose `In-Reply-To` echoes this id) can be
    /// resolved back to the originating Lead. Idempotent on
    /// `(tenant_id, rfc_message_id)` — a re-publish of the
    /// same outbound is a no-op.
    pub async fn record_outbound_message_id(
        &self,
        rfc_message_id: &str,
        lead_id: &LeadId,
        thread_id: &str,
        sent_at_ms: i64,
    ) -> Result<(), MarketingError> {
        sqlx::query(
            "INSERT INTO outbound_message_ids \
                 (tenant_id, rfc_message_id, lead_id, thread_id, sent_at_ms) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(tenant_id, rfc_message_id) DO NOTHING",
        )
        .bind(self.tenant_id.as_str())
        .bind(rfc_message_id)
        .bind(&lead_id.0)
        .bind(thread_id)
        .bind(sent_at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Resolve a sender's `In-Reply-To` to the LeadId we
    /// originally sent. Returns `None` when this message id
    /// wasn't one of ours (cold inbound from a stranger).
    /// Strips angle brackets so callers don't have to.
    pub async fn find_lead_by_outbound_message_id(
        &self,
        rfc_message_id: &str,
    ) -> Result<Option<LeadId>, MarketingError> {
        let trimmed = rfc_message_id.trim_matches(|c| c == '<' || c == '>');
        let row = sqlx::query(
            "SELECT lead_id FROM outbound_message_ids \
             WHERE tenant_id = ? AND rfc_message_id = ?",
        )
        .bind(self.tenant_id.as_str())
        .bind(trimmed)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| LeadId(r.try_get("lead_id").unwrap_or_default())))
    }

    /// Every outbound rfc_message_id we sent for a lead, newest
    /// first. Powers per-message engagement views in the lead
    /// drawer (operator queries each id against the tracking
    /// store to see opens / clicks for that specific outbound).
    pub async fn list_outbound_message_ids(
        &self,
        lead_id: &LeadId,
    ) -> Result<Vec<OutboundMessageIdRow>, MarketingError> {
        let rows = sqlx::query(
            "SELECT rfc_message_id, thread_id, sent_at_ms \
             FROM outbound_message_ids \
             WHERE tenant_id = ? AND lead_id = ? \
             ORDER BY sent_at_ms DESC",
        )
        .bind(self.tenant_id.as_str())
        .bind(&lead_id.0)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| OutboundMessageIdRow {
                rfc_message_id: r.try_get("rfc_message_id").unwrap_or_default(),
                thread_id: r.try_get("thread_id").unwrap_or_default(),
                sent_at_ms: r.try_get("sent_at_ms").unwrap_or_default(),
            })
            .collect())
    }

    /// Idempotent-draft dedup lookup — returns the single
    /// pending draft row whose `signature` matches, or `None`
    /// when no such row exists. Backed by the partial index
    /// `idx_thread_messages_pending_sig` (tenant + lead +
    /// signature, gated on direction='draft' AND
    /// draft_status='pending'); the LIMIT 1 + ORDER BY id is a
    /// stable tie-break in case the same lead somehow ended up
    /// with two pending drafts at the same signature (race
    /// before the dedup gate landed) — caller treats either as
    /// "use this one".
    pub async fn find_pending_draft_by_signature(
        &self,
        lead_id: &LeadId,
        signature: &str,
    ) -> Result<Option<ThreadMessage>, MarketingError> {
        let row: Option<ThreadMessageRow> = sqlx::query_as(
            "SELECT message_id, direction, from_label, body, at_ms, draft_status, subject \
             FROM thread_messages \
             WHERE tenant_id = ? AND lead_id = ? \
               AND direction = 'draft' AND draft_status = 'pending' \
               AND signature = ? \
             ORDER BY at_ms DESC, message_id ASC \
             LIMIT 1",
        )
        .bind(self.tenant_id.as_str())
        .bind(&lead_id.0)
        .bind(signature)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ThreadMessage::from))
    }
}

/// Row shape for `list_outbound_message_ids`. Surfaces the
/// minimum the lead-drawer UI needs to enumerate per-message
/// engagement.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct OutboundMessageIdRow {
    pub rfc_message_id: String,
    pub thread_id: String,
    pub sent_at_ms: i64,
}

/// Caller-provided message payload for `append_thread_message`.
/// `message_id` is the dedupe key — stable across delivery
/// retries.
#[derive(Debug, Clone)]
pub struct NewThreadMessage {
    pub message_id: String,
    pub direction: MessageDirection,
    pub from_label: String,
    pub body: String,
    pub at_ms: i64,
    pub draft_status: Option<DraftStatus>,
    /// Operator-supplied subject override. Drafts only.
    /// `None` ⇒ approve handler inherits the lead's
    /// subject (with `Re:` prefix when missing).
    pub subject: Option<String>,
    /// Idempotent-draft signature — sha256 hex of the
    /// `(lead_state, last_inbound_msg_id, seller_id, agent_id,
    /// prompt_hash)` tuple. Populated only on `direction =
    /// Draft` rows; non-draft callers pass `None`. The dedup
    /// gate in `generate_draft_handler` looks up by this
    /// value before deciding to spend tokens on a fresh LLM
    /// call.
    pub signature: Option<String>,
}

/// Wire-equivalent thread message exposed via `/leads/:id/thread`.
/// Mirrors `frontend/src/api/marketing.ts ThreadMessage`. Drop
/// in `nexo-tool-meta::marketing` if this lifts to multiple
/// extensions; today the extension owns the storage.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ThreadMessage {
    pub id: String,
    pub direction: MessageDirection,
    pub from_label: String,
    pub body: String,
    pub at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft_status: Option<DraftStatus>,
    /// Operator-supplied subject override (drafts). `None`
    /// ⇒ inherit lead subject at approve time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum MessageDirection {
    Inbound,
    Outbound,
    Draft,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DraftStatus {
    Pending,
    Approved,
    Rejected,
}

/// Tenant-wide pending draft row with the lead context
/// the operator needs to triage without a follow-up
/// fetch (subject + assigned seller + state). Powers the
/// drafts inbox queue.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PendingDraftRow {
    pub lead_id: String,
    pub message_id: String,
    pub from_label: String,
    pub body: String,
    pub at_ms: i64,
    pub lead_subject: String,
    pub lead_seller_id: String,
    pub lead_person_id: String,
    pub lead_state: String,
}

#[derive(sqlx::FromRow)]
struct PendingDraftRowSql {
    lead_id: String,
    message_id: String,
    from_label: String,
    body: String,
    at_ms: i64,
    lead_subject: String,
    lead_seller_id: String,
    lead_person_id: String,
    lead_state: String,
}

impl PendingDraftRowSql {
    fn into_row(self) -> PendingDraftRow {
        PendingDraftRow {
            lead_id: self.lead_id,
            message_id: self.message_id,
            from_label: self.from_label,
            body: self.body,
            at_ms: self.at_ms,
            lead_subject: self.lead_subject,
            lead_seller_id: self.lead_seller_id,
            lead_person_id: self.lead_person_id,
            lead_state: self.lead_state,
        }
    }
}

fn direction_str(d: MessageDirection) -> &'static str {
    match d {
        MessageDirection::Inbound => "inbound",
        MessageDirection::Outbound => "outbound",
        MessageDirection::Draft => "draft",
    }
}

fn draft_status_str(s: DraftStatus) -> &'static str {
    match s {
        DraftStatus::Pending => "pending",
        DraftStatus::Approved => "approved",
        DraftStatus::Rejected => "rejected",
    }
}

#[derive(Debug, sqlx::FromRow)]
struct ThreadMessageRow {
    message_id: String,
    direction: String,
    from_label: String,
    body: String,
    at_ms: i64,
    draft_status: Option<String>,
    subject: Option<String>,
}

impl From<ThreadMessageRow> for ThreadMessage {
    fn from(r: ThreadMessageRow) -> Self {
        Self {
            id: r.message_id,
            direction: match r.direction.as_str() {
                "inbound" => MessageDirection::Inbound,
                "outbound" => MessageDirection::Outbound,
                _ => MessageDirection::Draft,
            },
            from_label: r.from_label,
            body: r.body,
            at_ms: r.at_ms,
            draft_status: r.draft_status.and_then(|s| match s.as_str() {
                "pending" => Some(DraftStatus::Pending),
                "approved" => Some(DraftStatus::Approved),
                "rejected" => Some(DraftStatus::Rejected),
                _ => None,
            }),
            subject: r.subject,
        }
    }
}

// ── Row mapping ────────────────────────────────────────────────

const SELECT_LEAD: &str =
    "SELECT id, tenant_id, thread_id, subject, person_id, seller_id, \
            state, score, sentiment, intent, topic_tags_json, \
            last_activity_ms, next_check_at_ms, followup_attempts, why_routed_json, \
            operator_notes \
     FROM leads WHERE tenant_id = ? AND id = ?";

#[derive(Debug, sqlx::FromRow)]
struct LeadRow {
    id: String,
    tenant_id: String,
    thread_id: String,
    subject: String,
    person_id: String,
    seller_id: String,
    state: String,
    score: i64,
    sentiment: String,
    intent: String,
    topic_tags_json: String,
    last_activity_ms: i64,
    next_check_at_ms: Option<i64>,
    followup_attempts: i64,
    why_routed_json: String,
    /// M15.21.notes — `None` keeps the column NULL on disk;
    /// `Some("")` round-trips as an empty string so the
    /// editor can clear the field without re-introducing
    /// nulls (a delete UX would be ambiguous otherwise).
    operator_notes: Option<String>,
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
            seller_id: SellerId(self.seller_id),
            state: parse_state(&self.state),
            score: self.score.clamp(0, 100) as u8,
            sentiment: parse_sentiment(&self.sentiment),
            intent: parse_intent(&self.intent),
            topic_tags,
            last_activity_ms: self.last_activity_ms,
            next_check_at_ms: self.next_check_at_ms,
            followup_attempts: self.followup_attempts.clamp(0, 255) as u8,
            why_routed,
            operator_notes: self.operator_notes,
        }
    }
}

/// Convert a `LeadState` to its stable lowercase label.
/// Public so audit producers + tests share the same wire
/// shape as the SQL store without re-stringifying.
pub fn state_str(s: LeadState) -> &'static str {
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

    #[test]
    fn jitter_zero_window_returns_base() {
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        let got = LeadStore::jitter_timestamp_ms(100, 0, &mut rng);
        assert_eq!(got, 100);
    }

    #[test]
    fn jitter_stays_within_window() {
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        for _ in 0..100 {
            let got = LeadStore::jitter_timestamp_ms(1_000_000, 1000, &mut rng);
            assert!((1_000_000 - 500..=1_000_000 + 500).contains(&got),
                "jittered value {got} outside ±500 of base 1_000_000");
        }
    }

    #[test]
    fn jitter_actually_distributes() {
        use rand::SeedableRng;
        // Sanity: 100 jitter applications must produce at least
        // 20 distinct values inside a 1000 ms window. Catches a
        // future regression where the helper accidentally
        // returns the base unchanged.
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100 {
            let v = LeadStore::jitter_timestamp_ms(1_000_000, 1000, &mut rng);
            seen.insert(v);
        }
        assert!(seen.len() > 20, "jitter too clustered: {} distinct values", seen.len());
    }

    fn input(id: &str, person: &str, seller: &str) -> NewLead {
        NewLead {
            id: LeadId(id.into()),
            thread_id: format!("th-{id}"),
            subject: "Re: cotización".into(),
            person_id: PersonId(person.into()),
            seller_id: SellerId(seller.into()),
            last_activity_ms: 1_700_000_000_000,
            score: 0,
            topic_tags: vec![],
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
        // M15.21.notes — fresh leads land with NULL notes.
        assert!(got.operator_notes.is_none());
    }

    /// M15.21.notes — operator can write a markdown note and
    /// read it back; subsequent updates replace verbatim.
    #[tokio::test]
    async fn update_operator_notes_round_trips() {
        let s = fresh_store("acme").await;
        s.create(input("l1", "juan", "pedro")).await.unwrap();
        let updated = s
            .update_operator_notes(
                &LeadId("l1".into()),
                Some("called PA, voicemail".into()),
            )
            .await
            .unwrap();
        assert_eq!(
            updated.operator_notes.as_deref(),
            Some("called PA, voicemail"),
        );
        let read = s.get(&LeadId("l1".into())).await.unwrap().unwrap();
        assert_eq!(
            read.operator_notes.as_deref(),
            Some("called PA, voicemail"),
        );
        // Replace verbatim — no append semantics.
        let after = s
            .update_operator_notes(&LeadId("l1".into()), Some("**WIN**".into()))
            .await
            .unwrap();
        assert_eq!(after.operator_notes.as_deref(), Some("**WIN**"));
    }

    /// M15.21.notes — empty string is legal and round-trips
    /// faithfully so the editor can clear without re-introducing
    /// nulls (a delete UX would be ambiguous otherwise).
    #[tokio::test]
    async fn update_operator_notes_empty_string_persists_as_empty() {
        let s = fresh_store("acme").await;
        s.create(input("l1", "juan", "pedro")).await.unwrap();
        let updated = s
            .update_operator_notes(&LeadId("l1".into()), Some(String::new()))
            .await
            .unwrap();
        assert_eq!(updated.operator_notes.as_deref(), Some(""));
    }

    /// M15.21.notes — `None` writes SQL NULL so reads observe
    /// the "field never authored" sentinel.
    #[tokio::test]
    async fn update_operator_notes_none_clears_to_null() {
        let s = fresh_store("acme").await;
        s.create(input("l1", "juan", "pedro")).await.unwrap();
        // Author a note, then null it.
        s.update_operator_notes(&LeadId("l1".into()), Some("scratch".into()))
            .await
            .unwrap();
        let cleared = s
            .update_operator_notes(&LeadId("l1".into()), None)
            .await
            .unwrap();
        assert!(cleared.operator_notes.is_none());
    }

    /// M15.21.notes — updating a non-existent lead surfaces the
    /// canonical `RowNotFound` error so the admin handler can
    /// translate it to a 404 response.
    #[tokio::test]
    async fn update_operator_notes_missing_lead_errors() {
        let s = fresh_store("acme").await;
        let err = s
            .update_operator_notes(&LeadId("ghost".into()), Some("x".into()))
            .await
            .unwrap_err();
        assert!(matches!(err, MarketingError::Sqlite(sqlx::Error::RowNotFound)));
    }

    /// M15.21.notes — cross-tenant isolation. Tenant B's update
    /// for a lead id colliding with tenant A's lead must NOT
    /// touch tenant A's row. Each `LeadStore` already encodes
    /// the tenant in its file path, so the test asserts the
    /// invariant by-construction.
    #[tokio::test]
    async fn update_operator_notes_is_tenant_scoped() {
        let a = fresh_store("acme").await;
        let b = fresh_store("beta").await;
        a.create(input("l1", "juan", "pedro")).await.unwrap();
        b.create(input("l1", "luis", "ana")).await.unwrap();
        a.update_operator_notes(&LeadId("l1".into()), Some("A note".into()))
            .await
            .unwrap();
        // B's row stays NULL; A's row carries the new value.
        let from_b = b.get(&LeadId("l1".into())).await.unwrap().unwrap();
        assert!(from_b.operator_notes.is_none());
        let from_a = a.get(&LeadId("l1".into())).await.unwrap().unwrap();
        assert_eq!(from_a.operator_notes.as_deref(), Some("A note"));
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

    /// Audit fix #3 — moving a lead to a terminal state
    /// (`lost` reachable directly from `cold`/`engaged`/
    /// `meeting_scheduled`/`qualified`) auto-rejects pending
    /// drafts and clears `next_check_at_ms` so the cron stops
    /// evaluating the closed lead.
    #[tokio::test]
    async fn terminal_transition_auto_rejects_pending_drafts() {
        let s = fresh_store("acme").await;
        s.create(input("l1", "juan", "pedro")).await.unwrap();
        s.set_next_check(&LeadId("l1".into()), Some(1_700_000_000_000), false)
            .await
            .unwrap();
        // Engaged so the lead has both pending drafts and an
        // active followup schedule when the terminal transition
        // fires.
        s.transition(&LeadId("l1".into()), LeadState::Engaged)
            .await
            .unwrap();
        // Seed two pending drafts on the lead.
        s.append_thread_message(
            &LeadId("l1".into()),
            NewThreadMessage {
                message_id: "d1".into(),
                direction: MessageDirection::Draft,
                from_label: "AI".into(),
                body: "draft 1".into(),
                at_ms: 100,
                draft_status: Some(DraftStatus::Pending),
                subject: None,
                signature: Some("sig-1".into()),
            },
        )
        .await
        .unwrap();
        s.append_thread_message(
            &LeadId("l1".into()),
            NewThreadMessage {
                message_id: "d2".into(),
                direction: MessageDirection::Draft,
                from_label: "AI".into(),
                body: "draft 2".into(),
                at_ms: 200,
                draft_status: Some(DraftStatus::Pending),
                subject: None,
                signature: Some("sig-2".into()),
            },
        )
        .await
        .unwrap();
        // Move to lost — terminal transition reachable from
        // engaged. Same auto-reject + clear-next-check logic as
        // qualified (which would require walking through
        // meeting_scheduled first).
        let after = s
            .transition(&LeadId("l1".into()), LeadState::Lost)
            .await
            .unwrap();
        assert_eq!(after.state, LeadState::Lost);
        assert!(
            after.next_check_at_ms.is_none(),
            "terminal transition must clear next_check_at_ms",
        );
        // Pending drafts were rejected en bloc.
        let pending = s
            .list_drafts(&LeadId("l1".into()), Some(DraftStatus::Pending))
            .await
            .unwrap();
        assert!(
            pending.is_empty(),
            "pending drafts must be auto-rejected on terminal transition",
        );
        let rejected = s
            .list_drafts(&LeadId("l1".into()), Some(DraftStatus::Rejected))
            .await
            .unwrap();
        assert_eq!(rejected.len(), 2);
    }

    /// Audit fix #3 — non-terminal transition (cold→engaged)
    /// must NOT touch the pending drafts; the operator may still
    /// want to approve them.
    #[tokio::test]
    async fn nonterminal_transition_keeps_pending_drafts() {
        let s = fresh_store("acme").await;
        s.create(input("l1", "juan", "pedro")).await.unwrap();
        s.append_thread_message(
            &LeadId("l1".into()),
            NewThreadMessage {
                message_id: "d1".into(),
                direction: MessageDirection::Draft,
                from_label: "AI".into(),
                body: "draft".into(),
                at_ms: 100,
                draft_status: Some(DraftStatus::Pending),
                subject: None,
                signature: Some("sig".into()),
            },
        )
        .await
        .unwrap();
        s.transition(&LeadId("l1".into()), LeadState::Engaged)
            .await
            .unwrap();
        let pending = s
            .list_drafts(&LeadId("l1".into()), Some(DraftStatus::Pending))
            .await
            .unwrap();
        assert_eq!(pending.len(), 1, "engaged transition must not auto-reject drafts");
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

    fn nm(id: &str, dir: MessageDirection, body: &str, at: i64) -> NewThreadMessage {
        NewThreadMessage {
            message_id: id.into(),
            direction: dir,
            from_label: "Cliente".into(),
            body: body.into(),
            at_ms: at,
            draft_status: None,
            subject: None,
            signature: None,
        }
    }

    #[tokio::test]
    async fn thread_append_then_list_returns_chronological() {
        let s = fresh_store("acme").await;
        let lead = s.create(input("l-th-1", "p-1", "v-1")).await.unwrap();
        s.append_thread_message(&lead.id, nm("m1", MessageDirection::Inbound, "first", 100))
            .await
            .unwrap();
        s.append_thread_message(&lead.id, nm("m3", MessageDirection::Inbound, "third", 300))
            .await
            .unwrap();
        s.append_thread_message(&lead.id, nm("m2", MessageDirection::Outbound, "second", 200))
            .await
            .unwrap();
        let thread = s.list_thread(&lead.id).await.unwrap();
        let bodies: Vec<&str> = thread.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(bodies, vec!["first", "second", "third"]);
        assert_eq!(thread[1].direction, MessageDirection::Outbound);
    }

    #[tokio::test]
    async fn thread_append_idempotent_on_message_id() {
        let s = fresh_store("acme").await;
        let lead = s.create(input("l-th-2", "p-1", "v-1")).await.unwrap();
        let m = nm("dup", MessageDirection::Inbound, "hi", 1);
        s.append_thread_message(&lead.id, m.clone()).await.unwrap();
        s.append_thread_message(&lead.id, m).await.unwrap();
        let thread = s.list_thread(&lead.id).await.unwrap();
        assert_eq!(thread.len(), 1);
    }

    #[tokio::test]
    async fn thread_list_empty_for_new_lead() {
        let s = fresh_store("acme").await;
        let lead = s.create(input("l-th-3", "p-1", "v-1")).await.unwrap();
        let thread = s.list_thread(&lead.id).await.unwrap();
        assert!(thread.is_empty());
    }

    #[tokio::test]
    async fn draft_status_round_trips() {
        let s = fresh_store("acme").await;
        let lead = s.create(input("l-th-4", "p-1", "v-1")).await.unwrap();
        s.append_thread_message(
            &lead.id,
            NewThreadMessage {
                message_id: "draft-1".into(),
                direction: MessageDirection::Draft,
                from_label: "AI".into(),
                body: "draft body".into(),
                at_ms: 5,
                draft_status: Some(DraftStatus::Pending), subject: None,
                signature: None,
            },
        )
        .await
        .unwrap();
        let thread = s.list_thread(&lead.id).await.unwrap();
        assert_eq!(thread[0].direction, MessageDirection::Draft);
        assert_eq!(thread[0].draft_status, Some(DraftStatus::Pending));
    }

    // ─── M15.21 slice 1 — draft mutation methods ─────────────

    async fn store_with_draft(
        body: &str,
        status: DraftStatus,
    ) -> (LeadStore, LeadId, String) {
        let s = fresh_store("acme").await;
        let lead = s.create(input("l-d", "p-1", "v-1")).await.unwrap();
        let msg_id = "draft-1".to_string();
        s.append_thread_message(
            &lead.id,
            NewThreadMessage {
                message_id: msg_id.clone(),
                direction: MessageDirection::Draft,
                from_label: "AI".into(),
                body: body.into(),
                at_ms: 1,
                draft_status: Some(status), subject: None,
                signature: None,
            },
        )
        .await
        .unwrap();
        (s, lead.id.clone(), msg_id)
    }

    #[tokio::test]
    async fn update_draft_body_round_trips() {
        let (s, lead, msg) = store_with_draft("v1", DraftStatus::Pending).await;
        let n = s.update_draft_body(&lead, &msg, "v2").await.unwrap();
        assert_eq!(n, 1);
        let drafts = s.list_drafts(&lead, None).await.unwrap();
        assert_eq!(drafts[0].body, "v2");
    }

    #[tokio::test]
    async fn update_draft_body_refuses_when_already_approved() {
        let (s, lead, msg) =
            store_with_draft("locked", DraftStatus::Approved).await;
        let n = s.update_draft_body(&lead, &msg, "must not stick")
            .await
            .unwrap();
        assert_eq!(n, 0, "approved drafts are immutable");
        let drafts = s.list_drafts(&lead, None).await.unwrap();
        assert_eq!(drafts[0].body, "locked");
    }

    #[tokio::test]
    async fn update_draft_body_misses_unknown_message_id() {
        let (s, lead, _) =
            store_with_draft("v1", DraftStatus::Pending).await;
        let n = s.update_draft_body(&lead, "ghost-id", "x").await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn set_draft_status_pending_to_approved() {
        let (s, lead, msg) =
            store_with_draft("ok", DraftStatus::Pending).await;
        let n = s
            .set_draft_status(&lead, &msg, DraftStatus::Approved)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let drafts = s.list_drafts(&lead, None).await.unwrap();
        assert_eq!(drafts[0].draft_status, Some(DraftStatus::Approved));
    }

    #[tokio::test]
    async fn set_draft_status_pending_to_rejected() {
        let (s, lead, msg) =
            store_with_draft("ok", DraftStatus::Pending).await;
        let n = s
            .set_draft_status(&lead, &msg, DraftStatus::Rejected)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let drafts = s.list_drafts(&lead, None).await.unwrap();
        assert_eq!(drafts[0].draft_status, Some(DraftStatus::Rejected));
    }

    #[tokio::test]
    async fn set_draft_status_refuses_double_approve() {
        let (s, lead, msg) =
            store_with_draft("ok", DraftStatus::Pending).await;
        let _ = s
            .set_draft_status(&lead, &msg, DraftStatus::Approved)
            .await
            .unwrap();
        let n = s
            .set_draft_status(&lead, &msg, DraftStatus::Approved)
            .await
            .unwrap();
        assert_eq!(n, 0, "double-approve must NOT fire the publisher twice");
    }

    #[tokio::test]
    async fn delete_draft_removes_pending_row() {
        let (s, lead, msg) =
            store_with_draft("ok", DraftStatus::Pending).await;
        let n = s.delete_draft(&lead, &msg).await.unwrap();
        assert_eq!(n, 1);
        let drafts = s.list_drafts(&lead, None).await.unwrap();
        assert!(drafts.is_empty());
    }

    #[tokio::test]
    async fn delete_draft_does_not_touch_outbound_rows() {
        // Defense in depth: caller bug passing an outbound
        // message_id must NOT delete the outbound row.
        let s = fresh_store("acme").await;
        let lead = s.create(input("l-d", "p-1", "v-1")).await.unwrap();
        s.append_thread_message(
            &lead.id,
            NewThreadMessage {
                message_id: "out-1".into(),
                direction: MessageDirection::Outbound,
                from_label: "Pedro".into(),
                body: "real reply".into(),
                at_ms: 1,
                draft_status: None, subject: None,
                signature: None,
            },
        )
        .await
        .unwrap();
        let n = s.delete_draft(&lead.id, "out-1").await.unwrap();
        assert_eq!(n, 0, "outbound row must survive");
        let thread = s.list_thread(&lead.id).await.unwrap();
        assert_eq!(thread.len(), 1);
    }

    #[tokio::test]
    async fn list_drafts_filters_by_status() {
        let s = fresh_store("acme").await;
        let lead = s.create(input("l-d", "p-1", "v-1")).await.unwrap();
        for (i, status) in
            [(1, DraftStatus::Pending), (2, DraftStatus::Approved)].iter()
        {
            s.append_thread_message(
                &lead.id,
                NewThreadMessage {
                    message_id: format!("draft-{i}"),
                    direction: MessageDirection::Draft,
                    from_label: "AI".into(),
                    body: format!("body {i}"),
                    at_ms: *i as i64,
                    draft_status: Some(*status),
                    subject: None,
                    signature: None,
                },
            )
            .await
            .unwrap();
        }
        let pending =
            s.list_drafts(&lead.id, Some(DraftStatus::Pending)).await.unwrap();
        assert_eq!(pending.len(), 1);
        let approved = s
            .list_drafts(&lead.id, Some(DraftStatus::Approved))
            .await
            .unwrap();
        assert_eq!(approved.len(), 1);
        let all = s.list_drafts(&lead.id, None).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn list_drafts_excludes_inbound_and_outbound() {
        let s = fresh_store("acme").await;
        let lead = s.create(input("l-d", "p-1", "v-1")).await.unwrap();
        for (id, dir, status) in [
            ("in-1", MessageDirection::Inbound, None),
            ("draft-1", MessageDirection::Draft, Some(DraftStatus::Pending)),
            ("out-1", MessageDirection::Outbound, None),
        ] {
            s.append_thread_message(
                &lead.id,
                NewThreadMessage {
                    message_id: id.into(),
                    direction: dir,
                    from_label: "x".into(),
                    body: "x".into(),
                    at_ms: 1,
                    draft_status: status, subject: None,
                    signature: None,
                },
            )
            .await
            .unwrap();
        }
        let drafts = s.list_drafts(&lead.id, None).await.unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].id, "draft-1");
    }

    #[tokio::test]
    async fn draft_methods_tenant_scoped() {
        // Same lead id + same draft id under DIFFERENT
        // tenants — operations on tenant A must NOT touch
        // tenant B's row.
        let acme = fresh_store("acme").await;
        let acme_lead = acme.create(input("l-d", "p-1", "v-1")).await.unwrap();
        acme.append_thread_message(
            &acme_lead.id,
            NewThreadMessage {
                message_id: "draft-1".into(),
                direction: MessageDirection::Draft,
                from_label: "AI".into(),
                body: "acme body".into(),
                at_ms: 1,
                draft_status: Some(DraftStatus::Pending), subject: None,
                signature: None,
            },
        )
        .await
        .unwrap();
        let globex = fresh_store("globex").await;
        let globex_lead =
            globex.create(input("l-d", "p-1", "v-1")).await.unwrap();
        globex
            .append_thread_message(
                &globex_lead.id,
                NewThreadMessage {
                    message_id: "draft-1".into(),
                    direction: MessageDirection::Draft,
                    from_label: "AI".into(),
                    body: "globex body".into(),
                    at_ms: 1,
                    draft_status: Some(DraftStatus::Pending), subject: None,
                    signature: None,
                },
            )
            .await
            .unwrap();
        // Update under acme.
        let _ = acme
            .update_draft_body(&acme_lead.id, "draft-1", "acme v2")
            .await
            .unwrap();
        // Globex still pristine.
        let g = globex.list_drafts(&globex_lead.id, None).await.unwrap();
        assert_eq!(g[0].body, "globex body");
    }

    // ─── M15.24 — telemetry helpers ──────────────────────────

    #[tokio::test]
    async fn count_drafts_pending_only_pending_rows() {
        let s = fresh_store("acme").await;
        let lead = s.create(input("l-1", "p-1", "v-1")).await.unwrap();
        s.append_thread_message(
            &lead.id,
            NewThreadMessage {
                message_id: "d-1".into(),
                direction: MessageDirection::Draft,
                from_label: "AI".into(),
                body: "p1".into(),
                at_ms: 1,
                draft_status: Some(DraftStatus::Pending), subject: None,
                signature: None,
            },
        )
        .await
        .unwrap();
        s.append_thread_message(
            &lead.id,
            NewThreadMessage {
                message_id: "d-2".into(),
                direction: MessageDirection::Draft,
                from_label: "AI".into(),
                body: "p2".into(),
                at_ms: 2,
                draft_status: Some(DraftStatus::Pending), subject: None,
                signature: None,
            },
        )
        .await
        .unwrap();
        // One approved → does NOT count.
        s.append_thread_message(
            &lead.id,
            NewThreadMessage {
                message_id: "d-3".into(),
                direction: MessageDirection::Draft,
                from_label: "AI".into(),
                body: "p3".into(),
                at_ms: 3,
                draft_status: Some(DraftStatus::Approved), subject: None,
                signature: None,
            },
        )
        .await
        .unwrap();
        // Inbound message — also doesn't count.
        s.append_thread_message(
            &lead.id,
            NewThreadMessage {
                message_id: "in-1".into(),
                direction: MessageDirection::Inbound,
                from_label: "Cliente".into(),
                body: "hi".into(),
                at_ms: 4,
                draft_status: None, subject: None,
                signature: None,
            },
        )
        .await
        .unwrap();
        let n = s.count_drafts_pending().await.unwrap();
        assert_eq!(n, 2);
    }

    #[tokio::test]
    async fn count_drafts_pending_is_tenant_scoped() {
        let acme = fresh_store("acme").await;
        let globex = fresh_store("globex").await;
        let l1 = acme.create(input("l-1", "p-1", "v-1")).await.unwrap();
        let l2 = globex.create(input("l-1", "p-1", "v-1")).await.unwrap();
        for store in [&acme, &globex] {
            let lead = if store.tenant_id().as_str() == "acme" {
                &l1
            } else {
                &l2
            };
            store
                .append_thread_message(
                    &lead.id,
                    NewThreadMessage {
                        message_id: "d-1".into(),
                        direction: MessageDirection::Draft,
                        from_label: "AI".into(),
                        body: "x".into(),
                        at_ms: 1,
                        draft_status: Some(DraftStatus::Pending), subject: None,
                        signature: None,
                    },
                )
                .await
                .unwrap();
        }
        assert_eq!(acme.count_drafts_pending().await.unwrap(), 1);
        assert_eq!(globex.count_drafts_pending().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn list_pending_drafts_tenant_wide_joins_lead_context() {
        let s = fresh_store("acme").await;
        let l1 = s.create(input("l-1", "p-1", "v-1")).await.unwrap();
        let l2 = s.create(input("l-2", "p-2", "v-2")).await.unwrap();
        // Pending on l1 + pending on l2 + approved on l1 (excluded).
        s.append_thread_message(
            &l1.id,
            NewThreadMessage {
                message_id: "d-1".into(),
                direction: MessageDirection::Draft,
                from_label: "AI".into(),
                body: "older".into(),
                at_ms: 100,
                draft_status: Some(DraftStatus::Pending), subject: None,
                signature: None,
            },
        )
        .await
        .unwrap();
        s.append_thread_message(
            &l2.id,
            NewThreadMessage {
                message_id: "d-2".into(),
                direction: MessageDirection::Draft,
                from_label: "AI".into(),
                body: "newer".into(),
                at_ms: 300,
                draft_status: Some(DraftStatus::Pending), subject: None,
                signature: None,
            },
        )
        .await
        .unwrap();
        s.append_thread_message(
            &l1.id,
            NewThreadMessage {
                message_id: "d-3".into(),
                direction: MessageDirection::Draft,
                from_label: "AI".into(),
                body: "approved".into(),
                at_ms: 200,
                draft_status: Some(DraftStatus::Approved), subject: None,
                signature: None,
            },
        )
        .await
        .unwrap();
        let rows = s.list_pending_drafts_tenant_wide(50).await.unwrap();
        assert_eq!(rows.len(), 2);
        // Newest first.
        assert_eq!(rows[0].body, "newer");
        assert_eq!(rows[0].lead_id, "l-2");
        assert_eq!(rows[0].lead_seller_id, "v-2");
        assert_eq!(rows[1].body, "older");
        assert_eq!(rows[1].lead_subject, "Re: cotización");
    }

    #[tokio::test]
    async fn list_pending_drafts_tenant_wide_respects_limit() {
        let s = fresh_store("acme").await;
        let l = s.create(input("l-1", "p-1", "v-1")).await.unwrap();
        for i in 0..5 {
            s.append_thread_message(
                &l.id,
                NewThreadMessage {
                    message_id: format!("d-{i}"),
                    direction: MessageDirection::Draft,
                    from_label: "AI".into(),
                    body: format!("body-{i}"),
                    at_ms: i as i64 + 1,
                    draft_status: Some(DraftStatus::Pending),
                    subject: None,
                    signature: None,
                },
            )
            .await
            .unwrap();
        }
        let rows = s.list_pending_drafts_tenant_wide(2).await.unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn list_pending_drafts_tenant_wide_is_tenant_scoped() {
        let acme = fresh_store("acme").await;
        let globex = fresh_store("globex").await;
        let la = acme.create(input("l-1", "p", "v")).await.unwrap();
        let lg = globex.create(input("l-1", "p", "v")).await.unwrap();
        for (s, l, body) in [
            (&acme, &la, "acme draft"),
            (&globex, &lg, "globex draft"),
        ] {
            s.append_thread_message(
                &l.id,
                NewThreadMessage {
                    message_id: "d".into(),
                    direction: MessageDirection::Draft,
                    from_label: "AI".into(),
                    body: body.into(),
                    at_ms: 1,
                    draft_status: Some(DraftStatus::Pending), subject: None,
                    signature: None,
                },
            )
            .await
            .unwrap();
        }
        let acme_rows = acme.list_pending_drafts_tenant_wide(50).await.unwrap();
        assert_eq!(acme_rows.len(), 1);
        assert_eq!(acme_rows[0].body, "acme draft");
        let globex_rows = globex
            .list_pending_drafts_tenant_wide(50)
            .await
            .unwrap();
        assert_eq!(globex_rows.len(), 1);
        assert_eq!(globex_rows[0].body, "globex draft");
    }

    #[tokio::test]
    async fn count_messages_by_direction_window_filters_inclusive() {
        let s = fresh_store("acme").await;
        let lead = s.create(input("l-1", "p-1", "v-1")).await.unwrap();
        for (id, dir, at_ms) in [
            ("in-old", MessageDirection::Inbound, 100_i64),
            ("in-new", MessageDirection::Inbound, 200),
            ("out-mid", MessageDirection::Outbound, 150),
        ] {
            s.append_thread_message(
                &lead.id,
                NewThreadMessage {
                    message_id: id.into(),
                    direction: dir,
                    from_label: "x".into(),
                    body: "x".into(),
                    at_ms,
                    draft_status: None, subject: None,
                    signature: None,
                },
            )
            .await
            .unwrap();
        }
        // Window since=150 ⇒ inbound counts only `in-new` (200);
        // boundary `at_ms == since_ms` is inclusive (>=).
        let inbound = s
            .count_messages_by_direction_since(
                MessageDirection::Inbound,
                150,
            )
            .await
            .unwrap();
        assert_eq!(inbound, 1);
        let outbound = s
            .count_messages_by_direction_since(
                MessageDirection::Outbound,
                150,
            )
            .await
            .unwrap();
        assert_eq!(outbound, 1);
        // Window since=0 ⇒ everything in the corresponding
        // direction.
        let all_inbound = s
            .count_messages_by_direction_since(
                MessageDirection::Inbound,
                0,
            )
            .await
            .unwrap();
        assert_eq!(all_inbound, 2);
    }
}
