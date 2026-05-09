//! SQLite store for the per-tenant spam filter config + rule
//! lists. Lives on the marketing extension's identity pool to
//! keep one SQLite file per tenant (same model the enrichment
//! cache uses).

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

use super::defaults::{Strictness, ThresholdSet, BALANCED_THRESHOLDS};

/// One row in `spam_filter_rules`. `value` semantics depend on
/// `kind`:
/// - `DomainBlock` / `DomainAllow`: lowercase domain (no `@`)
/// - `KeywordBlock` / `KeywordAllow`: lowercase substring match
///   against subject + visible body
/// - `SenderBlock` / `SenderAllow`: lowercase full email
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpamFilterRule {
    pub id: i64,
    pub tenant_id: String,
    pub kind: RuleKind,
    pub value: String,
    pub note: Option<String>,
    pub enabled: bool,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    DomainBlock,
    DomainAllow,
    KeywordBlock,
    KeywordAllow,
    SenderBlock,
    SenderAllow,
}

impl RuleKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DomainBlock => "domain_block",
            Self::DomainAllow => "domain_allow",
            Self::KeywordBlock => "keyword_block",
            Self::KeywordAllow => "keyword_allow",
            Self::SenderBlock => "sender_block",
            Self::SenderAllow => "sender_allow",
        }
    }

    pub fn from_db(s: &str) -> Option<Self> {
        match s {
            "domain_block" => Some(Self::DomainBlock),
            "domain_allow" => Some(Self::DomainAllow),
            "keyword_block" => Some(Self::KeywordBlock),
            "keyword_allow" => Some(Self::KeywordAllow),
            "sender_block" => Some(Self::SenderBlock),
            "sender_allow" => Some(Self::SenderAllow),
            _ => None,
        }
    }

    pub fn is_block(&self) -> bool {
        matches!(
            self,
            Self::DomainBlock | Self::KeywordBlock | Self::SenderBlock
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpamFilterConfig {
    pub tenant_id: String,
    pub strictness: Strictness,
    /// Thresholds saved when `strictness == Custom`; for the
    /// fixed presets the row keeps the last-edited custom
    /// values around so a user toggling Custom → Balanced →
    /// Custom doesn't lose their tuning.
    pub thresholds: ThresholdSet,
    pub updated_at_ms: i64,
}

impl SpamFilterConfig {
    pub fn defaults_for(tenant_id: &str, now_ms: i64) -> Self {
        Self {
            tenant_id: tenant_id.to_string(),
            strictness: Strictness::default(),
            thresholds: BALANCED_THRESHOLDS.clone(),
            updated_at_ms: now_ms,
        }
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] sqlx::Error),
    #[error("invalid rule kind: {0:?}")]
    InvalidKind(String),
    #[error("invalid strictness: {0:?}")]
    InvalidStrictness(String),
    #[error("rule value cannot be empty")]
    EmptyValue,
    #[error("rule value too long (max {max} chars, got {got})")]
    ValueTooLong { max: usize, got: usize },
}

const MAX_VALUE_LEN: usize = 256;

const MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS spam_filter_config (
    tenant_id                  TEXT PRIMARY KEY,
    strictness                 TEXT NOT NULL DEFAULT 'balanced',
    image_only_drop            INTEGER NOT NULL DEFAULT 1,
    image_heavy_drop           INTEGER NOT NULL DEFAULT 1,
    image_heavy_min_count      INTEGER NOT NULL DEFAULT 3,
    image_heavy_max_text_chars INTEGER NOT NULL DEFAULT 200,
    role_keyword_drop          INTEGER NOT NULL DEFAULT 1,
    multi_weak_drop            INTEGER NOT NULL DEFAULT 1,
    multi_weak_threshold       INTEGER NOT NULL DEFAULT 2,
    updated_at_ms              INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS spam_filter_rules (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant_id     TEXT NOT NULL,
    rule_kind     TEXT NOT NULL,
    value         TEXT NOT NULL,
    note          TEXT,
    enabled       INTEGER NOT NULL DEFAULT 1,
    created_at_ms INTEGER NOT NULL,
    UNIQUE (tenant_id, rule_kind, value)
);
CREATE INDEX IF NOT EXISTS idx_spam_filter_rules_tenant
    ON spam_filter_rules(tenant_id, rule_kind);
"#;

/// Run the migration. Idempotent — safe on every boot.
pub async fn migrate(pool: &SqlitePool) -> Result<(), StoreError> {
    sqlx::query(MIGRATION_SQL).execute(pool).await?;
    Ok(())
}

#[derive(Clone)]
pub struct SpamFilterStore {
    pool: SqlitePool,
}

impl SpamFilterStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Read the per-tenant config. Returns the default config
    /// (Balanced, no custom thresholds) when the row doesn't
    /// exist yet — first-write happens on the first PUT.
    pub async fn get_config(
        &self,
        tenant_id: &str,
        now_ms: i64,
    ) -> Result<SpamFilterConfig, StoreError> {
        let row = sqlx::query(
            r#"SELECT strictness, image_only_drop, image_heavy_drop,
                      image_heavy_min_count, image_heavy_max_text_chars,
                      role_keyword_drop, multi_weak_drop, multi_weak_threshold,
                      updated_at_ms
               FROM spam_filter_config WHERE tenant_id = ?"#,
        )
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(SpamFilterConfig::defaults_for(tenant_id, now_ms));
        };
        let strictness_s: String = row.try_get("strictness")?;
        let strictness = Strictness::from_db(&strictness_s)
            .ok_or_else(|| StoreError::InvalidStrictness(strictness_s.clone()))?;
        let thresholds = ThresholdSet {
            image_only_drop: row.try_get::<i64, _>("image_only_drop")? != 0,
            image_heavy_drop: row.try_get::<i64, _>("image_heavy_drop")? != 0,
            image_heavy_min_count: row.try_get::<i64, _>("image_heavy_min_count")? as u32,
            image_heavy_max_text_chars: row
                .try_get::<i64, _>("image_heavy_max_text_chars")? as u32,
            role_keyword_drop: row.try_get::<i64, _>("role_keyword_drop")? != 0,
            multi_weak_drop: row.try_get::<i64, _>("multi_weak_drop")? != 0,
            multi_weak_threshold: row.try_get::<i64, _>("multi_weak_threshold")? as u32,
        };
        Ok(SpamFilterConfig {
            tenant_id: tenant_id.to_string(),
            strictness,
            thresholds,
            updated_at_ms: row.try_get("updated_at_ms")?,
        })
    }

    /// Upsert the per-tenant config. The custom thresholds are
    /// always persisted alongside the strictness so a switch
    /// `Balanced → Custom` keeps the previously saved values.
    pub async fn put_config(
        &self,
        cfg: &SpamFilterConfig,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"INSERT INTO spam_filter_config
                 (tenant_id, strictness, image_only_drop, image_heavy_drop,
                  image_heavy_min_count, image_heavy_max_text_chars,
                  role_keyword_drop, multi_weak_drop, multi_weak_threshold,
                  updated_at_ms)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
               ON CONFLICT(tenant_id) DO UPDATE SET
                  strictness = excluded.strictness,
                  image_only_drop = excluded.image_only_drop,
                  image_heavy_drop = excluded.image_heavy_drop,
                  image_heavy_min_count = excluded.image_heavy_min_count,
                  image_heavy_max_text_chars = excluded.image_heavy_max_text_chars,
                  role_keyword_drop = excluded.role_keyword_drop,
                  multi_weak_drop = excluded.multi_weak_drop,
                  multi_weak_threshold = excluded.multi_weak_threshold,
                  updated_at_ms = excluded.updated_at_ms"#,
        )
        .bind(&cfg.tenant_id)
        .bind(cfg.strictness.as_str())
        .bind(cfg.thresholds.image_only_drop as i64)
        .bind(cfg.thresholds.image_heavy_drop as i64)
        .bind(cfg.thresholds.image_heavy_min_count as i64)
        .bind(cfg.thresholds.image_heavy_max_text_chars as i64)
        .bind(cfg.thresholds.role_keyword_drop as i64)
        .bind(cfg.thresholds.multi_weak_drop as i64)
        .bind(cfg.thresholds.multi_weak_threshold as i64)
        .bind(cfg.updated_at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_rules(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<SpamFilterRule>, StoreError> {
        let rows = sqlx::query(
            r#"SELECT id, rule_kind, value, note, enabled, created_at_ms
               FROM spam_filter_rules
               WHERE tenant_id = ?
               ORDER BY rule_kind ASC, created_at_ms DESC"#,
        )
        .bind(tenant_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let kind_s: String = row.try_get("rule_kind")?;
            let kind = RuleKind::from_db(&kind_s)
                .ok_or_else(|| StoreError::InvalidKind(kind_s.clone()))?;
            out.push(SpamFilterRule {
                id: row.try_get("id")?,
                tenant_id: tenant_id.to_string(),
                kind,
                value: row.try_get("value")?,
                note: row.try_get("note")?,
                enabled: row.try_get::<i64, _>("enabled")? != 0,
                created_at_ms: row.try_get("created_at_ms")?,
            });
        }
        Ok(out)
    }

    /// Add a rule. Trims + lowercases the value (rules are
    /// matched lowercase). `ON CONFLICT IGNORE` so duplicate
    /// adds are no-ops.
    pub async fn add_rule(
        &self,
        tenant_id: &str,
        kind: RuleKind,
        value: &str,
        note: Option<&str>,
        now_ms: i64,
    ) -> Result<SpamFilterRule, StoreError> {
        let value = normalize_rule_value(kind, value)?;
        sqlx::query(
            r#"INSERT INTO spam_filter_rules
                 (tenant_id, rule_kind, value, note, enabled, created_at_ms)
               VALUES (?, ?, ?, ?, 1, ?)
               ON CONFLICT(tenant_id, rule_kind, value) DO NOTHING"#,
        )
        .bind(tenant_id)
        .bind(kind.as_str())
        .bind(&value)
        .bind(note)
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        // SELECT back the row (whether we inserted or not) so the
        // caller has the canonical state including id.
        let row = sqlx::query(
            r#"SELECT id, note, enabled, created_at_ms
               FROM spam_filter_rules
               WHERE tenant_id = ? AND rule_kind = ? AND value = ?"#,
        )
        .bind(tenant_id)
        .bind(kind.as_str())
        .bind(&value)
        .fetch_one(&self.pool)
        .await?;
        Ok(SpamFilterRule {
            id: row.try_get("id")?,
            tenant_id: tenant_id.to_string(),
            kind,
            value,
            note: row.try_get("note")?,
            enabled: row.try_get::<i64, _>("enabled")? != 0,
            created_at_ms: row.try_get("created_at_ms")?,
        })
    }

    pub async fn delete_rule(
        &self,
        tenant_id: &str,
        rule_id: i64,
    ) -> Result<bool, StoreError> {
        let res = sqlx::query(
            r#"DELETE FROM spam_filter_rules
               WHERE tenant_id = ? AND id = ?"#,
        )
        .bind(tenant_id)
        .bind(rule_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }
}

/// Trim + lowercase + sanity-check rule values per kind.
/// Domain rules also strip a leading `@` so users can paste
/// either `@bigcorp.com` or `bigcorp.com`.
pub(crate) fn normalize_rule_value(kind: RuleKind, raw: &str) -> Result<String, StoreError> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Err(StoreError::EmptyValue);
    }
    if trimmed.chars().count() > MAX_VALUE_LEN {
        return Err(StoreError::ValueTooLong {
            max: MAX_VALUE_LEN,
            got: trimmed.chars().count(),
        });
    }
    let v = match kind {
        RuleKind::DomainBlock | RuleKind::DomainAllow => {
            trimmed.trim_start_matches('@').to_string()
        }
        _ => trimmed,
    };
    Ok(v)
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
    async fn defaults_returned_when_no_row() {
        let s = SpamFilterStore::new(pool().await);
        let cfg = s.get_config("t1", 1_000).await.unwrap();
        assert_eq!(cfg.strictness, Strictness::Balanced);
        assert_eq!(cfg.thresholds, BALANCED_THRESHOLDS);
        assert_eq!(cfg.updated_at_ms, 1_000);
    }

    #[tokio::test]
    async fn put_then_get_round_trip() {
        let s = SpamFilterStore::new(pool().await);
        let mut cfg = SpamFilterConfig::defaults_for("t1", 1_000);
        cfg.strictness = Strictness::Strict;
        cfg.thresholds.multi_weak_threshold = 4;
        s.put_config(&cfg).await.unwrap();
        let back = s.get_config("t1", 9_999).await.unwrap();
        assert_eq!(back.strictness, Strictness::Strict);
        assert_eq!(back.thresholds.multi_weak_threshold, 4);
    }

    #[tokio::test]
    async fn add_rule_idempotent() {
        let s = SpamFilterStore::new(pool().await);
        let r1 = s
            .add_rule("t1", RuleKind::DomainBlock, "@Spam.com", None, 1)
            .await
            .unwrap();
        let r2 = s
            .add_rule("t1", RuleKind::DomainBlock, "spam.com", Some("dup"), 2)
            .await
            .unwrap();
        // Same canonical value → same row id, original note kept.
        assert_eq!(r1.id, r2.id);
        assert_eq!(r2.value, "spam.com");
        assert!(r2.note.is_none());
    }

    #[tokio::test]
    async fn list_rules_sorted_by_kind() {
        let s = SpamFilterStore::new(pool().await);
        s.add_rule("t1", RuleKind::KeywordBlock, "promo", None, 1)
            .await
            .unwrap();
        s.add_rule("t1", RuleKind::DomainBlock, "spam.com", None, 2)
            .await
            .unwrap();
        s.add_rule("t1", RuleKind::SenderAllow, "vip@x.com", None, 3)
            .await
            .unwrap();
        let rs = s.list_rules("t1").await.unwrap();
        assert_eq!(rs.len(), 3);
        // ORDER BY rule_kind ASC ⇒ domain_block, keyword_block, sender_allow
        // alphabetically on the DB string form.
        assert_eq!(rs[0].kind, RuleKind::DomainBlock);
        assert_eq!(rs[1].kind, RuleKind::KeywordBlock);
        assert_eq!(rs[2].kind, RuleKind::SenderAllow);
    }

    #[tokio::test]
    async fn delete_rule_removes_row() {
        let s = SpamFilterStore::new(pool().await);
        let r = s
            .add_rule("t1", RuleKind::KeywordBlock, "spam", None, 1)
            .await
            .unwrap();
        let removed = s.delete_rule("t1", r.id).await.unwrap();
        assert!(removed);
        let rs = s.list_rules("t1").await.unwrap();
        assert!(rs.is_empty());
    }

    #[tokio::test]
    async fn delete_rule_other_tenant_is_no_op() {
        let s = SpamFilterStore::new(pool().await);
        let r = s
            .add_rule("t1", RuleKind::KeywordBlock, "spam", None, 1)
            .await
            .unwrap();
        let removed = s.delete_rule("t2", r.id).await.unwrap();
        assert!(!removed);
    }

    #[test]
    fn normalize_strips_at_for_domain_rules() {
        let v = normalize_rule_value(RuleKind::DomainBlock, " @Foo.com ").unwrap();
        assert_eq!(v, "foo.com");
    }

    #[test]
    fn normalize_keeps_at_for_sender_rules() {
        let v = normalize_rule_value(RuleKind::SenderBlock, " Alice@Foo.com ").unwrap();
        assert_eq!(v, "alice@foo.com");
    }

    #[test]
    fn normalize_rejects_empty() {
        assert!(matches!(
            normalize_rule_value(RuleKind::KeywordBlock, "   "),
            Err(StoreError::EmptyValue)
        ));
    }
}

